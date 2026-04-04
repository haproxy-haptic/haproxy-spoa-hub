use std::io::stderr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use haproxy_spoa_hub::{config, listener, metrics, plugin_loader, registry};
use tokio::runtime;
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
}

fn init_logging(log_level: &str) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    tracing_subscriber::registry()
        .with(env_filter)
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

    // Build tokio runtime with configurable worker threads
    let mut rt_builder = runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
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

    // Build plugin registry with execution plans, wrapped in ArcSwap
    // for future hot-reload support (e.g., SIGHUP)
    let registry = registry::shared_registry(loaded)?;

    // Set up graceful shutdown via CancellationToken
    let shutdown = CancellationToken::new();

    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("received shutdown signal, initiating graceful drain");
            shutdown.cancel();
        });
    }

    // Set up SIGHUP handler for config reload
    #[cfg(unix)]
    {
        let registry = Arc::clone(&registry);
        let config_path = cli.config.clone();
        let default_timeout = hub_config.default_timeout_ms;
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("failed to listen for SIGHUP");
            loop {
                sig.recv().await;
                tracing::info!("received SIGHUP, reloading configuration");
                match reload_plugins(&config_path, default_timeout) {
                    Ok(new_registry) => {
                        let old_registry = registry.swap(Arc::new(new_registry));
                        old_registry.shutdown_all();
                        tracing::info!("configuration reloaded successfully");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "config reload failed, keeping current config");
                    }
                }
            }
        });
    }

    // Start listeners (returns after shutdown + drain, or on bind failure)
    let result = listener::start_listeners(
        &hub_config.listeners,
        Arc::clone(&registry),
        shutdown,
        hub_config.max_connections,
    )
    .await;

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

/// Re-read the config file and rebuild the plugin registry.
/// On success, returns a new `PluginRegistry` ready to be swapped in.
fn reload_plugins(
    config_path: &Path,
    default_timeout_ms: u64,
) -> anyhow::Result<registry::PluginRegistry> {
    let hub_config = config::HubConfig::load(config_path)?;

    let loaded = plugin_loader::load_all_plugins(
        &hub_config.plugin_dir,
        &hub_config.plugins,
        default_timeout_ms,
    );

    if loaded.is_empty() {
        anyhow::bail!("reload produced no valid plugins, keeping current config");
    }

    tracing::info!(count = loaded.len(), "plugins reloaded");
    Ok(registry::PluginRegistry::build(loaded)?)
}
