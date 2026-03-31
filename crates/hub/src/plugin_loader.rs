use std::path::Path;
use std::sync::Arc;

use abi_stable::library::{RootModule, lib_header_from_path};
use abi_stable::std_types::{RHashMap, RString};
use haproxy_spoa_hub_plugin_api::{PluginBox, PluginContext, PluginMod_Ref};

use crate::config::PluginConfig;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to load plugin '{name}' from '{path}': {source}")]
    Load {
        name: String,
        path: String,
        source: abi_stable::library::LibraryError,
    },
    #[error("plugin '{name}' init failed: {reason}")]
    Init { name: String, reason: String },
}

#[derive(Debug)]
pub struct LoadedPlugin {
    pub name: String,
    pub version: String,
    pub instance: Arc<PluginBox>,
    pub messages: Vec<String>,
    pub depends_on: Vec<String>,
    pub timeout: std::time::Duration,
}

/// Load a single plugin from its `.so` file.
pub fn load_plugin(
    plugin_dir: &Path,
    config: &PluginConfig,
    default_timeout_ms: u64,
) -> Result<LoadedPlugin, LoadError> {
    let lib_path = plugin_dir.join(&config.library);
    let lib_path_str = lib_path.display().to_string();

    // Verify the library has a valid abi_stable header before full load
    let _header = lib_header_from_path(&lib_path).map_err(|e| LoadError::Load {
        name: config.name.clone(),
        path: lib_path_str.clone(),
        source: e,
    })?;

    // Load the root module (performs ABI version + layout checking)
    let module = PluginMod_Ref::load_from_file(&lib_path).map_err(|e| LoadError::Load {
        name: config.name.clone(),
        path: lib_path_str,
        source: e,
    })?;

    // Create plugin instance via factory function
    let mut instance = module.new()().into_result().map_err(|e| LoadError::Init {
        name: config.name.clone(),
        reason: e.to_string(),
    })?;

    // Build plugin context from config
    let mut params = RHashMap::new();
    for (k, v) in &config.params {
        params.insert(RString::from(k.as_str()), RString::from(v.as_str()));
    }
    let context = PluginContext {
        name: RString::from(config.name.as_str()),
        config: params,
    };

    // Initialize the plugin with its context
    instance
        .init(&context)
        .into_result()
        .map_err(|e| LoadError::Init {
            name: config.name.clone(),
            reason: e.to_string(),
        })?;

    let version = instance.version().to_string();
    let timeout_ms = config.timeout_ms.unwrap_or(default_timeout_ms);

    Ok(LoadedPlugin {
        name: config.name.clone(),
        version,
        instance: Arc::new(instance),
        messages: config.messages.clone(),
        depends_on: config.depends_on.clone(),
        timeout: std::time::Duration::from_millis(timeout_ms),
    })
}

/// Load all plugins from config, logging errors for failures.
/// Returns the successfully loaded plugins.
pub fn load_all_plugins(
    plugin_dir: &Path,
    configs: &[PluginConfig],
    default_timeout_ms: u64,
) -> Vec<LoadedPlugin> {
    let mut loaded = Vec::new();

    for config in configs {
        match load_plugin(plugin_dir, config, default_timeout_ms) {
            Ok(plugin) => {
                tracing::info!(
                    plugin = %plugin.name,
                    version = %plugin.version,
                    messages = ?plugin.messages,
                    "plugin loaded"
                );
                loaded.push(plugin);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load plugin");
            }
        }
    }

    loaded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn workspace_target_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("debug")
    }

    #[test]
    fn test_load_valid_plugin() {
        let plugin_dir = workspace_target_dir();
        let config = PluginConfig {
            name: "noop".into(),
            library: "libnoop_plugin.so".into(),
            messages: vec!["test".into()],
            depends_on: vec![],
            timeout_ms: Some(500),
            params: Default::default(),
        };
        let loaded = load_plugin(&plugin_dir, &config, 100).unwrap();
        assert_eq!(loaded.name, "noop");
        assert!(!loaded.version.is_empty());
        assert_eq!(loaded.timeout, std::time::Duration::from_millis(500));
    }

    #[test]
    fn test_load_plugin_with_params() {
        let plugin_dir = workspace_target_dir();
        let mut params = std::collections::HashMap::new();
        params.insert("db_path".to_string(), "/data/test.mmdb".to_string());
        params.insert("locale".to_string(), "de".to_string());
        let config = PluginConfig {
            name: "noop".into(),
            library: "libnoop_plugin.so".into(),
            messages: vec!["test".into()],
            depends_on: vec![],
            timeout_ms: None,
            params,
        };
        // Plugin loads and init succeeds even with params (noop ignores them)
        let loaded = load_plugin(&plugin_dir, &config, 100).unwrap();
        assert_eq!(loaded.name, "noop");
    }

    #[test]
    fn test_load_uses_default_timeout() {
        let plugin_dir = workspace_target_dir();
        let config = PluginConfig {
            name: "noop".into(),
            library: "libnoop_plugin.so".into(),
            messages: vec!["test".into()],
            depends_on: vec![],
            timeout_ms: None,
            params: Default::default(),
        };
        let loaded = load_plugin(&plugin_dir, &config, 200).unwrap();
        assert_eq!(loaded.timeout, std::time::Duration::from_millis(200));
    }

    #[test]
    fn test_load_nonexistent_so_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = PluginConfig {
            name: "missing".into(),
            library: "libmissing.so".into(),
            messages: vec![],
            depends_on: vec![],
            timeout_ms: None,
            params: Default::default(),
        };
        let err = load_plugin(dir.path(), &config, 100).unwrap_err();
        assert!(matches!(err, LoadError::Load { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn test_load_invalid_so_fails() {
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("libfake.so");
        let mut f = std::fs::File::create(&fake_path).unwrap();
        f.write_all(b"not a shared library").unwrap();

        let config = PluginConfig {
            name: "fake".into(),
            library: "libfake.so".into(),
            messages: vec![],
            depends_on: vec![],
            timeout_ms: None,
            params: Default::default(),
        };
        let err = load_plugin(dir.path(), &config, 100).unwrap_err();
        assert!(matches!(err, LoadError::Load { .. }));
    }

    #[test]
    fn test_load_all_skips_failures() {
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("libfake.so");
        std::fs::write(&fake_path, b"bad").unwrap();

        let configs = vec![PluginConfig {
            name: "bad".into(),
            library: "libfake.so".into(),
            messages: vec![],
            depends_on: vec![],
            timeout_ms: None,
            params: Default::default(),
        }];
        let loaded = load_all_plugins(dir.path(), &configs, 100);
        assert!(loaded.is_empty());
    }
}
