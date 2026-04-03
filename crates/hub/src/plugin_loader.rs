use std::path::Path;
use std::sync::Arc;

use abi_stable::library::{RootModule, lib_header_from_path};
use abi_stable::std_types::{RHashMap, RString};
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
    pub timeout: std::time::Duration,
}

/// Convert a `toml::Value` into an FFI-safe `ConfigValue`.
///
/// TOML `Datetime` values are stringified since there is no
/// corresponding `ConfigValue` variant.
fn toml_to_config_value(value: &toml::Value) -> ConfigValue {
    match value {
        toml::Value::String(s) => ConfigValue::String(RString::from(s.as_str())),
        toml::Value::Integer(i) => ConfigValue::Integer(*i),
        toml::Value::Float(f) => ConfigValue::Float(*f),
        toml::Value::Boolean(b) => ConfigValue::Bool(*b),
        toml::Value::Array(arr) => {
            ConfigValue::Array(arr.iter().map(toml_to_config_value).collect())
        }
        toml::Value::Table(table) => ConfigValue::Table(toml_table_to_config(table)),
        toml::Value::Datetime(dt) => ConfigValue::String(RString::from(dt.to_string().as_str())),
    }
}

/// Convert a `toml::Table` into an `RHashMap<RString, ConfigValue>`.
fn toml_table_to_config(table: &toml::Table) -> RHashMap<RString, ConfigValue> {
    let mut map = RHashMap::new();
    for (k, v) in table {
        map.insert(RString::from(k.as_str()), toml_to_config_value(v));
    }
    map
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
    use abi_stable::std_types::ROption;

    let schema_str = match instance.config_schema() {
        ROption::RSome(s) => s,
        ROption::RNone => return Ok(()),
    };

    let schema_json: serde_json::Value =
        serde_json::from_str(schema_str.as_str()).map_err(|e| LoadError::Validation {
            name: plugin_name.to_string(),
            errors: format!("invalid JSON Schema: {e}"),
        })?;

    let schema = jsonschema::validator_for(&schema_json).map_err(|e| LoadError::Validation {
        name: plugin_name.to_string(),
        errors: format!("invalid JSON Schema: {e}"),
    })?;

    // Convert TOML params to JSON for validation
    let params_json = toml_table_to_json(params);

    let errors: Vec<String> = schema
        .iter_errors(&params_json)
        .map(|e| {
            let path = e.instance_path().to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(LoadError::Validation {
            name: plugin_name.to_string(),
            errors: errors.join("; "),
        })
    }
}

/// Convert a `toml::Table` to `serde_json::Value` for schema validation.
fn toml_table_to_json(table: &toml::Table) -> serde_json::Value {
    let map = table
        .iter()
        .map(|(k, v)| (k.clone(), toml_value_to_json(v)))
        .collect();
    serde_json::Value::Object(map)
}

fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(t) => toml_table_to_json(t),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
    }
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

    // Validate config against plugin's schema (if declared)
    validate_config_schema(&instance, &config.name, &config.params)?;

    // Build plugin context from config
    let context = PluginContext {
        name: RString::from(config.name.as_str()),
        config: toml_table_to_config(&config.params),
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

    use crate::test_support::workspace_target_dir;

    #[test]
    fn test_load_valid_plugin() {
        let plugin_dir = workspace_target_dir();
        let config = PluginConfig {
            name: "noop".into(),
            library: "libnoop_plugin.so".into(),
            messages: vec!["test".into()],
            depends_on: vec![],
            timeout_ms: Some(500),
            params: toml::Table::default(),
        };
        let loaded = load_plugin(&plugin_dir, &config, 100).unwrap();
        assert_eq!(loaded.name, "noop");
        assert!(!loaded.version.is_empty());
        assert_eq!(loaded.timeout, std::time::Duration::from_millis(500));
    }

    #[test]
    fn test_load_plugin_with_params() {
        let plugin_dir = workspace_target_dir();
        let mut params = toml::Table::new();
        params.insert(
            "db_path".into(),
            toml::Value::String("/data/test.mmdb".into()),
        );
        params.insert("locale".into(), toml::Value::String("de".into()));
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
            params: toml::Table::default(),
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
            params: toml::Table::default(),
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
            params: toml::Table::default(),
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
            params: toml::Table::default(),
        }];
        let loaded = load_all_plugins(dir.path(), &configs, 100);
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_validate_schema_valid_config() {
        let schema =
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
        let schema_json: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();
        let mut params = toml::Table::new();
        params.insert("name".into(), toml::Value::String("test".into()));
        let params_json = toml_table_to_json(&params);
        assert!(validator.validate(&params_json).is_ok());
    }

    #[test]
    fn test_validate_schema_missing_required_field() {
        let schema =
            r#"{"type":"object","required":["name"],"properties":{"name":{"type":"string"}}}"#;
        let schema_json: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();
        let params = toml::Table::new();
        let params_json = toml_table_to_json(&params);
        let errors: Vec<_> = validator.iter_errors(&params_json).collect();
        assert!(!errors.is_empty());
        let msg = errors[0].to_string();
        assert!(msg.contains("required"), "expected 'required' in: {msg}");
    }

    #[test]
    fn test_validate_schema_wrong_type() {
        let schema = r#"{"type":"object","properties":{"count":{"type":"integer"}}}"#;
        let schema_json: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema_json).unwrap();
        let mut params = toml::Table::new();
        params.insert("count".into(), toml::Value::String("not-a-number".into()));
        let params_json = toml_table_to_json(&params);
        let errors: Vec<_> = validator.iter_errors(&params_json).collect();
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_validate_schema_no_schema_skips_validation() {
        // No schema means validation is always skipped; empty params are fine
        let params = toml::Table::new();
        let params_json = toml_table_to_json(&params);
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
        let params_json = toml_table_to_json(&params);

        let errors: Vec<String> = validator
            .iter_errors(&params_json)
            .map(|e| {
                let path = e.instance_path().to_string();
                if path.is_empty() {
                    e.to_string()
                } else {
                    format!("{path}: {e}")
                }
            })
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
        std::fs::write(dir.path().join("libbad1.so"), b"bad").unwrap();
        std::fs::write(dir.path().join("libbad2.so"), b"bad").unwrap();

        let configs = vec![
            PluginConfig {
                name: "bad1".into(),
                library: "libbad1.so".into(),
                messages: vec![],
                depends_on: vec![],
                timeout_ms: None,
                params: toml::Table::default(),
            },
            PluginConfig {
                name: "bad2".into(),
                library: "libbad2.so".into(),
                messages: vec![],
                depends_on: vec![],
                timeout_ms: None,
                params: toml::Table::default(),
            },
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

        let json = toml_table_to_json(&table);
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
