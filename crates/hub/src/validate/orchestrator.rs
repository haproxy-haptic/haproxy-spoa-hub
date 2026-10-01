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
use crate::plugin_loader;

use super::protocol::{
    FileKind, PROTOCOL_VERSION, ValidateRequest, ValidateResponse, WireDiagnostic,
};

/// Map of plugin name → loaded plugin instance. Built once at
/// validator-mode startup and shared by all incoming requests.
pub type PluginMap = HashMap<String, Arc<Plugin>>;

/// Config key under which a plugin's `validate()` receives the request's
/// [`FileKind::Data`] files, as a table of path → content.
///
/// Injected into the `PluginContext` AFTER the JSON-Schema check, never into
/// the TOML subtree: `config_schema()` describes the operator-facing params,
/// and a schema with `additionalProperties: false` would otherwise reject its
/// own validator. Any operator-supplied key of this name is dropped first, so
/// a config cannot forge the file set its own rules are checked against.
pub const STAGED_FILES_KEY: &str = "__staged_files";

/// The request's data files as a plugin-visible table.
type StagedFiles = RHashMap<RString, ConfigValue>;

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

    // Data files are collected first, so a config file dispatched in this
    // request can be checked against every file it references regardless of
    // the order they arrived in.
    let staged = collect_staged_files(request);

    for file in &request.files {
        if file.kind == FileKind::Data {
            continue;
        }
        validate_file(
            &file.path,
            &file.content,
            plugins,
            &staged,
            &mut warnings,
            &mut errors,
        );
    }

    ValidateResponse::from_diagnostics(warnings, errors)
}

/// Build the plugin-visible table of the request's data files, keyed by the
/// path the validated config will reference them by.
///
/// When the request declares a `staged_root`, each file is keyed by that root
/// joined to its wire path — the spelling the running process would use — and
/// NOT additionally by its wire path. One unambiguous key per file keeps a
/// plugin from resolving a path the runtime never would, and avoids carrying a
/// second copy of every rule set.
fn collect_staged_files(request: &ValidateRequest) -> StagedFiles {
    let mut staged = RHashMap::new();
    for file in &request.files {
        if file.kind == FileKind::Data {
            staged.insert(
                RString::from(staged_key(request.staged_root.as_deref(), &file.path)),
                ConfigValue::String(RString::from(file.content.as_str())),
            );
        }
    }
    staged
}

/// Join a data file's wire path onto the declared root.
///
/// An already-absolute path is left alone: the sender has given the runtime
/// spelling directly, and rooting it again would produce a path nothing
/// references.
fn staged_key(root: Option<&str>, path: &str) -> String {
    let Some(root) = root.filter(|r| !r.is_empty()) else {
        return path.to_owned();
    };
    if path.starts_with('/') {
        return path.to_owned();
    }
    format!("{}/{}", root.trim_end_matches('/'), path)
}

/// Validate one file's content. TOML parse failures emit a single
/// error diagnostic; otherwise we walk the `[[plugins]]` array and
/// dispatch each entry's `params` sub-table to the matching plugin.
fn validate_file(
    path: &str,
    content: &str,
    plugins: &PluginMap,
    staged: &StagedFiles,
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

    walk_plugins_array(path, entries, plugins, staged, warnings, errors);
}

/// Borrowed reference to a single dispatchable plugin entry,
/// captured during phase 1 of [`walk_plugins_array`] for fan-out in
/// phase 2.
struct DispatchTask<'a> {
    path: &'a str,
    plugin_name: &'a str,
    instance: &'a Plugin,
    params: &'a toml::Table,
}

/// Validate plugin entries with bounded parallelism and deterministic diagnostic order.
fn walk_plugins_array(
    path: &str,
    entries: &[toml::Value],
    plugins: &PluginMap,
    staged: &StagedFiles,
    warnings: &mut Vec<WireDiagnostic>,
    errors: &mut Vec<WireDiagnostic>,
) {
    let empty_table = toml::Table::new();
    let mut tasks: Vec<DispatchTask<'_>> = Vec::with_capacity(entries.len());

    // Phase 1: structural checks fill `errors` and populate `tasks`.
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

        tasks.push(DispatchTask {
            path,
            plugin_name,
            instance,
            params,
        });
    }

    if tasks.is_empty() {
        return;
    }

    dispatch_batches(&tasks, staged, warnings, errors);
}

