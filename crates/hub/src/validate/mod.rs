//! Validator-mode hub: serves a unix-socket protocol that accepts
//! TOML configuration files and dispatches each plugin's
//! `[plugins.params.<name>]` subtree to that plugin's `validate()`
//! method, returning aggregated diagnostics.
//!
//! Spec: `specs/004-validate-mode/`. Wire protocol contract:
//! `specs/004-validate-mode/contracts/validate-socket-protocol.md`.
//! Plugin API delta: `specs/004-validate-mode/contracts/plugin-api-delta.md`.

pub mod framing;
pub mod loader;
pub mod orchestrator;
pub mod protocol;
pub mod server;

pub use loader::load_all_plugins_for_validation;
pub use orchestrator::{PluginMap, validate_request};
pub use protocol::{
    PROTOCOL_VERSION, ValidateRequest, ValidateResponse, ValidationResult, WireDiagnostic,
};
pub use server::{ServerConfig, serve};
