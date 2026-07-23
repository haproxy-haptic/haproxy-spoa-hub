//! Plugin loading orchestration.
//!
//! Wraps [`crate::plugin::Plugin`] with config-driven loading: pulls
//! TOML params from `PluginConfig`, runs the plugin's optional
//! JSON-Schema validation, runs `validate()`, and finally calls
//! `init()`. Returns a [`LoadedPlugin`] tagging the plugin with its
//! configured messages, dependencies, and timeout.
//!
//! # ABI evolution rules
//!
//! When you add a new method to the plugin-api `PluginVTable` (and
//! bump `PLUGIN_API_VERSION`), this loader does NOT need updating
//! unless the new method participates in load-time validation. The
//! `Plugin` struct's accessor methods are where version-gating lives.
//! See `crates/plugin-api/src/vtable.rs` and `crates/hub/CLAUDE.md`.

use std::{path::Path, sync::Arc, time::Duration};

use abi_stable::std_types::{RHashMap, RString};
use haproxy_spoa_hub_plugin_api::{ConfigValue, DiagnosticSeverity, PluginContext};

use crate::config::PluginConfig;
use crate::plugin::{Plugin, PluginLoadError};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to load plugin '{name}': {source}")]
    Load {
        name: String,
        #[source]
        source: PluginLoadError,
    },
    #[error("plugin '{name}' init failed: {reason}")]
    Init { name: String, reason: String },
    #[error("plugin '{name}' config validation failed: {errors}")]
    Validation { name: String, errors: String },
}

#[derive(Debug)]
pub struct LoadedPlugin {
    pub name: String,
    pub version: String,
    pub instance: Arc<Plugin>,
    pub messages: Vec<String>,
    pub depends_on: Vec<String>,
    pub timeout: Duration,
    pub max_concurrency: usize,
    pub max_queue: usize,
    pub queue_timeout: Option<Duration>,
    /// Resolved adaptive-concurrency config (ADR-0002) when the plugin opted in
    /// via `adaptive_concurrency = true`; `None` for the fixed-limit path.
    pub adaptive: Option<crate::adaptive::AdaptiveConfig>,
}

/// Convert a `toml::Value` into an FFI-safe `ConfigValue`.
///
/// TOML `Datetime` values are stringified since there is no
/// corresponding `ConfigValue` variant.
fn toml_to_config_value(value: &toml::Value) -> ConfigValue {
    match value {
        toml::Value::String(s) => ConfigValue::String(s.as_str().into()),
        toml::Value::Integer(i) => ConfigValue::Integer(*i),
        toml::Value::Float(f) => ConfigValue::Float(*f),
        toml::Value::Boolean(b) => ConfigValue::Bool(*b),
        toml::Value::Array(arr) => {
            ConfigValue::Array(arr.iter().map(toml_to_config_value).collect())
        }
        toml::Value::Table(table) => ConfigValue::Table(toml_table_to_config(table)),
        toml::Value::Datetime(dt) => ConfigValue::String(dt.to_string().into()),
    }
}

/// Convert a `toml::Table` into an `RHashMap<RString, ConfigValue>`.
fn toml_table_to_config(table: &toml::Table) -> RHashMap<RString, ConfigValue> {
    table
        .iter()
        .map(|(k, v)| (k.as_str().into(), toml_to_config_value(v)))
        .collect()
}

/// Format a JSON Schema validation error, including the instance path
/// when non-empty.
fn format_schema_error(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    if path.is_empty() {
        error.to_string()
    } else {
        format!("{path}: {error}")
    }
}

