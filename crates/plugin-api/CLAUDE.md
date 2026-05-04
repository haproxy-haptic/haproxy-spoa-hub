# crates/plugin-api

Published as `haproxy-spoa-hub-plugin-api` on crates.io. The only crate
plugin authors need to depend on.

## Read this before changing anything in this crate

This crate's binary layout is **load-bearing**. Plugins compiled against
older versions of it must keep loading on newer hubs. The full design
rationale is in `docs/adr/0001-abi-evolution.md` and the operator-facing
reference is `docs/abi-evolution.md`. Short version below.

### Three rules that keep ABI compat working

1. **Append-only.** Every new field appended after the existing ones in
   `vtable::PluginVTable`, every new variant of `ConfigValue` /
   `SpoeValue` / `VarScope` appended before the `__Other` catch-all,
   every new field of `PluginContext` / `SpoeMessage` /
   `ProcessingResult` / `Diagnostic` appended at the end of the struct.
2. **Never reorder, never remove.** Existing fields and variants stay
   exactly where they are. The byte offsets of every previously-shipped
   field are part of the ABI contract. Reordering breaks every old
   plugin in the field.
3. **Bump `PLUGIN_API_VERSION`** by 1 each time a vtable field is
   appended. The hub uses this to gate access to the new field. Without
   the bump, the hub thinks every plugin has the new field and reads
   past the end of older plugins' allocations (UB).

The matrix test `crates/hub/tests/abi_matrix.rs` enforces these rules
by loading every published plugin version against the current hub
binary. Violations turn it red.

### Pinned `abi_stable` dependency

`Cargo.toml` pins `abi_stable = "=0.11.3"` (exact). `RString`, `RVec`,
`RHashMap`, `RResult`, etc. show up across the FFI boundary, and
`abi_stable`'s load-time layout check no longer runs to catch
mismatches (we removed the prefix-type machinery in v0.4.0). Different
patch versions of `abi_stable` could re-lay-out `RString` internals
silently, which would corrupt memory at runtime with no error. **Do
not relax the pin in a casual bump.** Bumping abi_stable is a
coordinated rollout: bump this crate's version, rebuild + re-publish
every plugin, run the matrix test against the previous published
plugin set first to confirm layout has not drifted.

### What changes are still safe (do not require ABI work)

- Adding methods to types (e.g., new `&self` helper on `SpoeMessage`)
  if they don't change struct layout.
- Adding new public top-level functions / types that don't appear in
  any plugin or vtable signature.
- Documentation, tests, internal helpers.
- Performance changes that don't alter the binary interface.

### What changes are NOT safe (require an `api_version` bump)

- Adding a field to `PluginVTable` (must also bump `PLUGIN_API_VERSION`
  and add a host-side accessor that gates on the new version).
- Adding a field to `PluginContext`, `SpoeMessage`, `ProcessingResult`,
  `Diagnostic`, `TxnVariable` — these are passed by reference across
  the FFI boundary and a layout change breaks every old plugin.
  Currently we have no per-field version-gating for data types; if you
  need to add one, talk to the team about the migration plan first.
- Changing existing variant/field types.
- Anything that touches `vtable.rs` or the `define_plugin!` macro's
  emitted thunks needs careful review against the matrix test.

## Public API

- **`vtable::PluginVTable`** (`vtable.rs`): the `#[repr(C)]` struct of
  function pointers a plugin's `.so` exports. v1 baseline has
  `api_version`, `create`, `destroy`, `init`, `process`, `name`,
  `plugin_version`, `shutdown`, `config_schema`, `validate`.
- **`define_plugin!`** macro (`lib.rs`): generates the FFI thunks
  (each `extern "C" fn` that bridges into the plugin author's regular
  Rust methods), the static `PluginVTable` instance, and the
  `get_plugin_vtable` exported symbol the hub looks up via `dlsym`.
  Wraps `process` in `catch_unwind` so a plugin panic surfaces as
  `RResult::RErr(PluginPanicError)` instead of aborting the hub.
- **`PLUGIN_API_VERSION`** + **`PLUGIN_API_VERSION_V1`** constants: the
  hub uses these to gate access to version-introduced fields.
- **Types** (`types.rs`): `ConfigValue`, `SpoeValue`, `SpoeMessage`,
  `ProcessingResult`, `TxnVariable`, `VarScope`, `PluginContext`,
  `Diagnostic`, `DiagnosticSeverity` — all `#[derive(StableAbi)]` for
  `#[repr(C)]` layout. Their layout is part of the ABI contract.

## Plugin Author Workflow

```rust
#![allow(non_camel_case_types, non_local_definitions)]
use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, {
    fn new() -> Self { MyPlugin }
    fn init(&mut self, _context: &PluginContext)
        -> Result<(), Box<dyn std::error::Error + Send + Sync>> { Ok(()) }
    fn name(&self) -> &str { "my-plugin" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        let src = message.get_string("src").unwrap_or("unknown");
        Ok(ProcessingResult::single(
            TxnVariable::session("result", SpoeValue::String(src.into())),
        ))
    }
});
```

