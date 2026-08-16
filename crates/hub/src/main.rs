use std::{
    io::stderr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use arc_swap::ArcSwap;
use clap::Parser;
use haproxy_spoa_hub::{
    config, config_watcher, crash_handler, listener, metrics, plugin_loader, registry, validate,
};
use tokio::{runtime, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(
    name = "haproxy-spoa-hub",
    version,
    about = "HAProxy SPOE agent plugin hub"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "/etc/haproxy-spoa-hub/config.toml")]
    config: PathBuf,

    /// Validate configuration and exit.
    #[arg(long)]
    check: bool,

    /// Run in validator-sidecar mode: bind a unix socket at this
    /// path, accept length-prefixed JSON validation requests, and
    /// dispatch them to plugins' `validate()` method without calling
    /// `init()`. Suppresses the SPOE listener — the hub does not
    /// serve traffic in this mode. See specs/004-validate-mode/.
    #[arg(long, value_name = "PATH")]
    validate_socket: Option<PathBuf>,
}

fn init_logging(log_level: &str) {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level)))
        .with(
            fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_target(true)
                .with_writer(stderr),
        )
        .init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Install the fatal-signal backtrace handler BEFORE any plugin
    // loading or runtime startup. Plugins are dlopen'd shared libraries
    // — the most likely SIGSEGV source — so the handler needs to be
    // armed before the first dlopen call. Failure to install is
    // non-fatal: log to stderr (logging isn't initialised yet) and
    // continue, so a missing libc::backtrace doesn't block startup.
    if let Err(e) = crash_handler::install() {
        eprintln!("warning: failed to install fatal-signal backtrace handler: {e}");
    }

    // Load and validate configuration
    let hub_config = config::HubConfig::load(&cli.config)?;

    // Build tokio runtime with configurable worker threads.
    // thread_keep_alive is configurable to avoid cold-start thread creation
    // overhead (~10-50µs) when blocking pool threads are reaped between
    // traffic bursts. Default: 300s (tokio default is 10s).
    let mut rt_builder = runtime::Builder::new_multi_thread();
    rt_builder
        .enable_all()
        .max_blocking_threads(config::resolve_max_blocking_threads(&hub_config))
        .thread_keep_alive(std::time::Duration::from_secs(
            hub_config.blocking_thread_keep_alive_secs,
        ));
    if let Some(threads) = hub_config.worker_threads {
        rt_builder.worker_threads(threads);
    }
    let rt = rt_builder.build()?;

    rt.block_on(async_main(cli, hub_config))
}

async fn async_main(cli: Cli, hub_config: config::HubConfig) -> anyhow::Result<()> {
    // Initialize logging
    init_logging(&hub_config.log_level);
    tracing::info!(config = %cli.config.display(), "configuration loaded");

    if let Some(threads) = hub_config.worker_threads {
        tracing::info!(worker_threads = threads, "custom worker thread count");
    }
    tracing::info!(
        max_blocking_threads = config::resolve_max_blocking_threads(&hub_config),
        "bounded blocking pool configured"
    );

    // --check mode: validate and exit
    if cli.check {
        tracing::info!("configuration valid");
        return Ok(());
    }

    // --validate-socket mode: bind unix socket, serve validation
    // requests, never start the SPOE listener. See FR-006/FR-007 in
    // specs/004-validate-mode/spec.md.
    if let Some(socket_path) = cli.validate_socket {
        return run_validator_mode(&hub_config, &socket_path).await;
    }

    // Initialize metrics
    metrics::init_metrics(hub_config.metrics_addr)?;

    // Load plugins
    let loaded = plugin_loader::load_all_plugins(
        &hub_config.plugin_dir,
        &hub_config.plugins,
        hub_config.default_timeout_ms,
        None,
    );

    if loaded.is_empty() {
        anyhow::bail!("no plugins loaded successfully — nothing to do");
    }

    tracing::info!(count = loaded.len(), "all plugins loaded");

    // Pre-warm every configured blocking-pool thread so first traffic does not
    // pay thread creation latency.
    let prewarm_count = config::resolve_max_blocking_threads(&hub_config);
    let handles: Vec<_> = (0..prewarm_count)
        .map(|_| tokio::task::spawn_blocking(|| {}))
        .collect();
    futures::future::join_all(handles).await;

    // Build plugin registry with execution plans, wrapped in ArcSwap
    // so SIGHUP can swap in a new registry without interrupting connections
    let registry = registry::shared_registry(loaded)?;

    // Set up graceful shutdown via CancellationToken
    let shutdown = CancellationToken::new();

    let shutdown_trigger = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("received shutdown signal, initiating graceful drain");
        shutdown_trigger.cancel();
    });

    let watcher = setup_config_reload(&registry, &cli.config, &shutdown);

    // Start listeners (returns after shutdown + drain, or on bind failure)
    let result = listener::start_listeners(
        &hub_config.listeners,
        Arc::clone(&registry),
        shutdown,
        hub_config.max_connections,
    )
    .await;

    // Drop the watcher BEFORE shutting down plugins so the bridging
    // task observes the cancellation token and exits cleanly. Otherwise
    // the watcher's internal thread keeps holding the inotify fd open
    // for the (short) tail end of teardown — harmless but tidier this
    // way.
    drop(watcher);

    // Always shutdown plugins, whether we're exiting cleanly or due to error
    registry.load().shutdown_all();

    if let Err(e) = &result {
        tracing::error!(error = %e, "listener failed");
    }
    tracing::info!("shutdown complete");
    result
}