/// Validate plugin config against a JSON Schema declared by the plugin.
///
/// Returns an outer `Err(meta_error)` if the plugin's `config_schema()`
/// itself isn't valid JSON Schema (a plugin bug, not an operator
/// config issue) and `Ok(diagnostics)` with the per-instance schema
/// errors otherwise — `Ok(empty_vec)` means structurally valid.
///
/// Shared between two callers with different error vocabularies:
///   - [`load_plugin`] wraps the result in [`LoadError::Validation`]
///     to fail-fast at startup.
///   - [`crate::validate::orchestrator`] wraps it in `WireDiagnostic`s
///     to surface the same errors over the validate-socket protocol.
///
/// Plugin authors writing `validate()` overrides can rely on this
/// having run first — the [`PluginContext`] passed to `validate()`
/// always satisfies `config_schema()` in both call paths, so plugins
/// only need to do *semantic* validation (e.g. compiling `SecLang`
/// directives via Coraza) and never structural validation. See
/// `crates/plugin-api/src/lib.rs`'s `define_plugin!` docstring for
/// the contract.
pub(crate) fn collect_schema_errors(
    instance: &Plugin,
    params: &toml::Table,
) -> Result<Vec<String>, String> {
    let Some(schema_str) = instance.config_schema() else {
        return Ok(Vec::new());
    };

    let schema_json: serde_json::Value =
        serde_json::from_str(&schema_str).map_err(|e| format!("invalid JSON Schema: {e}"))?;

    let schema =
        jsonschema::validator_for(&schema_json).map_err(|e| format!("invalid JSON Schema: {e}"))?;

    let params_json =
        serde_json::to_value(params).expect("TOML table is always serializable to JSON");

    Ok(schema
        .iter_errors(&params_json)
        .map(|e| format_schema_error(&e))
        .collect())
}

/// Validate plugin config against a JSON Schema declared by the
/// plugin. Wraps [`collect_schema_errors`] for the load-time call
/// site, where any error is fatal.
fn validate_config_schema(
    instance: &Plugin,
    plugin_name: &str,
    params: &toml::Table,
) -> Result<(), LoadError> {
    match collect_schema_errors(instance, params) {
        Ok(errors) if errors.is_empty() => Ok(()),
        Ok(errors) => Err(LoadError::Validation {
            name: plugin_name.to_owned(),
            errors: errors.join("; "),
        }),
        Err(meta_error) => Err(LoadError::Validation {
            name: plugin_name.to_owned(),
            errors: meta_error,
        }),
    }
}

