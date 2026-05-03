# Plugin API Contract — `validate` Method Delta

**Feature**: [../spec.md](../spec.md) | **Base trait**: `crates/plugin-api/src/plugin_trait.rs`

This delta extends the existing `SpoePlugin` trait with a deep validation method. Additive, ABI-compatible with current plugins (default impl returns no diagnostics).

## Trait change

The current trait already declares `#[sabi(last_prefix_field)] fn shutdown(&self);` followed by `fn config_schema(&self) -> ROption<RString>` (added in spec 002). New methods can be appended after `config_schema` with default impls and remain ABI-stable for existing plugin .so files.

```rust
// Existing methods elided for brevity — this is the appended portion.

/// Diagnostic severity, ABI-stable.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, StableAbi)]
pub enum DiagnosticSeverity {
    Warning = 0,
    Error = 1,
}

/// One finding from a plugin's `validate()` call. Position fields are
/// 1-based; `0` indicates "unknown / file-level".
///
/// **`path` ownership:** Plugins MUST leave `path` empty (default
/// `RString::new()`) when constructing diagnostics. The hub fills it
/// in post-hoc with the file identity it knows about — in validator
/// sidecar mode that's the request's `files[].path`; in production
/// mode it's the value from the hub's `--config <path>` flag. Plugins
/// don't have access to file identity (their input is the parsed
/// `[plugins.params.<name>]` subtree, decoupled from the file it came
/// from), and asking them to track it would couple the plugin API to
/// the hub's I/O. The hub-set value is what HAPTIC and operators see.
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub path: RString,
    pub line: u32,
    pub column: u32,
    pub message: RString,
}

#[sabi_trait]
pub trait SpoePlugin: Send + Sync + core::fmt::Debug {
    // ... existing methods (init, process, name, version, shutdown, config_schema) ...

    /// Deep validation of the plugin's `[plugins.params]` subtree. Called
    /// by the hub before `init()` (production mode) and by the validator
    /// sidecar in `--validate-socket` mode. MUST be pure: no side effects,
    /// no tokio tasks, no network or file I/O beyond what the
    /// `PluginContext` carries, no global state mutation.
    ///
    /// Returns an `RVec<Diagnostic>`. Empty vec means "no findings, the
    /// configuration is valid as far as this plugin can tell." Any
    /// diagnostic with `severity = Error` blocks `init()` in production
    /// mode and yields a `result: "error"` response in validator mode.
    ///
    /// Default impl returns an empty vec — backward-compatible with
    /// every existing plugin built against the prior API. Plugins
    /// override this to surface their own structural / semantic /
    /// compile-time errors with line and column where their parser
    /// supports it.
    fn validate(&self, _context: &PluginContext) -> RVec<Diagnostic> {
        RVec::new()
    }
}
```

## Semantics for plugin authors

- **Purity is contractual.** A plugin that opens a TCP socket, fires up a tokio task, or mutates global state in `validate()` is broken — even if it works in normal use, the validator sidecar runs without those resources available and side-effects from validation calls would surface in unexpected places (e.g., the validator sidecar's container would attempt to dial OTLP collectors). Plugin tests SHOULD include an assertion that no goroutines / tasks remain after a validate call.

- **`PluginContext` is the only input.** The plugin reads its `[plugins.params]` subtree from `PluginContext` exactly as it would in `init()`. The same parsing path is used; if the plugin caches a typed config struct internally, validate produces it just like init does — only without registering it for runtime use.

- **Diagnostic positioning.** Where the underlying parser provides line/column, the plugin SHOULD populate both fields (1-based). Where only line is available (Coraza's SecLang parser via captured logger), set `column = 0`. For purely file-level diagnostics ("this plugin requires field X to be set"), use `line = 0, column = 0`.

- **Diagnostic count.** A single validation call may return zero, one, or many diagnostics. A plugin SHOULD report all problems it finds rather than failing at the first one — this gives operators a complete picture per `kubectl apply`. However, plugins that genuinely cannot continue past the first error (e.g., TOML parse failure inside the plugin) MAY return a single diagnostic and stop.

- **Path field is hub-owned, not plugin-owned.** Plugins MUST leave `path` empty when constructing diagnostics. The hub overwrites it post-hoc with the file identity from its surrounding context (request file path in validator-sidecar mode, `--config` value in production mode). Plugin authors do not need to track file identity — their input is the parsed config subtree, not the file it came from.

## Hub-side orchestration (production mode)

The hub's startup orchestrator MUST run **all** plugins' `validate()` first, aggregate diagnostics across plugins, and only then decide whether to proceed to `init()`. Fail-fast on the first error would force operators into an iterative fix-redeploy-fix-redeploy loop when multiple plugins have config errors; instead, every error gets surfaced in a single startup attempt:

```rust
// Pseudo-code, not literal.
let mut all_warnings = Vec::new();
let mut all_errors = Vec::new();
for plugin in &plugins {
    let diagnostics = plugin.validate(context);
    let path = hub_config_path();  // hub fills path post-hoc
    for mut d in diagnostics {
        d.path = path.clone();
        match d.severity {
            Warning => all_warnings.push(d),
            Error => all_errors.push(d),
        }
    }
}
for w in &all_warnings { log::warn!(...) }
if !all_errors.is_empty() {
    for e in &all_errors { log::error!(...) }
    return Err(StartupError::PluginValidationFailed { errors: all_errors });
}
// Only when every plugin's validate() returned no errors, run init() for each.
for plugin in &mut plugins {
    plugin.init(context)?;
}
```

This wraps `init` in "validate-all-first" without changing the `init` signature. Plugins that override `validate` get sharper startup errors for free; plugins that use the default get the same behaviour as today. The validator sidecar uses the same aggregation pattern (FR-012), so production-mode and sidecar-mode error reporting are symmetric.

## Backward compatibility

- Existing plugin .so files (built against prior `plugin-api` versions) load unchanged. The default `validate()` impl returns an empty vec, which the hub treats as "no findings", so `init()` runs as before.
- Plugins that opt in to validation override the method and gain line/column-precise startup errors plus validator-sidecar coverage.
- The new `Diagnostic` and `DiagnosticSeverity` types are additive to the public API. No existing types change shape.

## Plugin-api version bump

This is a minor version bump for `haproxy-spoa-hub-plugin-api` (e.g., 0.2.0 → 0.3.0). All four current plugins build against the new version; only coraza and otel ship with `validate()` overrides in the same release window. external-auth and sso-auth keep the default impl until their authors choose to add deep validation.