/// Wait for a shutdown signal (SIGTERM or Ctrl-C on Unix, Ctrl-C elsewhere).
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl-c");
    }
}

/// Validator-mode entry point. Loads plugins without calling
/// `init()`, binds the unix socket, and serves requests until SIGTERM
/// or Ctrl-C. The SPOE listener is never started; metrics aren't
/// initialised either since the validator is purely a request-driven
/// validation server with no traffic to instrument.
async fn run_validator_mode(
    hub_config: &config::HubConfig,
    socket_path: &Path,
) -> anyhow::Result<()> {
    tracing::info!(
        socket = %socket_path.display(),
        plugins = hub_config.plugins.len(),
        "starting validator-sidecar mode"
    );

    let plugins =
        validate::load_all_plugins_for_validation(&hub_config.plugin_dir, &hub_config.plugins);
    if plugins.is_empty() {
        anyhow::bail!("no plugins loaded for validator mode — nothing to validate against");
    }
    tracing::info!(count = plugins.len(), "validator-mode plugins ready");

    let shutdown = CancellationToken::new();
    let shutdown_trigger = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("received shutdown signal, closing validator socket");
        shutdown_trigger.cancel();
    });

    validate::serve(
        socket_path,
        Arc::new(plugins),
        validate::ServerConfig::default(),
        shutdown,
    )
    .await?;

    tracing::info!("validator-sidecar shutdown complete");
    Ok(())
}