Plugin `Cargo.toml` must have `crate-type = ["cdylib"]` (add `"rlib"`
too for unit testing). Adding `abi_stable` as a direct dependency is
no longer required — the `define_plugin!` macro routes through this
crate's re-exports. Plugins continue to import `abi_stable` if they
use raw `RString` / `RVec` constructors, but they do NOT need to use
`#[derive(StableAbi)]` themselves.

## How to add a new plugin method (the right way)

Worked example: imagine adding a `metrics(&self) -> Vec<MetricSample>`
method that lets a plugin report its own runtime metrics.

1. **Append the field** to `vtable::PluginVTable` AFTER the v1 baseline
   marker comment. Add a new `pub const PLUGIN_API_VERSION_V2: u32 = 2;`
   and bump `PLUGIN_API_VERSION` to 2.
2. **Extend the `define_plugin!` macro** to optionally accept a
   `metrics(&self) -> Vec<MetricSample>` block. If the author provides
   one, emit a thunk that calls into it; if not, emit a thunk that
   returns an empty `RVec`. Mirror the pattern used for `validate` and
   `config_schema` today.
3. **Add a host accessor** in `crates/hub/src/plugin.rs::Plugin` that
   gates on `PLUGIN_API_VERSION_V2` (mirror the validate accessor's
   pattern).
4. **Update the matrix test** (`crates/hub/tests/abi_matrix.rs` +
   `crates/hub/tests/abi-matrix-versions.toml`) so cells from older
   plugin versions are expected to take the fallback path.
5. **Bump this crate's version** to `0.5.0` (a minor bump is the right
   shape — old plugins still load).

What you do NOT do: change `process`'s signature, reorder methods,
move the `// End of v1 baseline` marker comment up.

## Panic Safety

Every FFI thunk emitted by `define_plugin!` is wrapped in
`std::panic::catch_unwind`, not just `process`. This is non-negotiable:
Rust treats unwinding through `extern "C"` as abort, so a panic in any
of `create`, `init`, `process`, `name`, `plugin_version`,
`config_schema`, `validate`, `shutdown`, or `destroy` would otherwise
take down the hub process — including in `--validate-socket` mode,
where the hub absolutely must survive a buggy plugin.

Each thunk has a documented fallback for the catch case:

| Thunk | On panic |
|-------|----------|
| `create` | `RResult::RErr(PluginPanicError)` — load fails cleanly |
| `init` | `RResult::RErr(PluginPanicError)` — load fails cleanly |
| `process` | `RResult::RErr(PluginPanicError)` — request fails cleanly |
| `validate` | `RVec` containing a single `Diagnostic::error("plugin's validate() panicked")` — admission webhook denies cleanly |
| `config_schema` | `RNone` — schema validation is skipped |
| `name` / `plugin_version` | `RStr::from("<plugin-panic>")` — visible in logs but doesn't crash |
| `shutdown` / `destroy` | absorbed; possible memory leak but hub stays up |

Code review on plugin-api changes must verify the catch_unwind wrapper
is intact for every thunk. The `define_plugin!` macro doc-comment lists
the contract; do not move any thunk outside catch_unwind without a new
ADR.

## `&'static str` requirement on `name` / `version`

The FFI thunks for `name` and `plugin_version` return `RStr<'static>`,
so the author-supplied bodies MUST return `&'static str`. In practice
this means string literals or `env!("CARGO_PKG_VERSION")`. If a plugin
needs to compute its name from instance fields, it must produce a
`&'static str` at construction time (e.g. via `Box::leak(...)`) or
pre-register the strings as `const`s.

This is a tightening from the original `SpoePlugin` trait which used
`fn name(&self) -> &str` (lifetime tied to `&self`). The macro's doc-
comment surfaces this; mention it to plugin authors who hit a compile
error after upgrading to plugin-api 0.4.0.

## Convenience Helpers

- **`SpoeMessage`**: `get()`, `get_string()`, `get_int()`, `get_bool()`,
  `get_ipv4()`, `get_ipv6()`, `get_binary()` — O(1) argument lookup
  accepting `&str` directly (`RString` implements `Borrow<str>`).
- **`TxnVariable`**: `new()`, `transaction()`, `session()` — scope-
  specific constructors that accept `impl Into<RString>`.
- **`ProcessingResult`**: `empty()`, `single()`, `from_vars()` — result
  constructors replacing verbose struct literals.
- **`PluginContext`**: `get_config()` — config key lookup accepting
  `&str`.
- **Re-exports**: `RString`, `RVec`, `RHashMap`, `Tuple2` are re-
  exported from this crate, so plugins can import them without
  reaching into `abi_stable::std_types` directly.

## Enum-evolution rule (legacy carry-over from `abi_stable` era)

Public enums (`ConfigValue`, `SpoeValue`, `VarScope`) have a `__Other`
catch-all variant. Plugins MUST use `_ =>` wildcard arms in match
statements to handle future variants gracefully. This rule predates
the v0.4.0 vtable migration and continues to apply because the data
types themselves still flow through the FFI boundary.
