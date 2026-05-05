//! Validation orchestrator.
//!
//! Takes a parsed `ValidateRequest`, walks each file's `[[plugins]]`
//! array, and dispatches each entry's `params` sub-table to the
//! matching loaded plugin's `validate()` method, aggregating
//! diagnostics into a `ValidateResponse`. Each plugin's `validate()`
//! is wrapped in `catch_unwind` so a panic in one plugin doesn't take
//! down the validator (FR-014).
//!
//! The accepted TOML shape is the canonical hub config — the same
//! one `HubConfig` deserialises at runtime startup, so the validator
//! consumes byte-for-byte the file the runtime hub will see. HAPTIC's
//! chart-rendered `general/spoa-hub-config.toml` uses this shape.
//!
//! ```toml
//! [[plugins]]
//! name = "coraza"
//! library = "libcoraza_plugin.so"
//! [plugins.params]
//! directives = """SecRuleEngine On"""
//! ```
//!
//! Earlier drafts of this code path also accepted a flat
//! `[plugins.params.<name>]` schema. That second shape was unique to
//! the validator and never matched what `HubConfig` parses, so it was
//! removed — keeping it would have meant validator-mode TOMLs that
//! cannot be loaded by the runtime hub, which defeats the point of
//! validating with the same plugin set.

use std::{collections::HashMap, panic::AssertUnwindSafe, sync::Arc};

use abi_stable::std_types::{RHashMap, RString};
use haproxy_spoa_hub_plugin_api::{
    ConfigValue, DiagnosticSeverity as PluginSeverity, PluginContext,
};

use crate::plugin::Plugin;

use super::protocol::{PROTOCOL_VERSION, ValidateRequest, ValidateResponse, WireDiagnostic};

/// Map of plugin name → loaded plugin instance. Built once at
/// validator-mode startup and shared by all incoming requests.
pub type PluginMap = HashMap<String, Arc<Plugin>>;

/// Validate one incoming request against the loaded plugin set.
///
/// Each file is parsed as TOML; for each `[[plugins]]` entry, the
/// matching plugin's `validate()` is invoked with that entry's
/// `params` sub-table. Diagnostics are collected into the response
/// with the originating file's path attached (hub-owned, per
/// FR-002).
#[must_use]
pub fn validate_request(request: &ValidateRequest, plugins: &PluginMap) -> ValidateResponse {
    if request.protocol_version != PROTOCOL_VERSION {
        return ValidateResponse::protocol_error(format!(
            "protocol version {} not supported (max: {})",
            request.protocol_version, PROTOCOL_VERSION
        ));
    }
    if request.files.is_empty() {
        return ValidateResponse::protocol_error("'files' array must be non-empty");
    }

    let mut warnings: Vec<WireDiagnostic> = Vec::new();
    let mut errors: Vec<WireDiagnostic> = Vec::new();

    for file in &request.files {
        validate_file(
            &file.path,
            &file.content,
            plugins,
            &mut warnings,
            &mut errors,
        );
    }

    ValidateResponse::from_diagnostics(warnings, errors)
}

/// Validate one file's content. TOML parse failures emit a single
/// error diagnostic; otherwise we walk the `[[plugins]]` array and
/// dispatch each entry's `params` sub-table to the matching plugin.
fn validate_file(
    path: &str,
    content: &str,
    plugins: &PluginMap,
    warnings: &mut Vec<WireDiagnostic>,
    errors: &mut Vec<WireDiagnostic>,
) {
    let parsed: toml::Value = match toml::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            errors.push(toml_parse_diagnostic(path, &e, content));
            return;
        }
    };

    let Some(table) = parsed.as_table() else {
        errors.push(WireDiagnostic {
            path: path.to_owned(),
            line: 0,
            column: 0,
            message: "top-level TOML value must be a table".to_owned(),
        });
        return;
    };

    // Missing `plugins` is not an error — the file may not be intended
    // for plugin validation.
    let Some(plugins_value) = table.get("plugins") else {
        return;
    };

    let Some(entries) = plugins_value.as_array() else {
        errors.push(WireDiagnostic {
            path: path.to_owned(),
            line: 0,
            column: 0,
            message: format!(
                "`plugins` must be an Array of Tables (canonical hub config); got {:?}",
                plugins_value.type_str()
            ),
        });
        return;
    };

    walk_plugins_array(path, entries, plugins, warnings, errors);
}