/// Wire up SIGHUP + file-watch as parallel triggers for the in-process
/// reload pipeline, plus a periodic ticker that updates the reload-age
/// gauge between actual reloads.
///
/// Returns the file-watcher handle. Caller keeps it alive (drop = stop
/// the inotify watch) until the listener returns.
///
/// Splitting this out of `async_main` keeps that function under the
/// 100-line clippy budget and groups the four interlocked spawns
/// (SIGHUP listener, file-watch bridge, file-watch consumer,
/// age-gauge ticker) in one place.
fn setup_config_reload(
    registry: &Arc<ArcSwap<registry::PluginRegistry>>,
    config_path: &Path,
    shutdown: &CancellationToken,
) -> Option<notify::RecommendedWatcher> {
    // Last-success timestamp shared by the reload paths and the
    // age-gauge ticker. Initialised at startup so the gauge has a
    // sensible "since when" baseline before the first reload fires.
    let last_reload_success = Arc::new(Mutex::new(Instant::now()));

    // Mutex serialising apply_reload calls. SIGHUP and file-watch are
    // independent triggers; without this lock, two reloads racing would
    // both call `registry::swap_and_retire` and the loser would retire
    // (quiesce + drain + shut down) a registry that just became (or
    // briefly was) the active one. The lock makes each apply_reload()
    // observe a consistent before/after registry pair. Wasted
    // file-read+dlopen work in the rare race window is an acceptable
    // cost; the serialised-retire invariant matters more.
    let reload_lock = Arc::new(Mutex::new(()));

    // Content of the config file at the last successful load. Used by
    // apply_reload() to skip reloads that would re-load byte-identical
    // content — upstream controllers (HAPTIC) rewrite the file on every
    // reconcile via atomic-rename, and the file-watch triggers
    // unconditionally on any inode change. Without this dedup, every
    // such rewrite tears down the in-flight plugin state (fire-and-
    // forget dispatchers like the mirror plugin lose their background
    // tokio tasks mid-request — observed as gateway-API conformance's
    // HTTPRouteRequestMultipleMirrors timing out waiting for the mirror
    // log to land). Starts as None so the very first SIGHUP / first
    // file-watch event after startup always runs a real load (we don't
    // know what's on disk vs the embedded startup config).
    let last_reload_bytes: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));

    // SIGHUP trigger.
    #[cfg(unix)]
    {
        let registry = Arc::clone(registry);
        let config_path = config_path.to_path_buf();
        let last_success = Arc::clone(&last_reload_success);
        let last_bytes = Arc::clone(&last_reload_bytes);
        let reload_lock = Arc::clone(&reload_lock);
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("failed to listen for SIGHUP");
            loop {
                sig.recv().await;
                tracing::info!("received SIGHUP, reloading configuration");
                apply_reload(
                    &registry,
                    &config_path,
                    &last_success,
                    &last_bytes,
                    &reload_lock,
                )
                .await;
            }
        });
    }

    // File-watch trigger: HAPTIC pushes a new config via the dataplane
    // API (atomic rename), the watcher reacts within milliseconds and
    // calls apply_reload() the same way SIGHUP does. Channel is bounded
    // to 1: any pending reload covers all enqueued changes.
    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::channel::<()>(1);
    let watcher = match config_watcher::spawn(config_path, watch_tx, shutdown.clone()) {
        Ok(w) => Some(w),
        Err(e) => {
            // File-watch is best-effort. If it fails to attach (e.g.
            // unsupported filesystem), we log and continue — SIGHUP
            // still works as a manual fallback. Don't fail startup
            // over this.
            tracing::warn!(
                error = %e,
                "config-watch unavailable; falling back to SIGHUP-only reload"
            );
            None
        }
    };
    {
        let registry = Arc::clone(registry);
        let config_path = config_path.to_path_buf();
        let last_success = Arc::clone(&last_reload_success);
        let last_bytes = Arc::clone(&last_reload_bytes);
        let reload_lock = Arc::clone(&reload_lock);
        tokio::spawn(async move {
            while watch_rx.recv().await.is_some() {
                tracing::info!("config file change detected, reloading configuration");
                apply_reload(
                    &registry,
                    &config_path,
                    &last_success,
                    &last_bytes,
                    &reload_lock,
                )
                .await;
            }
        });
    }

    // Periodic tick to keep the reload-age gauge fresh between actual
    // reloads. 30s is fine-grained enough for alerting (typical alert
    // window is minutes) and coarse enough not to fight prometheus
    // scrape cadence.
    {
        let last_success = Arc::clone(&last_reload_success);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let age = last_success.lock().await.elapsed().as_secs_f64();
                        metrics::update_config_reload_age(age);
                    }
                    () = shutdown.cancelled() => break,
                }
            }
        });
    }

    watcher
}

