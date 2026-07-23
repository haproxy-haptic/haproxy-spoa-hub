use std::path::PathBuf;

use crate::{
    config::PluginConfig,
    plugin_loader::{self, LoadedPlugin},
};

/// Path to the workspace `target/debug` directory where plugin `.so` files are built.
#[must_use]
pub fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
}

/// Build a `PluginConfig` with common defaults (no dependencies, empty params).
///
/// Shared across unit tests, integration tests, and benchmarks to avoid
/// duplicating `PluginConfig` construction.
#[must_use]
pub fn plugin_config(
    name: &str,
    library: &str,
    messages: &[&str],
    timeout_ms: u64,
) -> PluginConfig {
    PluginConfig {
        name: name.into(),
        library: library.into(),
        messages: messages.iter().map(|&m| m.into()).collect(),
        depends_on: vec![],
        timeout_ms: Some(timeout_ms),
        max_concurrency: None,
        max_queue: 0,
        queue_timeout_ms: None,
        adaptive_concurrency: false,
        params: toml::Table::new(),
    }
}

/// Load plugins from a list of configs using the workspace build directory.
///
/// Shared across integration tests and benchmarks to avoid duplicating
/// the load-and-unwrap pattern.
#[must_use]
pub fn load_test_plugins(configs: &[PluginConfig], default_timeout_ms: u64) -> Vec<LoadedPlugin> {
    let dir = workspace_target_dir();
    configs
        .iter()
        .map(|c| {
            plugin_loader::load_plugin(&dir, c, default_timeout_ms)
                .unwrap_or_else(|e| panic!("failed to load plugin '{}': {e}", c.name))
        })
        .collect()
}

/// Load the noop plugin from the workspace build output.
#[cfg(test)]
#[must_use]
pub fn load_noop_plugin() -> LoadedPlugin {
    let config = plugin_config("noop", "libnoop_plugin.so", &["test-msg"], 1000);
    plugin_loader::load_plugin(&workspace_target_dir(), &config, 100)
        .unwrap_or_else(|e| panic!("failed to load noop plugin: {e}"))
}
