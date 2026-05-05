//! Example plugin that exercises both optional methods of `define_plugin!`:
//! `config_schema` (added in spec 002) and `validate` (added in spec 004).
//!
//! Beyond serving as a reference for plugin authors, this crate's
//! existence in the workspace ensures CI catches regressions in the
//! macro's optional-arm expansion. Without an in-repo plugin using
//! these methods, a macro bug (e.g., a `$()?` metavariable that fails
//! to be referenced inside the expansion) would build clean and only
//! be discovered by external plugin authors.

#![allow(non_camel_case_types, non_local_definitions)]

use haproxy_spoa_hub_plugin_api::{
    Diagnostic, ProcessingResult, SpoeValue, TxnVariable, define_plugin,
};

#[derive(Debug)]
struct ValidatingNoop;

define_plugin!(ValidatingNoop, {
    fn new() -> Self {
        ValidatingNoop
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn name(&self) -> &str {
        "validating-noop"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        _message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessingResult::single(TxnVariable::session(
            "result",
            SpoeValue::String("ok".into()),
        )))
    }

    fn config_schema(&self) -> Option<&str> {
        // Permissive schema — accepts any object — but documents the
        // optional `sleep_ms` knob the parallel-dispatch test uses to
        // inject deterministic per-plugin latency. Other keys
        // (`make_me_warn`, `make_me_fail`) are silently allowed by
        // the `additionalProperties: true` default.
        Some(
            r#"{
              "type": "object",
              "properties": {
                "sleep_ms": { "type": "integer", "minimum": 0 }
              }
            }"#,
        )
    }

    fn validate(&self, ctx: &PluginContext) -> Vec<Diagnostic> {
        // Demonstration: emit a warning if the operator set the magic
        // key `make_me_warn = true`, an error if `make_me_fail = true`.
        // Otherwise no diagnostics. Real plugins would do meaningful
        // structural / semantic checks here (parse SecLang, validate
        // OIDC discovery URL, etc.).
        //
        // Optional `sleep_ms` (used by the parallel-dispatch test):
        // sleep this many milliseconds before returning. Lets the
        // test inject deterministic per-plugin latency without
        // depending on coraza or any other heavy plugin. Production
        // plugin authors should NOT mirror this; it exists purely as
        // a wall-clock control knob for the orchestrator's
        // fan-out test.
        if let Some(v) = ctx.get_config("sleep_ms") {
            if let Some(ms) = v.as_integer() {
                if let Ok(ms_u64) = u64::try_from(ms) {
                    if ms_u64 > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(ms_u64));
                    }
                }
            }
        }

        let mut diagnostics = Vec::new();
        if let Some(v) = ctx.get_config("make_me_warn") {
            if v.as_bool() == Some(true) {
                diagnostics.push(Diagnostic::warning(0, 0, "warn flag was set"));
            }
        }
        if let Some(v) = ctx.get_config("make_me_fail") {
            if v.as_bool() == Some(true) {
                diagnostics.push(Diagnostic::error(0, 0, "fail flag was set"));
            }
        }
        diagnostics
    }
});

#[cfg(test)]
mod regression_tests {
    //! Regression tests for the `define_plugin!` macro's optional
    //! `config_schema` and `validate` arms.
    //!
    //! These tests exist because the macro's optional-arm expansion
    //! historically had a bug (the helper `__plugin_present!` matched
    //! `()` but was invoked with a block) that compiled clean against
    //! plugins that used neither optional method. By exercising both
    //! optional methods in a real in-repo plugin AND asserting their
    //! behaviour at runtime, we catch the bug immediately on any
    //! macro-related change.

    use super::*;
    use haproxy_spoa_hub_plugin_api::{
        ConfigValue, DiagnosticSeverity, PluginContext, RHashMap, RString,
    };

    fn ctx_with(entries: &[(&str, ConfigValue)]) -> PluginContext {
        let mut config = RHashMap::new();
        for (k, v) in entries {
            config.insert(RString::from(*k), v.clone());
        }
        PluginContext {
            name: RString::from("validating-noop"),
            config,
        }
    }

    #[test]
    fn config_schema_arm_compiles_and_returns_user_value() {
        let plugin = ValidatingNoop::__new();
        let schema_str = plugin.__config_schema().expect("schema present");
        let schema_json: serde_json::Value =
            serde_json::from_str(schema_str).expect("schema parses as JSON");
        // Sanity-check the schema describes an object with the
        // optional `sleep_ms` integer property the parallel-dispatch
        // test injects. Keeps this regression test resilient to
        // whitespace changes in the schema literal.
        assert_eq!(schema_json["type"], "object");
        assert_eq!(schema_json["properties"]["sleep_ms"]["type"], "integer");
    }

    #[test]
    fn validate_arm_returns_no_diagnostics_for_neutral_config() {
        let plugin = ValidatingNoop::__new();
        let ctx = ctx_with(&[]);
        let diagnostics = plugin.__validate(&ctx);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn validate_arm_returns_warning_for_warn_flag() {
        let plugin = ValidatingNoop::__new();
        let ctx = ctx_with(&[("make_me_warn", ConfigValue::Bool(true))]);
        let diagnostics = plugin.__validate(&ctx);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Warning);
        assert_eq!(diagnostics[0].message.as_str(), "warn flag was set");
    }

    #[test]
    fn validate_arm_returns_error_for_fail_flag() {
        let plugin = ValidatingNoop::__new();
        let ctx = ctx_with(&[("make_me_fail", ConfigValue::Bool(true))]);
        let diagnostics = plugin.__validate(&ctx);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].message.as_str(), "fail flag was set");
    }

    #[test]
    fn validate_arm_returns_both_when_both_flags_set() {
        let plugin = ValidatingNoop::__new();
        let ctx = ctx_with(&[
            ("make_me_warn", ConfigValue::Bool(true)),
            ("make_me_fail", ConfigValue::Bool(true)),
        ]);
        let diagnostics = plugin.__validate(&ctx);
        assert_eq!(diagnostics.len(), 2);
    }

    #[test]
    fn validate_arm_diagnostics_have_empty_path_for_hub_to_fill() {
        // The path field is hub-owned. Plugins must construct
        // diagnostics with an empty path; the hub overwrites it
        // post-hoc with file identity.
        let plugin = ValidatingNoop::__new();
        let ctx = ctx_with(&[("make_me_fail", ConfigValue::Bool(true))]);
        let diagnostics = plugin.__validate(&ctx);
        assert!(
            diagnostics[0].path.is_empty(),
            "plugin diagnostic carried non-empty path; hub-owned invariant violated"
        );
    }
}
