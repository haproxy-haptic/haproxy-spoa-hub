use std::path::PathBuf;

use crate::{
    config::PluginConfig,
    plugin_loader::{self, LoadedPlugin},
};

/// Path to the workspace `target/debug` directory where plugin `.so` files are built.
pub fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
}

/// Load the noop plugin from the workspace build output.
pub fn load_noop_plugin() -> LoadedPlugin {
    let dir = workspace_target_dir();
    let config = PluginConfig {
        name: "noop".into(),
        library: "libnoop_plugin.so".into(),
        messages: vec!["test-msg".into()],
        depends_on: vec![],
        timeout_ms: Some(1000),
        params: toml::Table::default(),
    };
    plugin_loader::load_plugin(&dir, &config, 100).unwrap()
}