/// Walk the `[[plugins]]` array. Each entry must be a table with a
/// `name` field; its `params` sub-table (default-empty when absent)
/// is dispatched to the plugin instance registered under `name`.
/// Entries missing `name`, carrying a non-string `name`, or naming a
/// plugin not loaded by this validator emit error diagnostics; non-
/// table array entries and non-table `params` values are flagged
/// with their actual type so operators can debug malformed configs.
fn walk_plugins_array(
    path: &str,
    entries: &[toml::Value],
    plugins: &PluginMap,
    warnings: &mut Vec<WireDiagnostic>,
    errors: &mut Vec<WireDiagnostic>,
) {
    let empty_table = toml::Table::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(entry_table) = entry.as_table() else {
            errors.push(WireDiagnostic {
                path: path.to_owned(),
                line: 0,
                column: 0,
                message: format!(
                    "[[plugins]] entry at index {index} is not a table (got {:?})",
                    entry.type_str()
                ),
            });
            continue;
        };

        let plugin_name = match entry_table.get("name") {
            Some(toml::Value::String(s)) => s.as_str(),
            Some(other) => {
                errors.push(WireDiagnostic {
                    path: path.to_owned(),
                    line: 0,
                    column: 0,
                    message: format!(
                        "[[plugins]] entry at index {index} has non-string `name` \
                         (got {:?})",
                        other.type_str()
                    ),
                });
                continue;
            }
            None => {
                errors.push(WireDiagnostic {
                    path: path.to_owned(),
                    line: 0,
                    column: 0,
                    message: format!("[[plugins]] entry at index {index} is missing `name`"),
                });
                continue;
            }
        };

        let params = match entry_table.get("params") {
            Some(toml::Value::Table(t)) => t,
            Some(other) => {
                errors.push(WireDiagnostic {
                    path: path.to_owned(),
                    line: 0,
                    column: 0,
                    message: format!(
                        "[plugins.params] for plugin '{plugin_name}' is not a table (got {:?})",
                        other.type_str()
                    ),
                });
                continue;
            }
            None => &empty_table,
        };

        let Some(instance) = plugins.get(plugin_name) else {
            errors.push(WireDiagnostic {
                path: path.to_owned(),
                line: 0,
                column: 0,
                message: format!("plugin '{plugin_name}' is not loaded by this validator"),
            });
            continue;
        };

        dispatch_to_plugin(path, plugin_name, instance, params, warnings, errors);
    }
}

/// Build a [`PluginContext`] from a TOML subtree and call the plugin's
/// `validate()`, with panic isolation. Diagnostics get the path
/// attached.
fn dispatch_to_plugin(
    path: &str,
    plugin_name: &str,
    instance: &Plugin,
    subtree: &toml::Table,
    warnings: &mut Vec<WireDiagnostic>,
    errors: &mut Vec<WireDiagnostic>,
) {
    let context = PluginContext {
        name: plugin_name.into(),
        config: toml_table_to_rhashmap(subtree),
    };

    // Panic isolation per FR-014: a plugin's validate() panicking
    // turns into a single error diagnostic; the validator continues.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| instance.validate(&context)));

    match result {
        Ok(diagnostics) => {
            for d in diagnostics {
                let wire = WireDiagnostic {
                    path: path.to_owned(),
                    line: d.line,
                    column: d.column,
                    message: d.message.as_str().to_owned(),
                };
                match d.severity {
                    PluginSeverity::Warning => warnings.push(wire),
                    PluginSeverity::Error => errors.push(wire),
                }
            }
        }
        Err(panic_payload) => {
            let panic_msg = panic_message(&panic_payload);
            errors.push(WireDiagnostic {
                path: path.to_owned(),
                line: 0,
                column: 0,
                message: format!("internal validator error in plugin '{plugin_name}': {panic_msg}"),
            });
        }
    }
}

/// Best-effort conversion of a panic payload (a `Box<dyn Any + Send>`)
/// into a string. Covers the common `&str` and `String` payload types;
/// otherwise returns a generic placeholder.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic with non-string payload".to_owned()
    }
}

/// Build a diagnostic from a TOML parse error. The toml crate's
/// `Error::span()` gives a byte range; we convert the start byte to a
/// 1-based line and column by counting newlines in the source.
fn toml_parse_diagnostic(path: &str, err: &toml::de::Error, content: &str) -> WireDiagnostic {
    let (line, column) = match err.span() {
        Some(range) => byte_offset_to_line_col(content, range.start),
        None => (0, 0),
    };
    WireDiagnostic {
        path: path.to_owned(),
        line,
        column,
        message: err.message().to_owned(),
    }
}

/// Convert a byte offset into the source string to (1-based line,
/// 1-based column). Returns (0, 0) if offset exceeds content length.
fn byte_offset_to_line_col(content: &str, offset: usize) -> (u32, u32) {
    if offset > content.len() {
        return (0, 0);
    }
    let prefix = &content[..offset];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = prefix.bytes().rev().take_while(|b| *b != b'\n').count() + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
}