fn dispatch_batches(
    tasks: &[DispatchTask<'_>],
    staged: &StagedFiles,
    warnings: &mut Vec<WireDiagnostic>,
    errors: &mut Vec<WireDiagnostic>,
) {
    for batch in tasks.chunks(2) {
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|task| {
                    scope.spawn(move || {
                        dispatch_to_plugin(
                            task.path,
                            task.plugin_name,
                            task.instance,
                            task.params,
                            staged,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("scoped dispatch thread panicked"))
                .collect::<Vec<_>>()
        });
        for (mut batch_warnings, mut batch_errors) in results {
            warnings.append(&mut batch_warnings);
            errors.append(&mut batch_errors);
        }
    }
}

/// Build a [`PluginContext`] from a TOML subtree and call the plugin's
/// `validate()`, with panic isolation. Returns `(warnings, errors)`
/// so the caller can fan out across plugins and merge results in
/// input order without sharing `&mut Vec` accumulators across
/// threads. Diagnostics get the path attached.
fn dispatch_to_plugin(
    path: &str,
    plugin_name: &str,
    instance: &Plugin,
    subtree: &toml::Table,
    staged: &StagedFiles,
) -> (Vec<WireDiagnostic>, Vec<WireDiagnostic>) {
    let mut warnings: Vec<WireDiagnostic> = Vec::new();
    let mut errors: Vec<WireDiagnostic> = Vec::new();

    // Validate against the plugin's JSON Schema BEFORE calling
    // `plugin.validate()`. This makes the contract symmetric with the
    // load-time path (see `plugin_loader::load_plugin`): both paths
    // schema-check first, then call `validate()` with a context the
    // plugin can trust to satisfy `config_schema()`. Plugin authors
    // writing `validate()` overrides therefore only need semantic
    // validation (e.g. compiling SecLang directives via Coraza) and
    // never structural validation. See `crates/plugin-api/src/lib.rs`
    // (the `define_plugin!` docstring on the `validate` arm) for the
    // explicit contract.
    //
    // This was the latent gap that let the coraza plugin's
    // schema-shape drift in v0.4.0 / v0.4.1 (validator read
    // `applications` as an array, runtime read it as a table-keyed-by-
    // name) sneak past admission-time validation: `validate()` was
    // called with a `PluginContext` whose structure didn't match
    // `config_schema()`, the plugin's parallel parser silently saw
    // zero entries, the broken TOML reached the data plane, and the
    // runtime hub then refused to reload. Schema-checking first
    // closes that gap centrally.
    match plugin_loader::collect_schema_errors(instance, subtree) {
        Ok(schema_errors) if !schema_errors.is_empty() => {
            for msg in schema_errors {
                errors.push(WireDiagnostic {
                    path: path.to_owned(),
                    line: 0,
                    column: 0,
                    message: msg,
                });
            }
            return (warnings, errors);
        }
        Err(meta_error) => {
            // The plugin's own config_schema() string is invalid
            // JSON Schema — a plugin bug, not an operator config
            // issue. Surface it so plugin authors see it, but don't
            // attempt to call validate() with no schema validation
            // (the structure could be anything).
            errors.push(WireDiagnostic {
                path: path.to_owned(),
                line: 0,
                column: 0,
                message: format!(
                    "plugin '{plugin_name}' returned invalid config_schema(): {meta_error}"
                ),
            });
            return (warnings, errors);
        }
        Ok(_) => {}
    }

    let context = PluginContext {
        name: plugin_name.into(),
        config: plugin_config_with_staged(subtree, staged),
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

    (warnings, errors)
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

/// Build the config a plugin's `validate()` sees: its own params plus the
/// request's data files under [`STAGED_FILES_KEY`].
///
/// The key is removed before it is inserted, unconditionally. An operator
/// config that set it itself would otherwise choose the file set its own rules
/// are validated against — which is the one input a validator must own.
fn plugin_config_with_staged(
    subtree: &toml::Table,
    staged: &StagedFiles,
) -> RHashMap<RString, ConfigValue> {
    let mut config = toml_table_to_rhashmap(subtree);
    config.insert(RString::from("__validation_only"), ConfigValue::Bool(true));
    config.remove(&RString::from(STAGED_FILES_KEY));
    if !staged.is_empty() {
        config.insert(
            RString::from(STAGED_FILES_KEY),
            ConfigValue::Table(staged.clone()),
        );
    }
    config
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
            staged_root: None,
        }
    }

    #[test]
    fn rejects_unknown_protocol_version() {
        let mut req = empty_request_with(vec![RequestFile {
            path: "x".into(),
            content: String::new(),
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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
            kind: FileKind::Config,
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

    // A data file is never parsed. Before this, every request file was run
    // through toml::from_str, so shipping a SecLang ruleset alongside the hub
    // config produced a TOML parse error and blocked admission — the reason a
    // rule set could not be validated at all.
    #[test]
    fn data_files_are_not_parsed_as_toml() {
        let req = empty_request_with(vec![RequestFile {
            path: "general/crs/REQUEST-901-INIT.conf".into(),
            content: "SecRule REQUEST_URI \"@streq /x\" \"id:1,phase:1,deny\"\n".into(),
            kind: FileKind::Data,
        }]);

        let resp = validate_request(&req, &PluginMap::new());

        assert_eq!(resp.result, ValidationResult::Valid);
        assert!(resp.errors.is_empty(), "unexpected: {:?}", resp.errors);
    }

    // The classification is explicit precisely so this keeps failing: a broken
    // hub config must never be waved through as "must have been a data file".
    #[test]
    fn a_config_file_that_is_not_toml_still_errors() {
        let req = empty_request_with(vec![RequestFile {
            path: "config.toml".into(),
            content: "SecRuleEngine On\n".into(),
            kind: FileKind::Config,
        }]);

        let resp = validate_request(&req, &PluginMap::new());

        assert_eq!(resp.result, ValidationResult::Error);
        assert!(!resp.errors.is_empty());
    }

    #[test]
    fn collects_data_files_and_skips_config_ones() {
        let req = empty_request_with(vec![
            RequestFile {
                path: "config.toml".into(),
                content: "[hub]\n".into(),
                kind: FileKind::Config,
            },
            RequestFile {
                path: "general/a.conf".into(),
                content: "AAA".into(),
                kind: FileKind::Data,
            },
            RequestFile {
                path: "general/b.data".into(),
                content: "BBB".into(),
                kind: FileKind::Data,
            },
        ]);

        let staged = collect_staged_files(&req);

        assert_eq!(staged.len(), 2);
        assert_eq!(
            staged
                .get(&RString::from("general/a.conf"))
                .and_then(ConfigValue::as_str),
            Some("AAA")
        );
        assert!(staged.get(&RString::from("config.toml")).is_none());
    }

    // Order must not matter: a data file arriving after the config that
    // references it still has to be available when that config dispatches.
    #[test]
    fn data_files_are_collected_before_dispatch_regardless_of_order() {
        let req = empty_request_with(vec![
            RequestFile {
                path: "config.toml".into(),
                content: "[hub]\n".into(),
                kind: FileKind::Config,
            },
            RequestFile {
                path: "z-last.conf".into(),
                content: "Z".into(),
                kind: FileKind::Data,
            },
        ]);

        assert_eq!(collect_staged_files(&req).len(), 1);
    }

    fn staged_with(entries: &[(&str, &str)]) -> StagedFiles {
        let mut m = RHashMap::new();
        for (k, v) in entries {
            m.insert(RString::from(*k), ConfigValue::String(RString::from(*v)));
        }
        m
    }

    #[test]
    fn dedicated_validation_mode_overrides_operator_parameters() {
        for params in [
            "",
            "__validation_only = false",
            "__validation_only = 'false'",
        ] {
            let config = plugin_config_with_staged(&params.parse().unwrap(), &StagedFiles::new());
            assert!(matches!(
                config.get(&RString::from("__validation_only")),
                Some(ConfigValue::Bool(true))
            ));
        }
    }

    #[test]
    fn no_staged_files_means_no_injected_key() {
        let subtree: toml::Table = "directives = \"SecRuleEngine On\"".parse().unwrap();

        let config = plugin_config_with_staged(&subtree, &StagedFiles::new());

        assert!(config.get(&RString::from(STAGED_FILES_KEY)).is_none());
        assert!(config.get(&RString::from("directives")).is_some());
    }

    #[test]
    fn staged_files_reach_the_plugin_config() {
        let subtree: toml::Table = "directives = \"SecRuleEngine On\"".parse().unwrap();
        let staged = staged_with(&[("/etc/haproxy/general/crs/a.conf", "SecAction id:1")]);

        let config = plugin_config_with_staged(&subtree, &staged);

        let Some(ConfigValue::Table(files)) = config.get(&RString::from(STAGED_FILES_KEY)) else {
            panic!("staged files missing from plugin config");
        };
        assert_eq!(
            files
                .get(&RString::from("/etc/haproxy/general/crs/a.conf"))
                .and_then(ConfigValue::as_str),
            Some("SecAction id:1")
        );
        // The plugin's own params survive alongside it.
        assert!(config.get(&RString::from("directives")).is_some());
    }

    // A config cannot hand itself the file set it is checked against.
    #[test]
    fn operator_supplied_staged_key_is_dropped() {
        let subtree: toml::Table =
            format!("{STAGED_FILES_KEY} = {{ \"/evil.conf\" = \"SecRuleEngine Off\" }}")
                .parse()
                .unwrap();

        let without = plugin_config_with_staged(&subtree, &StagedFiles::new());
        assert!(
            without.get(&RString::from(STAGED_FILES_KEY)).is_none(),
            "a forged key must not survive when the request staged nothing"
        );

        let with =
            plugin_config_with_staged(&subtree, &staged_with(&[("/real.conf", "SecAction id:1")]));
        let Some(ConfigValue::Table(files)) = with.get(&RString::from(STAGED_FILES_KEY)) else {
            panic!("staged files missing");
        };
        assert!(files.get(&RString::from("/evil.conf")).is_none());
        assert!(files.get(&RString::from("/real.conf")).is_some());
    }

    fn data_request(root: Option<&str>, path: &str) -> ValidateRequest {
        ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: path.to_owned(),
                content: "SecAction id:1".into(),
                kind: FileKind::Data,
            }],
            staged_root: root.map(str::to_owned),
        }
    }

    // The config references its files by the path the running process will
    // see. Keying them by the sender's own identifier would leave every such
    // reference unresolvable.
    #[test]
    fn staged_root_is_joined_onto_data_file_paths() {
        let staged =
            collect_staged_files(&data_request(Some("/etc/haproxy"), "general/crs-a.conf"));

        assert!(
            staged
                .get(&RString::from("/etc/haproxy/general/crs-a.conf"))
                .is_some(),
            "expected the runtime spelling, got {:?}",
            staged.keys().collect::<Vec<_>>()
        );
        // Deliberately NOT also under the wire path: one key per file keeps a
        // plugin from resolving a path the runtime never would, and avoids a
        // second copy of every rule set in the request.
        assert!(staged.get(&RString::from("general/crs-a.conf")).is_none());
    }

    #[test]
    fn absent_root_leaves_paths_untouched() {
        let staged = collect_staged_files(&data_request(None, "general/crs-a.conf"));
        assert!(staged.get(&RString::from("general/crs-a.conf")).is_some());
    }

    // An absolute path is already the runtime spelling. Rooting it again would
    // produce a path nothing references.
    #[test]
    fn absolute_paths_are_not_rooted_twice() {
        let staged = collect_staged_files(&data_request(
            Some("/etc/haproxy"),
            "/etc/haproxy/general/crs-a.conf",
        ));
        assert!(
            staged
                .get(&RString::from("/etc/haproxy/general/crs-a.conf"))
                .is_some(),
            "got {:?}",
            staged.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn trailing_slash_on_the_root_does_not_double_up() {
        let staged = collect_staged_files(&data_request(Some("/etc/haproxy/"), "general/a.conf"));
        assert!(
            staged
                .get(&RString::from("/etc/haproxy/general/a.conf"))
                .is_some()
        );
    }

    #[test]
    fn empty_root_behaves_as_absent() {
        let staged = collect_staged_files(&data_request(Some(""), "general/a.conf"));
        assert!(staged.get(&RString::from("general/a.conf")).is_some());
    }

    // The JSON-Schema check must run against the operator's TOML, never
    // against the context the validator injects into. Both coraza and mirror
    // declare `additionalProperties: false`, so a refactor that reversed this
    // would have every plugin reject its own valid config — and the diagnostic
    // would point at the operator's params rather than at the hub.
    #[test]
    fn injected_key_is_absent_from_the_schema_checked_subtree() {
        let subtree: toml::Table = "directives = \"SecRuleEngine On\"".parse().unwrap();

        // What the schema sees.
        assert!(subtree.get(STAGED_FILES_KEY).is_none());

        // What the plugin sees, built only after the schema check.
        let staged = staged_with(&[("/etc/haproxy/general/a.conf", "SecAction id:1")]);
        let config = plugin_config_with_staged(&subtree, &staged);
        assert!(config.get(&RString::from(STAGED_FILES_KEY)).is_some());
    }
}