/// Apply a config reload triggered by SIGHUP or file-watch.
///
/// Calls `reload_plugins` to re-read the config + rebuild the plugin
/// registry, then hands the result to `registry::swap_and_retire`
/// (swap → quiesce → drain → shutdown). On parse / plugin-init failure
/// the previous registry stays in place — operators see the failure in
/// logs and metrics, traffic keeps flowing on last-known-good rules.
///
/// `reload_lock` serialises this function across the SIGHUP and
/// file-watch trigger paths. Without it, two reloads racing would
/// both call `swap_and_retire`, and the loser would retire a registry
/// that just briefly was the active one. We hold the lock across the
/// entire pipeline (file read + dlopen + swap + retire) so each
/// reload sees a consistent before/after registry pair. The cost in
/// the rare race window is one duplicated dlopen; the serialised-
/// retire invariant matters more.
///
/// Both reload paths converge here so the metrics + last-success
/// bookkeeping live in one place. Renaming this helper or splitting
/// the SIGHUP and file-watch paths would let those bookkeeping bits
/// drift out of sync (one path forgets to update the gauge, etc.) —
/// keep them unified.
async fn apply_reload(
    registry: &Arc<ArcSwap<registry::PluginRegistry>>,
    config_path: &Path,
    last_reload_success: &Arc<Mutex<Instant>>,
    last_reload_bytes: &Arc<Mutex<Option<Vec<u8>>>>,
    reload_lock: &Arc<Mutex<()>>,
) {
    let _guard = reload_lock.lock().await;

    // Content-dedup. Reload tears down in-flight plugin state — fire-
    // and-forget dispatchers (mirror) that haven't completed their
    // background tokio task lose the request. Skip when the new
    // content is byte-identical to the last successfully-loaded
    // version: a file-watch event for an atomic-rename rewrite of
    // identical content is observable-by-inotify but semantically a
    // no-op.
    //
    // Read the file ONCE so the dedup decision and the registry
    // rebuild operate on identical bytes. The previous two-read
    // pattern (dedup read + reload_plugins' own read) was TOCTOU-racy
    // — an atomic-rename landing between the two reads would store
    // stale bytes in last_reload_bytes against a freshly-loaded
    // registry, and the NEXT reload's dedup would compare against
    // the wrong baseline (Gitar review on !163). On I/O-error
    // fallback we drop to the path-based loader; the second read
    // will surface the same error with full context.
    let new_bytes = match std::fs::read(config_path) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %config_path.display(),
                "failed to read config for dedup check; falling through to a full reload (the real I/O error will surface from the loader)"
            );
            None
        }
    };

    if let Some(ref bytes) = new_bytes {
        let prev = last_reload_bytes.lock().await;
        if reload_is_noop(prev.as_deref(), bytes) {
            tracing::info!(
                bytes = bytes.len(),
                "config file change detected but content unchanged; skipping reload"
            );
            metrics::record_config_reload_skipped();
            // Refresh the success age — the operator's intent ("the
            // config I want is loaded") is still met, even though we
            // didn't physically reload.
            *last_reload_success.lock().await = Instant::now();
            metrics::update_config_reload_age(0.0);
            return;
        }
    }

    // Build the registry from the bytes we just read (when available)
    // so the dedup snapshot stored on success is guaranteed to match
    // what the registry was built from. Fall back to a path-based
    // load only when the pre-read failed — in that case there's
    // nothing useful to cache anyway.
    let load_result = {
        let previous = registry.load();
        match new_bytes.as_deref() {
            Some(bytes) => reload_plugins_from_bytes(bytes, &previous),
            None => reload_plugins(config_path, &previous),
        }
    };

    match load_result {
        Ok((new_hub_config, new_registry)) => {
            let drain_timeout = config::resolve_reload_drain_timeout(&new_hub_config);
            if drain_timeout.is_zero() {
                tracing::warn!(
                    "reload_drain_timeout_ms = 0; skipping quiesce and drain \
                     (in-flight work may be lost)"
                );
            }

            // Retire phase: swap → quiesce → drain → shutdown. The
            // swap atomically routes new NOTIFY frames to the new
            // registry; the quiesce wait lets dispatches that loaded
            // the pre-swap snapshot finish (so their fire-and-forget
            // background work is actually enqueued); the drain then
            // waits for that background work; only then is the old
            // plugin set shut down. See `registry::swap_and_retire`
            // for the ordering rationale and
            // `config::resolve_reload_drain_timeout` for how the
            // budget is computed.
            let report = registry::swap_and_retire(registry, new_registry, drain_timeout).await;

            if !drain_timeout.is_zero() {
                metrics::record_config_reload_drain_duration(report.retire_duration);
                if !report.quiesced_cleanly {
                    metrics::record_config_reload_quiesce_forced();
                }
                for (plugin_name, drained_cleanly) in &report.drain_results {
                    if !drained_cleanly {
                        metrics::record_config_reload_drain_forced(plugin_name);
                    }
                }
            }

            *last_reload_success.lock().await = Instant::now();
            if let Some(bytes) = new_bytes {
                *last_reload_bytes.lock().await = Some(bytes);
            }
            metrics::record_config_reload_success();
            tracing::info!("configuration reloaded successfully");
        }
        Err(e) => {
            metrics::record_config_reload_failure();
            tracing::error!(error = %e, "config reload failed, keeping current config");
        }
    }
}

/// Decide whether the bytes just read from the config file are
/// byte-identical to what was last successfully reloaded. Pure
/// function so the dedup decision can be unit-tested without standing
/// up a real `PluginRegistry`. Returns true (skip the reload) only
/// when both inputs are present AND equal; the startup case (no last
/// content yet, `None`) always proceeds with a full reload.
fn reload_is_noop(last: Option<&[u8]>, new: &[u8]) -> bool {
    last == Some(new)
}

/// Re-read the config file and rebuild the plugin registry.
/// On success, returns a new `PluginRegistry` ready to be swapped in.
///
/// Used by the I/O-error fallback path in `apply_reload` (when the
/// pre-dedup read failed and we don't have bytes to pass to the
/// from-bytes variant). The common path goes through
/// `reload_plugins_from_bytes` instead, which guarantees the dedup
/// snapshot stored on success matches the bytes the registry was
/// built from.
fn reload_plugins(
    config_path: &Path,
    previous: &registry::PluginRegistry,
) -> anyhow::Result<(config::HubConfig, registry::PluginRegistry)> {
    let hub_config = config::HubConfig::load(config_path)?;
    let registry = build_registry_from_hub_config(&hub_config, previous)?;
    Ok((hub_config, registry))
}