/// Convert a `toml::Table` into an `RHashMap<RString, ConfigValue>`,
/// matching what `plugin_loader::load_plugin` builds for production
/// init. Duplicated rather than imported because making it pub in
/// `plugin_loader` risks changing that file's review surface
/// unnecessarily.
fn toml_table_to_rhashmap(table: &toml::Table) -> RHashMap<RString, ConfigValue> {
    table
        .iter()
        .map(|(k, v)| (k.as_str().into(), toml_to_config_value(v)))
        .collect()
}

fn toml_to_config_value(value: &toml::Value) -> ConfigValue {
    match value {
        toml::Value::String(s) => ConfigValue::String(s.as_str().into()),
        toml::Value::Integer(i) => ConfigValue::Integer(*i),
        toml::Value::Float(f) => ConfigValue::Float(*f),
        toml::Value::Boolean(b) => ConfigValue::Bool(*b),
        toml::Value::Array(arr) => {
            ConfigValue::Array(arr.iter().map(toml_to_config_value).collect())
        }
        toml::Value::Table(t) => ConfigValue::Table(toml_table_to_rhashmap(t)),
        toml::Value::Datetime(dt) => ConfigValue::String(dt.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::protocol::{RequestFile, ValidationResult};

    fn empty_request_with(files: Vec<RequestFile>) -> ValidateRequest {
        ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files,
        }
    }

    #[test]
    fn rejects_unknown_protocol_version() {
        let mut req = empty_request_with(vec![RequestFile {
            path: "x".into(),
            content: String::new(),
        }]);
        req.protocol_version = 99;
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(resp.errors[0].message.contains("protocol version 99"));
    }

    #[test]
    fn rejects_empty_files_array() {
        let req = empty_request_with(Vec::new());
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(
            resp.errors[0]
                .message
                .contains("'files' array must be non-empty")
        );
    }

    #[test]
    fn surfaces_toml_parse_error_with_line() {
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[hub\nfoo = 1".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert_eq!(resp.errors[0].path, "config.toml");
        // toml parser puts the error on line 1 (the unclosed bracket)
        assert!(resp.errors[0].line >= 1);
    }

    #[test]
    fn unknown_plugin_in_array_yields_error() {
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[[plugins]]\nname = \"absent_plugin\"\n[plugins.params]\nx = 1\n".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(resp.errors[0].message.contains("absent_plugin"));
        assert!(resp.errors[0].message.contains("not loaded"));
    }

    #[test]
    fn plugins_table_yields_schema_error() {
        // The legacy `[plugins.params.<name>]` shape is rejected: the
        // validator and the runtime hub must consume the same TOML, and
        // `HubConfig` only deserialises `[[plugins]]` arrays.
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[plugins.params.coraza]\ndirectives = \"\"\n".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(
            resp.errors[0].message.contains("Array of Tables"),
            "expected schema-shape diagnostic, got {:?}",
            resp.errors[0].message
        );
    }

    #[test]
    fn plugin_entry_missing_name_yields_error() {
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[[plugins]]\n[plugins.params]\nx = 1\n".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(resp.errors[0].message.contains("missing `name`"));
    }

    #[test]
    fn plugin_entry_with_non_string_name_yields_error() {
        // Distinct error from "missing `name`": the key exists but
        // carries the wrong type. Operators copy/pasting from another
        // file and dropping the quotes hit this; the message should
        // make the type mismatch obvious.
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[[plugins]]\nname = 42\n".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Error);
        assert_eq!(resp.errors.len(), 1);
        assert!(
            resp.errors[0].message.contains("non-string `name`"),
            "expected non-string name diagnostic, got {:?}",
            resp.errors[0].message
        );
    }

    #[test]
    fn no_plugins_section_is_valid() {
        // A TOML that doesn't reference plugins at all should be
        // valid — the validator only checks plugin params, not other
        // hub config sections.
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "[hub]\nlisten = \"0.0.0.0:9000\"\n".into(),
        }]);
        let resp = validate_request(&req, &PluginMap::new());
        assert_eq!(resp.result, ValidationResult::Valid);
        assert!(resp.errors.is_empty());
    }

    #[test]
    fn byte_offset_to_line_col_basic_cases() {
        let s = "abc\ndef\nghi";
        // Start of file
        assert_eq!(byte_offset_to_line_col(s, 0), (1, 1));
        // Middle of first line
        assert_eq!(byte_offset_to_line_col(s, 2), (1, 3));
        // Just after first newline (start of line 2)
        assert_eq!(byte_offset_to_line_col(s, 4), (2, 1));
        // Middle of second line
        assert_eq!(byte_offset_to_line_col(s, 6), (2, 3));
        // Start of third line
        assert_eq!(byte_offset_to_line_col(s, 8), (3, 1));
        // Out-of-bounds
        assert_eq!(byte_offset_to_line_col(s, 999), (0, 0));
    }
}
