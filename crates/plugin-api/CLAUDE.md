# crates/plugin-api

Published as `haproxy-spoa-hub-plugin-api` on crates.io. This is the only
crate plugin authors need to depend on.

## Public API

- **`SpoePlugin`** trait (`plugin_trait.rs`): `init`, `process`, `name`,
  `version`, `shutdown`, `config_schema`. Defined via `#[sabi_trait]` for
  FFI safety. Requires `Send + Sync + Debug`.
- **`define_plugin!`** macro (`lib.rs`): Generates `#[export_root_module]`
  boilerplate and wraps `process()` in `catch_unwind` for panic safety.
  Supports an optional `config_schema` block for JSON Schema validation.
- **Types** (`types.rs`): `ConfigValue`, `SpoeValue`, `SpoeMessage`,
  `ProcessingResult`, `TxnVariable`, `VarScope` — all `#[derive(StableAbi)]`.
- **`PluginMod` / `PluginMod_Ref`**: Root module with factory function.

## abi_stable Conventions

- `#[sabi(last_prefix_field)]` on `SpoePlugin::shutdown()` — methods added
  in future minor versions go after this marker with default implementations.
  `config_schema()` is the first such method (returns `RNone` by default).
- `RootModule::load_from_file()` for loading (not `load_from_directory()`
  which caches per `RootModule` type).
- FFI-safe types: `RString`, `RVec`, `RHashMap`, `RResult`, `RBoxError`,
  `ROption`.
- `RHashMap` iterates as `Tuple2<&K, &V>`, not `(&K, &V)` tuples. Use
  `pair.0` / `pair.1` instead of destructuring.

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

Plugin Cargo.toml must have `crate-type = ["cdylib"]` (add `"rlib"` too
for unit testing). The `abi_stable` crate is also required as a direct
dependency (the `#[export_root_module]` proc macro generates code that
references `::abi_stable` at the crate root).

## Convenience Helpers (v0.2.0)

- **`SpoeMessage`**: `get()`, `get_string()`, `get_int()`, `get_bool()`,
  `get_ipv4()`, `get_ipv6()`, `get_binary()` — O(1) argument lookup
  accepting `&str` directly (`RString` implements `Borrow<str>`).
- **`TxnVariable`**: `new()`, `transaction()`, `session()` — scope-specific
  constructors that accept `impl Into<RString>`.
- **`ProcessingResult`**: `empty()`, `single()`, `from_vars()` — result
  constructors replacing verbose struct literals.
- **`PluginContext`**: `get_config()` — config key lookup accepting `&str`.
- **Re-exports**: `RString`, `RVec`, `RHashMap`, `Tuple2` are re-exported
  from the plugin API crate, so plugins can import them without reaching
  into `abi_stable::std_types` directly.

## Panic Safety

The `define_plugin!` macro wraps `process()` in `std::panic::catch_unwind`.
A panicking plugin returns `RResult::RErr(PluginPanicError)` instead of
aborting the hub. This is critical because abi_stable's `#[sabi_trait]`
vtable uses `AbortBomb` semantics — without `catch_unwind`, a plugin panic
aborts the entire process.

## Versioning Rules

- Adding new named variants to enums **before** `__Other` = minor version
  (safe, `__Other` catch-all absorbs the layout change for old plugins)
- Adding new `SpoePlugin` methods after `last_prefix_field` = minor version
  (safe, old plugins use defaults)
- Adding new `SpoeMessage` fields = requires prefix types (major version)
- Changing existing variant/field types or reordering = major version bump
- Removing variants or fields = major version bump
- Moving `#[sabi(last_prefix_field)]` = major version bump

All public enums (`ConfigValue`, `SpoeValue`, `VarScope`) have a `__Other`
catch-all variant. Plugins MUST use `_ =>` wildcard arms in match
statements to handle future variants gracefully.