/// Rebuild the plugin registry from already-read config bytes. Lifts
/// the parse + plugin-loader pipeline off the filesystem path so the
/// dedup read and the registry rebuild can share a single byte
/// buffer (Gitar review on !163 — TOCTOU between two independent
/// reads of `config_path`). Failures still surface the same error
/// types as the path-based loader (parse errors are
/// `config::ConfigError::Parse`; validation errors keep their
/// validation prefix), so the caller doesn't need to distinguish
/// which loader ran.
fn reload_plugins_from_bytes(
    bytes: &[u8],
    previous: &registry::PluginRegistry,
) -> anyhow::Result<(config::HubConfig, registry::PluginRegistry)> {
    let content = std::str::from_utf8(bytes)
        .map_err(|e| anyhow::anyhow!("config is not valid UTF-8: {e}"))?;
    let hub_config = config::HubConfig::load_from_str(content)?;
    let registry = build_registry_from_hub_config(&hub_config, previous)?;
    Ok((hub_config, registry))
}

/// Build the successor registry. Plugins whose library and params are
/// unchanged keep their running instance from `previous`; only the
/// rest are created and initialised anew.
fn build_registry_from_hub_config(
    hub_config: &config::HubConfig,
    previous: &registry::PluginRegistry,
) -> anyhow::Result<registry::PluginRegistry> {
    let loaded = plugin_loader::load_all_plugins(
        &hub_config.plugin_dir,
        &hub_config.plugins,
        hub_config.default_timeout_ms,
        Some(previous),
    );

    if loaded.is_empty() {
        anyhow::bail!("reload produced no valid plugins, keeping current config");
    }

    let carried_over = loaded.iter().filter(|p| p.carried_over).count();
    tracing::info!(
        count = loaded.len(),
        carried_over,
        loaded = loaded.len() - carried_over,
        "plugins reloaded"
    );
    Ok(registry::PluginRegistry::build(loaded)?)
}

#[cfg(test)]
mod tests {
    use super::reload_is_noop;

    // Dedup decision: identical content is the precondition for skipping
    // a reload. The chart-driven failure mode (HAPTIC rewriting
    // spoa-hub-config.toml every reconcile with byte-identical content)
    // is the case that motivated this whole helper — pin it.
    #[test]
    fn reload_is_noop_when_content_matches() {
        let last = b"plugins = [\"mirror\"]\n";
        let new = b"plugins = [\"mirror\"]\n";
        assert!(reload_is_noop(Some(last), new));
    }

    // Any byte difference must trigger a real reload — including
    // trailing-whitespace-only or newline-only edits (operators
    // intentionally tweaking a config and expecting it to take effect).
    #[test]
    fn reload_is_noop_false_when_content_differs() {
        let last = b"plugins = [\"mirror\"]\n";
        let new = b"plugins = [\"mirror\", \"coraza\"]\n";
        assert!(!reload_is_noop(Some(last), new));
    }

    // Startup case: no previous load tracked. The first reload after
    // startup must always run — we don't know whether what's on disk
    // matches the embedded startup-time load, so the dedup must
    // proceed (`reload_is_noop` returns false → caller falls through
    // to a real reload).
    #[test]
    fn reload_is_noop_false_on_first_load() {
        let new = b"plugins = []\n";
        assert!(!reload_is_noop(None, new));
    }

    // Whitespace difference still counts as a difference. Operators
    // who change only formatting expect the reload to fire — letting
    // it slip silently would obscure config errors that only become
    // visible after a reload (e.g. typoed plugin name now in a stale
    // section that "looks" unchanged on diff).
    #[test]
    fn reload_is_noop_false_on_whitespace_only_change() {
        let last = b"plugins = [\"mirror\"]\n";
        let new = b"plugins  =  [\"mirror\"]\n";
        assert!(!reload_is_noop(Some(last), new));
    }

    // Empty file vs empty-last-known. Empty is a valid content state
    // (the file existed but was truncated to zero bytes by an upstream
    // writer between two reconciles); reload should not be skipped
    // unless the previous content was ALSO empty.
    #[test]
    fn reload_is_noop_true_when_both_empty() {
        assert!(reload_is_noop(Some(b""), b""));
    }
    #[test]
    fn reload_is_noop_false_when_only_new_is_empty() {
        let last = b"plugins = [\"mirror\"]\n";
        assert!(!reload_is_noop(Some(last), b""));
    }
}
