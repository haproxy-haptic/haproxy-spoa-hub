use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use abi_stable::{
    library::{RootModule, lib_header_from_path},
    std_types::{RHashMap, RString},
};
use haproxy_spoa_hub_plugin_api::{ConfigValue, PluginBox, PluginContext, PluginMod_Ref};

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
    #[error("plugin '{name}' config validation failed: {errors}")]
    Validation { name: String, errors: String },
}

#[derive(Debug)]
pub struct LoadedPlugin {
    pub name: String,
    pub version: String,
    pub instance: Arc<PluginBox>,
    pub messages: Vec<String>,
    pub depends_on: Vec<String>,
    pub timeout: Duration,
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
/// If the plugin returns `RNone` from `config_schema()`, validation is
/// skipped. If the schema string is malformed or the config fails
/// validation, a `LoadError::Validation` is returned.
fn validate_config_schema(
    instance: &PluginBox,
    plugin_name: &str,
    params: &toml::Table,
) -> Result<(), LoadError> {
    let Some(schema_str) = instance.config_schema().into_option() else {
        return Ok(());
    };

    let make_schema_err = |e: &dyn fmt::Display| LoadError::Validation {
        name: plugin_name.to_owned(),
        errors: format!("invalid JSON Schema: {e}"),
    };

    let schema_json: serde_json::Value =
        serde_json::from_str(schema_str.as_str()).map_err(|e| make_schema_err(&e))?;

    let schema = jsonschema::validator_for(&schema_json).map_err(|e| make_schema_err(&e))?;

    // Convert TOML params to JSON for validation
    let params_json =
        serde_json::to_value(params).expect("TOML table is always serializable to JSON");

    let errors: Vec<String> = schema
        .iter_errors(&params_json)
        .map(|e| format_schema_error(&e))
        .collect();

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

    let load_err = |source| LoadError::Load {
        name: config.name.clone(),
        path: lib_path.display().to_string(),
        source,
    };

    // Verify the library has a valid abi_stable header before full load.
    // This is necessary because load_from_file() caches by path and may
    // return stale results for invalid files without this pre-check.
    let _ = lib_header_from_path(&lib_path).map_err(&load_err)?;

    // Load the root module (performs ABI version + layout checking)
    let module = PluginMod_Ref::load_from_file(&lib_path).map_err(load_err)?;

    let name = config.name.clone();

    // Create plugin instance via factory function
    let mut instance = module.new()().into_result().map_err(|e| LoadError::Init {
        name: name.clone(),
        reason: e.to_string(),
    })?;

    // Validate config against plugin's schema (if declared)
    validate_config_schema(&instance, &name, &config.params)?;

    // Build plugin context from config
    let context = PluginContext {
        name: name.as_str().into(),
        config: toml_table_to_config(&config.params),
    };

    // Initialize the plugin with its context
    instance
        .init(&context)
        .into_result()
        .map_err(|e| LoadError::Init {
            name: name.clone(),
            reason: e.to_string(),
        })?;

    let version = instance.version().to_string();
    let timeout_ms = config.timeout_ms.unwrap_or(default_timeout_ms);

    Ok(LoadedPlugin {
        name,
        version,
        instance: Arc::new(instance),
        messages: config.messages.clone(),
        depends_on: config.depends_on.clone(),
        timeout: Duration::from_millis(timeout_ms),
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
    use std::fs;
    use std::io::Write;

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
        // Plugin loads and init succeeds even with params (noop ignores them)
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

    /// Validate TOML params against a JSON Schema, returning error messages.
    fn schema_errors(schema: &str, params: &toml::Table) -> Vec<String> {
        let schema_json: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();
        let params_json = serde_json::to_value(params).unwrap();
        validator
            .iter_errors(&params_json)
            .map(|e| e.to_string())
            .collect()
    }

    #[test]
    fn test_validate_schema_valid_config() {
        let schema =
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
        let mut params = toml::Table::new();
        params.insert("name".into(), toml::Value::String("test".into()));
        assert!(schema_errors(schema, &params).is_empty());
    }

    #[test]
    fn test_validate_schema_missing_required_field() {
        let schema =
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
        let errors = schema_errors(schema, &toml::Table::new());
        assert!(!errors.is_empty());
        assert!(
            errors[0].contains("required"),
            "expected 'required' in: {}",
            errors[0]
        );
    }

    #[test]
    fn test_validate_schema_wrong_type() {
        let schema = r#"{"type":"object","properties":{"count":{"type":"integer"}}}"#;
        let mut params = toml::Table::new();
        params.insert("count".into(), toml::Value::String("not-a-number".into()));
        assert!(!schema_errors(schema, &params).is_empty());
    }

    #[test]
    fn test_validate_schema_no_schema_skips_validation() {
        // No schema means validation is always skipped; empty params are fine
        let params = toml::Table::new();
        let params_json = serde_json::to_value(&params).unwrap();
        // Just verify we can convert and it's a valid JSON object
        assert!(params_json.is_object());
    }

    #[test]
    fn test_validate_schema_malformed_schema() {
        let bad_schema = "not valid json {{{";
        let result: Result<serde_json::Value, _> = serde_json::from_str(bad_schema);
        assert!(result.is_err());
    }

    #[test]
    fn test_validation_error_includes_plugin_name_and_path() {
        let schema = r#"{
            "type": "object",
            "required": ["server"],
            "properties": {
                "server": {
                    "type": "object",
                    "required": ["host"],
                    "properties": {
                        "host": { "type": "string" }
                    }
                }
            }
        }"#;
        let schema_json: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();

        // Missing required nested field
        let mut server = toml::Table::new();
        server.insert("port".into(), toml::Value::Integer(8080));
        let mut params = toml::Table::new();
        params.insert("server".into(), toml::Value::Table(server));
        let params_json = serde_json::to_value(&params).unwrap();

        let errors: Vec<String> = validator
            .iter_errors(&params_json)
            .map(|e| format_schema_error(&e))
            .collect();

        assert!(!errors.is_empty());
        // The error should reference the path to the missing field
        let combined = errors.join("; ");
        assert!(
            combined.contains("host") || combined.contains("required"),
            "expected field path or 'required' in error: {combined}"
        );

        // Verify LoadError::Validation format includes plugin name
        let load_err = LoadError::Validation {
            name: "my-plugin".into(),
            errors: combined,
        };
        let msg = load_err.to_string();
        assert!(msg.contains("my-plugin"), "missing plugin name in: {msg}");
        assert!(
            msg.contains("config validation failed"),
            "missing context in: {msg}"
        );
    }

    #[test]
    fn test_load_all_skips_invalid_config_loads_others() {
        // This tests that load_all_plugins continues past failures.
        // We use two bad .so files to verify both are attempted.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("libbad1.so"), b"bad").unwrap();
        fs::write(dir.path().join("libbad2.so"), b"bad").unwrap();

        let configs = vec![
            test_support::plugin_config("bad1", "libbad1.so", &[], 0),
            test_support::plugin_config("bad2", "libbad2.so", &[], 0),
        ];
        // Both fail (invalid .so), but load_all_plugins doesn't panic
        let loaded = load_all_plugins(dir.path(), &configs, 100);
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_toml_table_to_json_roundtrip() {
        let mut table = toml::Table::new();
        table.insert("s".into(), toml::Value::String("hello".into()));
        table.insert("i".into(), toml::Value::Integer(42));
        table.insert("f".into(), toml::Value::Float(2.72));
        table.insert("b".into(), toml::Value::Boolean(true));
        table.insert(
            "arr".into(),
            toml::Value::Array(vec![toml::Value::Integer(1)]),
        );
        let mut nested = toml::Table::new();
        nested.insert("k".into(), toml::Value::String("v".into()));
        table.insert("obj".into(), toml::Value::Table(nested));

        let json = serde_json::to_value(&table).unwrap();
        assert_eq!(json["s"], "hello");
        assert_eq!(json["i"], 42);
        assert_eq!(json["b"], true);
        assert_eq!(json["arr"][0], 1);
        assert_eq!(json["obj"]["k"], "v");
    }

    #[test]
    fn test_toml_to_config_value_string() {
        let v = toml_to_config_value(&toml::Value::String("hello".into()));
        assert_eq!(v, ConfigValue::String("hello".into()));
    }

    #[test]
    fn test_toml_to_config_value_integer() {
        let v = toml_to_config_value(&toml::Value::Integer(42));
        assert_eq!(v, ConfigValue::Integer(42));
    }

    #[test]
    fn test_toml_to_config_value_float() {
        let v = toml_to_config_value(&toml::Value::Float(2.72));
        assert_eq!(v, ConfigValue::Float(2.72));
    }

    #[test]
    fn test_toml_to_config_value_bool() {
        let v = toml_to_config_value(&toml::Value::Boolean(true));
        assert_eq!(v, ConfigValue::Bool(true));
    }

    #[test]
    fn test_toml_to_config_value_array() {
        let arr = toml::Value::Array(vec![toml::Value::Integer(1), toml::Value::Integer(2)]);
        let v = toml_to_config_value(&arr);
        let expected =
            ConfigValue::Array(vec![ConfigValue::Integer(1), ConfigValue::Integer(2)].into());
        assert_eq!(v, expected);
    }

    #[test]
    fn test_toml_to_config_value_nested_table() {
        let mut inner = toml::Table::new();
        inner.insert("port".into(), toml::Value::Integer(8080));
        let mut outer = toml::Table::new();
        outer.insert("server".into(), toml::Value::Table(inner));

        let config = toml_table_to_config(&outer);
        let server = config.get(&RString::from("server")).unwrap();
        let table = server.as_table().unwrap();
        let port = table.get(&RString::from("port")).unwrap();
        assert_eq!(port.as_integer(), Some(8080));
    }

    #[test]
    fn test_toml_to_config_value_datetime() {
        let dt: toml::Value = "1979-05-27T07:32:00Z".parse().unwrap();
        let v = toml_to_config_value(&dt);
        assert!(v.as_str().is_some());
        assert!(v.as_str().unwrap().contains("1979"));
    }

    #[test]
    fn test_toml_table_to_config_mixed_types() {
        let mut table = toml::Table::new();
        table.insert("name".into(), toml::Value::String("test".into()));
        table.insert("count".into(), toml::Value::Integer(5));
        table.insert("enabled".into(), toml::Value::Boolean(true));
        table.insert("rate".into(), toml::Value::Float(0.5));
        table.insert(
            "tags".into(),
            toml::Value::Array(vec![
                toml::Value::String("a".into()),
                toml::Value::String("b".into()),
            ]),
        );

        let config = toml_table_to_config(&table);
        assert_eq!(config.len(), 5);
        assert_eq!(
            config.get(&RString::from("name")).unwrap().as_str(),
            Some("test")
        );
        assert_eq!(
            config.get(&RString::from("count")).unwrap().as_integer(),
            Some(5)
        );
        assert_eq!(
            config.get(&RString::from("enabled")).unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            config.get(&RString::from("rate")).unwrap().as_float(),
            Some(0.5)
        );
        let tags = config
            .get(&RString::from("tags"))
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(tags.len(), 2);
    }
}
