//! Validator-mode plugin loader.
//!
//! Mirrors [`crate::plugin_loader::load_plugin`] but skips both the
//! schema-validation step and the `init()` call: the validator sidecar
//! holds plugin instances purely to dispatch `validate()` against them
//! (FR-007). No init means no side effects (no tokio tasks, no
//! network, no file handles), which is the contract that makes
//! validator mode safe to run in the controller pod.

use std::{path::Path, sync::Arc};

use crate::config::PluginConfig;
use crate::plugin::Plugin;
use crate::plugin_loader::LoadError;
use crate::validate::orchestrator::PluginMap;

/// Load a single plugin's `.so` and return its instance without
/// calling `init()` or validating its bootstrap params (the validator
/// sidecar's bootstrap config has no `[plugins.params]` — params come
/// per-validation-request from clients).
pub fn load_plugin_for_validation(
    plugin_dir: &Path,
    config: &PluginConfig,
) -> Result<(String, Arc<Plugin>), LoadError> {
    let lib_path = plugin_dir.join(&config.library);
    let name = &config.name;

    // SAFETY: dlopen of operator-configured plugin path. Plugin::load
    // performs dlsym + version-check + factory call.
    let instance = unsafe { Plugin::load(&lib_path) }.map_err(|source| LoadError::Load {
        name: name.clone(),
        source,
    })?;

    Ok((name.clone(), Arc::new(instance)))
}

/// Load all plugins for validator mode. Logs failures and returns
/// the successfully loaded plugins as a name→instance map suitable
/// for the orchestrator.
pub fn load_all_plugins_for_validation(plugin_dir: &Path, configs: &[PluginConfig]) -> PluginMap {
    let mut loaded: PluginMap = PluginMap::with_capacity(configs.len());
    for config in configs {
        match load_plugin_for_validation(plugin_dir, config) {
            Ok((name, instance)) => {
                tracing::info!(plugin = %name, "validator-mode plugin loaded");
                loaded.insert(name, instance);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load plugin for validator mode");
            }
        }
    }
    loaded
}
