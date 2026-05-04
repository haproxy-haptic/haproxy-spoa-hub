use std::{
    io::stderr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use arc_swap::ArcSwap;
use clap::Parser;
use haproxy_spoa_hub::{
    config, config_watcher, listener, metrics, plugin_loader, registry, validate,
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

    // Load and validate configuration
    let hub_config = config::HubConfig::load(&cli.config)?;

    // Build tokio runtime with configurable worker threads.
    // thread_keep_alive is configurable to avoid cold-start thread creation
    // overhead (~10-50µs) when blocking pool threads are reaped between
    // traffic bursts. Default: 300s (tokio default is 10s).
    let mut rt_builder = runtime::Builder::new_multi_thread();
    rt_builder
        .enable_all()
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
    );

    if loaded.is_empty() {
        anyhow::bail!("no plugins loaded successfully — nothing to do");
    }

    tracing::info!(count = loaded.len(), "all plugins loaded");

    // Pre-warm the blocking thread pool so threads are parked and ready
    // before the first SPOP message arrives. The count matches the number
    // of async worker threads, since each worker can submit one
    // spawn_blocking call concurrently.
    let prewarm_count = hub_config
        .worker_threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, std::num::NonZero::get));
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
    // both call `registry.swap` then `shutdown_all()` on what they got
    // back, and the loser would shut down a registry that just became
    // (or briefly was) the active one — including any plugins still
    // held by in-flight SPOE messages via Arc snapshots. The lock makes
    // each apply_reload() observe a consistent before/after registry
    // pair. Wasted file-read+dlopen work in the rare race window is an
    // acceptable cost; the serialised-shutdown invariant matters more.
    let reload_lock = Arc::new(Mutex::new(()));

    // SIGHUP trigger.
    #[cfg(unix)]
    {
        let registry = Arc::clone(registry);
        let config_path = config_path.to_path_buf();
        let last_success = Arc::clone(&last_reload_success);
        let reload_lock = Arc::clone(&reload_lock);
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("failed to listen for SIGHUP");
            loop {
                sig.recv().await;
                tracing::info!("received SIGHUP, reloading configuration");
                apply_reload(&registry, &config_path, &last_success, &reload_lock).await;
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
        let reload_lock = Arc::clone(&reload_lock);
        tokio::spawn(async move {
            while watch_rx.recv().await.is_some() {
                tracing::info!("config file change detected, reloading configuration");
                apply_reload(&registry, &config_path, &last_success, &reload_lock).await;
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
/// registry, then atomically swaps the result into the shared
/// `ArcSwap<PluginRegistry>`. On parse / plugin-init failure the
/// previous registry stays in place — operators see the failure in
/// logs and metrics, traffic keeps flowing on last-known-good rules.
///
/// `reload_lock` serialises this function across the SIGHUP and
/// file-watch trigger paths. Without it, two reloads racing would
/// both call `registry.swap` then `shutdown_all()` on what they got
/// back, and the loser would shut down a registry that just briefly
/// was the active one — including any plugins still held by in-flight
/// SPOE messages via `Arc` snapshots. We hold the lock across the
/// entire pipeline (file read + dlopen + swap + `shutdown_all`) so each
/// reload sees a consistent before/after registry pair. The cost in
/// the rare race window is one duplicated dlopen; the serialised-
/// shutdown invariant matters more.
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
    reload_lock: &Arc<Mutex<()>>,
) {
    let _guard = reload_lock.lock().await;
    match reload_plugins(config_path) {
        Ok(new_registry) => {
            let old_registry = registry.swap(Arc::new(new_registry));
            old_registry.shutdown_all();
            *last_reload_success.lock().await = Instant::now();
            metrics::record_config_reload_success();
            tracing::info!("configuration reloaded successfully");
        }
        Err(e) => {
            metrics::record_config_reload_failure();
            tracing::error!(error = %e, "config reload failed, keeping current config");
        }
    }
}

/// Re-read the config file and rebuild the plugin registry.
/// On success, returns a new `PluginRegistry` ready to be swapped in.
fn reload_plugins(config_path: &Path) -> anyhow::Result<registry::PluginRegistry> {
    let hub_config = config::HubConfig::load(config_path)?;

    let loaded = plugin_loader::load_all_plugins(
        &hub_config.plugin_dir,
        &hub_config.plugins,
        hub_config.default_timeout_ms,
    );

    if loaded.is_empty() {
        anyhow::bail!("reload produced no valid plugins, keeping current config");
    }

    tracing::info!(count = loaded.len(), "plugins reloaded");
    Ok(registry::PluginRegistry::build(loaded)?)
}
