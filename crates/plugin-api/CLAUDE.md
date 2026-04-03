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
    fn init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> { Ok(()) }
    fn name(&self) -> &str { "my-plugin" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessingResult { variables: vec![].into() })
    }
});
```

Plugin Cargo.toml must have `crate-type = ["cdylib"]` (add `"rlib"` too
for unit testing).

## Panic Safety

The `define_plugin!` macro wraps `process()` in `std::panic::catch_unwind`.
A panicking plugin returns `RResult::RErr(PluginPanicError)` instead of
aborting the hub. This is critical because abi_stable's `#[sabi_trait]`
vtable uses `AbortBomb` semantics — without `catch_unwind`, a plugin panic
aborts the entire process.

## Versioning Rules

- Adding new `SpoeValue` or `ConfigValue` variants = major version bump
  (enum layout change)
- Adding new `SpoeMessage` fields = requires prefix types
- Adding new `SpoePlugin` methods after `last_prefix_field` = minor version
  (safe, old plugins use defaults)
- Moving `#[sabi(last_prefix_field)]` = major version bump
