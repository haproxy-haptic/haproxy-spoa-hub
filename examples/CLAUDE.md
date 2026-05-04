# examples/

Example and test plugins. Each is a standalone Rust crate that compiles
to a shared library (`.so`).

## Why these plugins exist (read before deleting any)

Each example plugin pulls double duty:

1. **Reference implementation** for plugin authors looking to write their
   own. The whole point of `define_plugin!` having a stable, ergonomic
   shape is that an external author can copy `examples/noop` and have
   a working plugin in five minutes.
2. **Continuous regression net** for the macro and the FFI surface.
   The macro has hidden complexity around the optional `config_schema`
   and `validate` arms (see `validating-noop`'s regression-tests
   module). Without a real plugin in this workspace exercising those
   arms, a macro bug would only be discovered by external plugin
   authors after release.
3. **Inputs to the matrix test** (`crates/hub/tests/abi_matrix.rs`).
   Once the plugin-api evolves past v1, these examples serve as the
   "current main" cells in the matrix — the contract being that they
   must keep loading on the current hub even when their crate version
   is older than the hub's plugin-api dependency.

If you find yourself wanting to delete an example because "we already
test that elsewhere", check the regression-tests module first — there
is usually a load-bearing test that depends on the exact macro arm
that example exercises.

## Common Pattern

All plugins follow the same structure:

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
haproxy-spoa-hub-plugin-api = { path = "../../crates/plugin-api" }
```

```rust
// src/lib.rs
#![allow(non_camel_case_types, non_local_definitions)]
use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, { /* ... */ });
```

The `#![allow(...)]` is required to suppress warnings from the macro's
generated code (`#[derive(StableAbi)]` on data types and FFI thunk
generation).

External plugins **no longer need a direct `abi_stable` dependency**
since the v0.4.0 plugin-api migration. The `define_plugin!` macro
routes through this crate's re-exports. Drop the `abi_stable = "0.11"`
line from your plugin's `Cargo.toml` if it's still there.

## Build

```sh
cargo build --release -p noop-plugin               # → target/release/libnoop_plugin.so
cargo build --release -p delay-plugin              # → target/release/libdelay_plugin.so
cargo build --release -p echo-plugin               # → target/release/libecho_plugin.so
cargo build --release -p validating-noop-plugin    # → target/release/libvalidating_noop_plugin.so
```

## Available Plugins

| Plugin | Purpose | Variables Set |
|--------|---------|--------------|
| `noop` | Minimal example, sets one variable | `sess.noop.result = "ok"` |
| `delay` | Configurable sleep (via `delay_ms` arg, default 5ms) | `sess.delay.done = true` |
| `echo` | Returns all message args as txn variables | `txn.echo.<arg_name> = <arg_value>` |
| `validating-noop` | Exercises the macro's `config_schema` AND `validate` arms; carries regression tests for them | `sess.validating-noop.result = "ok"` |

The `delay` plugin is used in parallel execution timing tests. The
`echo` plugin is used for type conversion verification tests. The
`validating-noop` plugin's regression-tests module is the only place
the macro's optional arms are exercised at compile time — do not
delete them without a replacement.

## Adding a new example

Required when the plugin-api evolves with a new optional method (the
example plugin's job is to prove the macro's arm for it works
end-to-end). Pattern:

1. Copy `examples/noop`, rename in `Cargo.toml` and `src/lib.rs`.
2. Add the new optional method's body to the `define_plugin!` block.
3. Add a regression-tests module mirroring `validating-noop`'s — the
   tests should exercise both the with-body and without-body shapes
   (the latter via a separate plugin if necessary).
4. Add the package to the workspace `members =` list in the
   workspace `Cargo.toml`.
5. Add the new plugin to `crates/hub/tests/abi-matrix-versions.toml`
   so future hubs verify it keeps loading.
