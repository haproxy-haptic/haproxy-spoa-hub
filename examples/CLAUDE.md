# examples/

Example and test plugins. Each is a standalone Rust crate that compiles to
a shared library (`.so`).

## Common Pattern

All plugins follow the same structure:

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
haproxy-spoa-hub-plugin-api = { path = "../../crates/plugin-api" }
abi_stable = "0.11"
```

```rust
// src/lib.rs
#![allow(non_camel_case_types, non_local_definitions)]
use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, { /* ... */ });
```

The `#![allow(...)]` is required to suppress warnings from abi_stable's
generated code.

## Build

```sh
cargo build --release -p noop-plugin    # → target/release/libnoop_plugin.so
cargo build --release -p delay-plugin   # → target/release/libdelay_plugin.so
cargo build --release -p echo-plugin    # → target/release/libecho_plugin.so
```

## Available Plugins

| Plugin | Purpose | Variables Set |
|--------|---------|--------------|
| `noop` | Minimal example, sets one variable | `sess.noop.result = "ok"` |
| `delay` | Configurable sleep (via `delay_ms` arg, default 5ms) | `sess.delay.done = true` |
| `echo` | Returns all message args as txn variables | `txn.echo.<arg_name> = <arg_value>` |

The `delay` plugin is used in parallel execution timing tests.
The `echo` plugin is used for type conversion verification tests.
