//! Wire protocol types for the validator-sidecar unix-socket interface.
//!
//! The protocol is documented in
//! `specs/004-validate-mode/contracts/validate-socket-protocol.md`.
//! This module defines the JSON serde types only; framing lives in
//! [`super::framing`] and dispatch in [`super::orchestrator`].

use serde::{Deserialize, Serialize};

/// Current wire-protocol version. Bumped when request or response
/// shapes change incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Validation request received from a client (HAPTIC's controller, an
/// operator's `curl`, etc.).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidateRequest {
    pub protocol_version: u32,
    pub files: Vec<RequestFile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RequestFile {
    /// Operator-facing file identifier echoed back in diagnostic
    /// `path` fields. The validator does NOT open this on disk; it
    /// processes `content` directly.
    pub path: String,
    /// File bytes as a UTF-8 string. For TOML files, this is the raw
    /// TOML text.
    pub content: String,
}

/// Validation response sent back to the client.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ValidateResponse {
    pub protocol_version: u32,
    pub result: ValidationResult,
    pub warnings: Vec<WireDiagnostic>,
    pub errors: Vec<WireDiagnostic>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValidationResult {
    Valid,
    Warning,
    Error,
}

/// Wire form of a diagnostic. Mirrors `plugin_api::Diagnostic` but
/// uses serde-friendly types and lowercase severity for the JSON
/// representation operators see.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WireDiagnostic {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl ValidateResponse {
    /// Build a response from collected warnings and errors. The
    /// `result` field is computed: error if `errors` is non-empty,
    /// else warning if `warnings` is non-empty, else valid.
    #[must_use]
    pub fn from_diagnostics(warnings: Vec<WireDiagnostic>, errors: Vec<WireDiagnostic>) -> Self {
        let result = if !errors.is_empty() {
            ValidationResult::Error
        } else if !warnings.is_empty() {
            ValidationResult::Warning
        } else {
            ValidationResult::Valid
        };
        Self {
            protocol_version: PROTOCOL_VERSION,
            result,
            warnings,
            errors,
        }
    }

    /// Build a response for a single protocol-level error (malformed
    /// JSON, unknown protocol version, missing required field, etc.).
    /// The error has `path = ""`, `line = 0`, `column = 0`.
    #[must_use]
    pub fn protocol_error(message: impl Into<String>) -> Self {
        Self::from_diagnostics(
            Vec::new(),
            vec![WireDiagnostic {
                path: String::new(),
                line: 0,
                column: 0,
                message: message.into(),
            }],
        )
    }
}

/// Convert a plugin-api `Diagnostic` to its wire form. The `path` is
/// supplied separately by the orchestrator (hub-owned per spec FR-002).
pub fn from_plugin_diagnostic(
    plugin: &haproxy_spoa_hub_plugin_api::Diagnostic,
    path: impl Into<String>,
) -> WireDiagnostic {
    WireDiagnostic {
        path: path.into(),
        line: plugin.line,
        column: plugin.column,
        message: plugin.message.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_computed_from_diagnostics() {
        // No diagnostics → valid
        let r = ValidateResponse::from_diagnostics(Vec::new(), Vec::new());
        assert_eq!(r.result, ValidationResult::Valid);
        assert_eq!(r.protocol_version, PROTOCOL_VERSION);
        assert!(r.warnings.is_empty());
        assert!(r.errors.is_empty());

        // Only warnings → warning
        let r = ValidateResponse::from_diagnostics(
            vec![WireDiagnostic {
                path: "x".into(),
                line: 1,
                column: 0,
                message: "w".into(),
            }],
            Vec::new(),
        );
        assert_eq!(r.result, ValidationResult::Warning);

        // Errors take precedence over warnings
        let r = ValidateResponse::from_diagnostics(
            vec![WireDiagnostic {
                path: "x".into(),
                line: 1,
                column: 0,
                message: "w".into(),
            }],
            vec![WireDiagnostic {
                path: "x".into(),
                line: 2,
                column: 0,
                message: "e".into(),
            }],
        );
        assert_eq!(r.result, ValidationResult::Error);
    }

    #[test]
    fn protocol_error_produces_single_error_diagnostic() {
        let r = ValidateResponse::protocol_error("bad");
        assert_eq!(r.result, ValidationResult::Error);
        assert!(r.warnings.is_empty());
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].path, "");
        assert_eq!(r.errors[0].line, 0);
        assert_eq!(r.errors[0].column, 0);
        assert_eq!(r.errors[0].message, "bad");
    }

    #[test]
    fn round_trips_via_json() {
        let original = ValidateResponse::from_diagnostics(
            vec![WireDiagnostic {
                path: "config.toml".into(),
                line: 5,
                column: 0,
                message: "warning text".into(),
            }],
            vec![WireDiagnostic {
                path: "config.toml".into(),
                line: 10,
                column: 3,
                message: "error text".into(),
            }],
        );
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);

        // Sanity: lowercase severity in JSON
        assert!(json.contains(r#""result":"error""#));
    }

    #[test]
    fn request_round_trips() {
        let req = ValidateRequest {
            protocol_version: 1,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: "[hub]\n".into(),
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ValidateRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.protocol_version, 1);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].path, "config.toml");
    }

    #[test]
    fn from_plugin_diagnostic_carries_position_and_message() {
        let pd = haproxy_spoa_hub_plugin_api::Diagnostic::error(42, 7, "unknown directive");
        let wire = from_plugin_diagnostic(&pd, "config.toml");
        assert_eq!(wire.path, "config.toml");
        assert_eq!(wire.line, 42);
        assert_eq!(wire.column, 7);
        assert_eq!(wire.message, "unknown directive");
    }
}