/// Run a plugin's `validate()` method and convert any returned
/// diagnostics into log lines. Errors block init; warnings are logged
/// at WARN but not blocking (spec FR-004 / FR-005).
fn run_plugin_validate(
    instance: &Plugin,
    plugin_name: &str,
    context: &PluginContext,
) -> Result<(), LoadError> {
    let diagnostics = instance.validate(context);
    let mut errors = Vec::new();
    for d in &diagnostics {
        let pos = if d.line > 0 {
            format!(" at line {}, column {}", d.line, d.column)
        } else {
            String::new()
        };
        match d.severity {
            DiagnosticSeverity::Warning => {
                tracing::warn!(
                    plugin = plugin_name,
                    line = d.line,
                    column = d.column,
                    message = %d.message,
                    "plugin validation warning"
                );
            }
            DiagnosticSeverity::Error => {
                errors.push(format!("{}{pos}", d.message));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(LoadError::Validation {
            name: plugin_name.to_owned(),
            errors: errors.join("; "),
        })
    }
}

/// Load a single plugin from its `.so` file.
pub fn load_plugin(
    plugin_dir: &Path,
    config: &PluginConfig,
    default_timeout_ms: u64,
) -> Result<LoadedPlugin, LoadError> {
    let lib_path = plugin_dir.join(&config.library);
    let name = &config.name;

    // SAFETY: dlopen is inherently unsafe; we trust the operator's
    // configured plugin path. The Plugin::load constructor handles
    // the dlsym + version-check + factory call.
    let mut instance = unsafe { Plugin::load(&lib_path) }.map_err(|source| LoadError::Load {
        name: name.clone(),
        source,
    })?;

    // Validate config against plugin's schema (if declared).
    validate_config_schema(&instance, name, &config.params)?;

    // Build plugin context from config.
    let context = PluginContext {
        name: name.as_str().into(),
        config: toml_table_to_config(&config.params),
    };

    // Deep validation via the plugin's `validate()` method (FR-004).
    // Runs before init() so plugins that override validate get
    // sharper startup errors than init's generic failure path.
    run_plugin_validate(&instance, name, &context)?;

    // Initialize.
    instance.init(&context).map_err(|e| LoadError::Init {
        name: name.clone(),
        reason: e.to_string(),
    })?;

    let version = instance.plugin_version();
    Ok(LoadedPlugin {
        name: name.clone(),
        version,
        instance: Arc::new(instance),
        messages: config.messages.clone(),
        depends_on: config.depends_on.clone(),
        timeout: Duration::from_millis(config.timeout_ms.unwrap_or(default_timeout_ms)),
        max_concurrency: crate::config::resolve_plugin_concurrency(config),
        max_queue: config.max_queue,
        queue_timeout: config.queue_timeout_ms.map(Duration::from_millis),
        adaptive: config.adaptive_concurrency.then(|| {
            crate::adaptive::AdaptiveConfig::resolve(config.max_concurrency, &config.params)
        }),
    })
}

/// Load all plugins from config, logging errors for failures.
/// Returns the successfully loaded plugins.
pub fn load_all_plugins(
    plugin_dir: &Path,
    configs: &[PluginConfig],
    default_timeout_ms: u64,
) -> Vec<LoadedPlugin> {
    let mut loaded = Vec::with_capacity(configs.len());

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
    use std::{fs, io::Write};

    use super::*;
    use crate::test_support::{self, workspace_target_dir};

    #[test]
    fn test_load_valid_plugin() {
        let plugin_dir = workspace_target_dir();
        let mut config = test_support::plugin_config("noop", "libnoop_plugin.so", &["test"], 0);
        config.timeout_ms = Some(500);
        let loaded = load_plugin(&plugin_dir, &config, 100).unwrap();
        assert_eq!(loaded.name, "noop");
        assert!(!loaded.version.is_empty());
        assert_eq!(loaded.timeout, Duration::from_millis(500));
    }

    #[test]
    fn test_load_plugin_with_params() {
        let plugin_dir = workspace_target_dir();
        let mut config = test_support::plugin_config("noop", "libnoop_plugin.so", &["test"], 0);
        config.timeout_ms = None;
        config.params.insert(
            "db_path".into(),
            toml::Value::String("/data/test.mmdb".into()),
        );
        config
            .params
            .insert("locale".into(), toml::Value::String("de".into()));
        let loaded = load_plugin(&plugin_dir, &config, 100).unwrap();
        assert_eq!(loaded.name, "noop");
    }

    #[test]
    fn test_load_uses_default_timeout() {
        let plugin_dir = workspace_target_dir();
        let mut config = test_support::plugin_config("noop", "libnoop_plugin.so", &["test"], 0);
        config.timeout_ms = None;
        let loaded = load_plugin(&plugin_dir, &config, 200).unwrap();
        assert_eq!(loaded.timeout, Duration::from_millis(200));
    }

    #[test]
    fn test_load_nonexistent_so_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_support::plugin_config("missing", "libmissing.so", &[], 0);
        let err = load_plugin(dir.path(), &config, 100).unwrap_err();
        assert!(matches!(err, LoadError::Load { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn test_load_invalid_so_fails() {
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("libfake.so");
        let mut f = fs::File::create(&fake_path).unwrap();
        f.write_all(b"not a shared library").unwrap();

        let config = test_support::plugin_config("fake", "libfake.so", &[], 0);
        let err = load_plugin(dir.path(), &config, 100).unwrap_err();
        assert!(matches!(err, LoadError::Load { .. }));
    }

    #[test]
    fn test_load_all_skips_failures() {
        let dir = tempfile::tempdir().unwrap();
        let fake_path = dir.path().join("libfake.so");
        fs::write(&fake_path, b"bad").unwrap();

        let configs = vec![test_support::plugin_config("bad", "libfake.so", &[], 0)];
        let loaded = load_all_plugins(dir.path(), &configs, 100);
        assert!(loaded.is_empty());
    }
}
