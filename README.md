# HAProxy SPOA Hub

[![License](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub/-/badges/custom.svg?key_text=License&key_width=60&value_text=Apache%202.0&value_width=80&key_color=555&value_color=007ec6)](https://opensource.org/licenses/Apache-2.0)
[![Build Status](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub/badges/main/pipeline.svg)](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub/-/pipelines)
[![Coverage](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub/badges/main/coverage.svg)](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub/-/graphs/main/charts)

A dynamic plugin hub for [HAProxy](https://www.haproxy.org/) SPOE agents. Loads Rust plugins as shared libraries at runtime, dispatches SPOE messages (running independent plugins in parallel), and returns aggregated variables to HAProxy.

## Overview

HAProxy's [Stream Processing Offload Engine (SPOE)](https://www.haproxy.com/documentation/haproxy-configuration-manual/latest/#9.3) allows offloading request processing to external agents via the SPOP protocol. This hub acts as a single SPOE agent that multiplexes work across dynamically loaded plugins, so you can add, update, or remove processing logic without modifying or recompiling the hub itself.

**Key features:**

- **Parallel plugin execution** — independent plugins run concurrently, with topological sorting for dependency chains
- **Per-plugin timeouts** — slow plugins are aborted and partial results from completed plugins are returned
- **Variable namespacing** — plugin output variables are auto-prefixed (e.g., `geoip.country_code`) to prevent conflicts
- **Pre-parsed data** — plugins receive typed, structured arguments instead of raw SPOP bytes
- **Hot reload** — SIGHUP or a config-file change reloads configuration and swaps plugins without dropping connections; in-flight dispatches finish on the old plugin set before it is drained and torn down (swap → quiesce → drain → shutdown)
- **Graceful shutdown** — SIGTERM drains in-flight messages before exiting
- **Observability** — structured JSON logging and built-in Prometheus metrics endpoint

## Quick Start

### Prerequisites

- Rust toolchain (stable, >= 1.86)
- HAProxy >= 2.0 with SPOE support

### Build

```bash
git clone https://gitlab.com/haproxy-haptic/haproxy-spoa-hub.git
cd haproxy-spoa-hub
cargo build --release
```

The hub binary is at `target/release/haproxy-spoa-hub`. The build also produces example plugins: `libnoop_plugin.so` (sets a static variable) and `libecho_plugin.so` (echoes all message arguments back as variables).

### Configure the Hub

Create a configuration file (e.g., `config.toml`) with both plugins registered for the same SPOE message — the hub runs them in parallel automatically:

```toml
plugin_dir = "/opt/spoa-plugins"
default_timeout_ms = 100
log_level = "info"
metrics_addr = "0.0.0.0:9090"

[[listeners]]
type = "unix"
address = "/var/run/haproxy-spoa-hub.sock"

[[plugins]]
name = "noop"
library = "libnoop_plugin.so"
messages = ["check-request"]

[[plugins]]
name = "echo"
library = "libecho_plugin.so"
messages = ["check-request"]
```

Copy both plugins and start the hub:

```bash
mkdir -p /opt/spoa-plugins
cp target/release/libnoop_plugin.so target/release/libecho_plugin.so /opt/spoa-plugins/
haproxy-spoa-hub --config config.toml
```

Both plugins handle the same `check-request` message. Since neither depends on the other, the hub executes them concurrently — total latency is the slower of the two, not the sum.

### Configure HAProxy

Add an SPOE filter to your frontend. First, the SPOE agent definition (`spoe-hub.conf`):

```
[spoa-hub]
spoe-agent spoa-hub-agent
    messages check-request
    option var-prefix hub
    timeout hello      2s
    timeout idle       120s
    timeout processing 500ms
    use-backend spoa-hub-backend

spoe-message check-request
    args src=src method=method path=path
    event on-frontend-http-request
```

Then reference it in `haproxy.cfg`. Variables from each plugin are automatically namespaced with the plugin name:

```haproxy
frontend http-in
    bind *:8080
    filter spoe engine spoa-hub config /etc/haproxy/spoe-hub.conf
    # noop plugin sets "result", available as hub.noop.result
    http-request set-header X-Noop %[var(sess.hub.noop.result)]
    # echo plugin mirrors args back, e.g. hub.echo.src
    http-request set-header X-Client-Src %[var(txn.hub.echo.src)]
    default_backend app
```

### Verify

```bash
# Validate configuration without starting
haproxy-spoa-hub --config config.toml --check

# Send a request through HAProxy and check the hub logs
curl http://localhost:8080/
```

The hub logs structured JSON to stderr. Both plugins load at startup and execute in parallel on each request:

```json
{"timestamp":"...","level":"INFO","message":"plugin loaded","plugin":"noop","version":"0.1.0"}
{"timestamp":"...","level":"INFO","message":"plugin loaded","plugin":"echo","version":"0.1.0"}
{"timestamp":"...","level":"INFO","message":"listening","transport":"unix","address":"/var/run/haproxy-spoa-hub.sock"}
{"timestamp":"...","level":"DEBUG","message":"plugin completed","plugin":"noop","latency_us":55,"variables":1}
{"timestamp":"...","level":"DEBUG","message":"plugin completed","plugin":"echo","latency_us":69,"variables":1}
{"timestamp":"...","level":"DEBUG","message":"message processed","message_name":"check-request","latency_us":330}
```

To add a plugin that depends on another's output, use `depends_on`:

```toml
[[plugins]]
name = "bot-detect"
library = "libbot_detection.so"
messages = ["check-request"]
depends_on = ["ja3"]  # runs after ja3, receives ja3.hash in message args
```

## Configuration Reference

### Top-Level Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `plugin_dir` | string | yes | — | Path to directory containing plugin `.so` files |
| `default_timeout_ms` | integer | no | 100 | Default per-plugin execution timeout |
| `log_level` | string | no | `"info"` | Log level (`RUST_LOG` syntax) |
| `metrics_addr` | string | no | disabled | Prometheus endpoint bind address (e.g., `"0.0.0.0:9090"`) |
| `worker_threads` | integer | no | num CPUs | Tokio runtime worker threads |
| `max_connections` | integer | no | unlimited | Maximum concurrent SPOP connections |
| `blocking_thread_keep_alive_secs` | integer | no | 300 | How long idle blocking pool threads stay alive before being reaped. Higher values avoid thread re-creation overhead (~10-50µs) on bursty traffic. Tokio's built-in default is 10 seconds. |

### `[[listeners]]`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type` | string | yes | `"unix"` or `"tcp"` |
| `address` | string | yes | Socket path or `host:port` |

### `[[plugins]]`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | — | Unique identifier (used as variable namespace prefix) |
| `library` | string | yes | — | Filename of `.so` in `plugin_dir` |
| `messages` | array | yes | — | SPOE message names this plugin handles |
| `depends_on` | array | no | `[]` | Plugin names that must complete before this one |
| `timeout_ms` | integer | no | `default_timeout_ms` | Per-plugin timeout override |
| `params` | table | no | `{}` | Typed parameters passed to `init()` (supports nested tables, arrays, and all TOML types) |

!!! note
    Top-level keys (`plugin_dir`, `default_timeout_ms`, etc.) must appear **before** any `[[listeners]]` or `[[plugins]]` table arrays in the TOML file.

## Writing a Plugin

Plugins are standalone Rust crates compiled to shared libraries. You only need the `haproxy-spoa-hub-plugin-api` crate — no access to the hub source code required.

### 1. Create the crate

```bash
cargo new --lib my-spoa-plugin
cd my-spoa-plugin
```

Set up `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
haproxy-spoa-hub-plugin-api = "0.2"
abi_stable = "0.11"
```

### 2. Implement the plugin

Define your plugin struct, then register it with `define_plugin!`. The macro generates the FFI trait implementation and wraps `process()` in `catch_unwind` so a panic doesn't crash the hub:

```rust
#![allow(non_camel_case_types, non_local_definitions)]

use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, {
    fn new() -> Self {
        MyPlugin
    }

    fn init(
        &mut self,
        context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Access per-plugin configuration from [plugins.params]
        let db_path = context.get_config("db_path")
            .and_then(|v| v.as_str())
            .ok_or("missing required config: db_path")?;
        Ok(())
    }

    fn name(&self) -> &str { "my-plugin" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        // Typed argument lookup (O(1), no allocation)
        let src = message.get_string("src").unwrap_or("unknown");

        Ok(ProcessingResult::single(
            TxnVariable::session("result", SpoeValue::String("processed".into())),
        ))
    }
});
```

### 3. Available types

Message arguments and return values use `SpoeValue`:

| Variant | Rust type | Description |
|---------|-----------|-------------|
| `Null` | — | No value |
| `Bool(bool)` | `bool` | Boolean |
| `Int32(i32)` | `i32` | Signed 32-bit integer |
| `Uint32(u32)` | `u32` | Unsigned 32-bit integer |
| `Int64(i64)` | `i64` | Signed 64-bit integer |
| `Uint64(u64)` | `u64` | Unsigned 64-bit integer |
| `Ipv4([u8; 4])` | `[u8; 4]` | IPv4 address |
| `Ipv6([u8; 16])` | `[u8; 16]` | IPv6 address |
| `String(RString)` | FFI-safe string | UTF-8 string |
| `Binary(RVec<u8>)` | FFI-safe byte vec | Raw bytes |

`SpoeMessage` provides typed accessors for extracting arguments without manual pattern matching: `get()`, `get_string()`, `get_int()`, `get_bool()`, `get_ipv4()`, `get_ipv6()`, `get_binary()`. All return `Option` and perform O(1) lookup.

Variable scopes control lifetime in HAProxy: `Process`, `Session`, `Transaction`, `Request`, `Response`. Use `TxnVariable::transaction()` or `TxnVariable::session()` as shorthand constructors.

### 4. Variable namespacing

You set variables with plain names (e.g., `"country_code"`). The hub automatically prefixes them with your plugin name. A plugin named `"geoip"` setting `"country_code"` produces the HAProxy variable `geoip.country_code`.

### 5. Plugin configuration

Plugins receive per-plugin parameters via the `[plugins.params]` TOML table. All native TOML types are supported — strings, integers, floats, booleans, arrays, and nested tables:

```toml
[[plugins]]
name = "geoip"
library = "libgeoip_lookup.so"
messages = ["check-request"]

[plugins.params]
db_path = "/data/GeoLite2-City.mmdb"
locale = "en"
max_results = 10
trusted_cidrs = ["10.0.0.0/8", "172.16.0.0/12"]

[plugins.params.cache]
enabled = true
ttl_seconds = 300
```

These are passed to `init()` as a `PluginContext` with a typed config map. Use `get_config()` to look up keys and `ConfigValue` accessor methods to extract typed values:

```rust
fn init(&mut self, context: &PluginContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let db_path = context.get_config("db_path")
        .and_then(|v| v.as_str())
        .ok_or("missing required config: db_path")?;
    self.db = MaxMindDB::open(db_path)?;

    // Integer parameters
    self.max_results = context.get_config("max_results")
        .and_then(|v| v.as_integer())
        .unwrap_or(5) as usize;

    // Nested table access
    if let Some(cache) = context.get_config("cache")
        .and_then(|v| v.as_table())
    {
        self.cache_enabled = cache.get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    Ok(())
}
```

`ConfigValue` provides typed accessors: `as_str()`, `as_integer()`, `as_float()`, `as_bool()`, `as_array()`, and `as_table()`. Each returns `None` if the value is a different type.

### 6. Config schema validation

Plugins can declare a [JSON Schema](https://json-schema.org/) to validate their configuration at startup. If the user-provided config doesn't match the schema, the hub rejects the plugin with a clear error message before `init()` is called.

Add an optional `config_schema` block at the end of `define_plugin!`:

```rust
    fn config_schema(&self) -> Option<&str> {
        Some(r#"{
            "type": "object",
            "required": ["db_path"],
            "properties": {
                "db_path": { "type": "string" },
                "max_results": { "type": "integer", "minimum": 1 },
                "cache": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "ttl_seconds": { "type": "integer", "minimum": 0 }
                    }
                }
            }
        }"#)
    }
```

If the config is invalid, the hub logs an error like:

```json
{"level":"ERROR","message":"failed to load plugin","error":"plugin 'my-plugin' config validation failed: \"db_path\" is a required property"}
```

Plugins that don't declare a schema skip validation entirely (the default).

### 7. Plugin dependencies

If your plugin needs output from another plugin, declare it in the hub config:

```toml
[[plugins]]
name = "bot-detect"
library = "libbot_detection.so"
messages = ["check-request"]
depends_on = ["ja3"]
timeout_ms = 80
```

Your plugin receives the upstream plugin's output variables as additional entries in `message.args`, keyed by their prefixed names (e.g., `"ja3.hash"`).

### 8. Skipping plugins per-request

The hub supports `hub.`-prefixed control args in SPOE messages. These are consumed by the hub and never passed to plugins. The first control arg is `hub.skip_plugins`: a comma-separated list of plugin names to skip for the current request.

Configure HAProxy to set it conditionally using ACLs:

```
spoe-message check-request
    args src=src method=method path=path hub.skip_plugins=var(txn.skip_plugins)
    event on-frontend-http-request
```

```haproxy
acl allowlisted src -f /etc/haproxy/allowlist.txt
http-request set-var(txn.skip_plugins) str("rate-limiter") if allowlisted
```

Skipped plugins incur zero processing overhead (no thread pool scheduling, no FFI call). If a skipped plugin has dependents (declared via `depends_on`), those dependents are automatically skipped too.

The hub logs skipped plugins at debug level and records a `spoa_plugin_skips_total` Prometheus counter with a `plugin` label.

### 9. Build and deploy

```bash
cargo build --release
cp target/release/libmy_spoa_plugin.so /opt/spoa-plugins/
```

Add a `[[plugins]]` entry to the hub config and either restart the hub or send SIGHUP to reload.

### 10. Plugin logging

Plugins can use the standard `log` crate for structured logging. Add `log = "0.4"` to your plugin's dependencies and use the standard macros:

```rust
log::info!("request processed: path={}", path);
log::warn!("lookup failed for IP: {}", addr);
```

Log lines appear in the hub's structured JSON output. Available levels: `trace`, `debug`, `info`, `warn`, `error`.

## Observability

### Structured Logging

The hub writes structured JSON to stderr. Key fields:

| Field | Description |
|-------|-------------|
| `timestamp` | ISO 8601 timestamp |
| `level` | `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` |
| `message` | Event description |
| `plugin` | Plugin name (on plugin-related events) |
| `latency_us` | Execution time in microseconds (per-plugin and per-message) |
| `message_name` | SPOE message name (on dispatch events) |
| `target` | Rust module path |

Configure the log level via the `log_level` config field or the `RUST_LOG` environment variable.

### Prometheus Metrics

Set `metrics_addr` in the config to expose a `/metrics` endpoint:

```toml
metrics_addr = "0.0.0.0:9090"
```

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `spoa_messages_total` | counter | `message` | Total SPOP messages processed |
| `spoa_message_duration_seconds` | histogram | `message` | Per-message processing latency |
| `spoa_active_connections` | gauge | — | Current active SPOP connections |
| `spoa_plugin_errors_total` | counter | `plugin`, `kind` | Plugin processing errors (`kind`: `transient` or `permanent`) |
| `spoa_plugin_timeouts_total` | counter | `plugin` | Plugin timeout events |
| `spoa_plugin_skips_total` | counter | `plugin` | Plugins skipped via `hub.skip_plugins` |

All metrics carry a `service="haproxy-spoa-hub"` global label.

## Development

### Prerequisites

- Rust >= 1.86 (edition 2024)
- Docker and Docker Compose (for integration tests)

### Commands

```bash
cargo build --workspace              # Build everything
cargo test --workspace               # Unit + integration tests (83 tests)
cargo clippy --workspace             # Lint (pedantic)
cargo bench -p haproxy-spoa-hub      # Criterion benchmarks
cargo doc --workspace --no-deps      # Build rustdoc

cd tests/integration && make test    # Docker integration tests (HAProxy + hub)
```

### Project Structure

```
Cargo.toml                  Workspace root
crates/
  hub/                      Hub daemon (binary + library)
    src/
      main.rs               Entry point, CLI, signal handling
      config.rs             TOML config parsing and validation
      listener.rs           Unix + TCP listener setup
      connection.rs         Per-connection SPOP handling
      dispatch.rs           Message -> plugin routing + parallel exec
      plugin_loader.rs      abi_stable plugin loading + ABI check
      registry.rs           Plugin registry + execution plan builder
      metrics.rs            Prometheus metrics
  plugin-api/               Published plugin API crate
    src/
      lib.rs                Re-exports, define_plugin! macro
      types.rs              ConfigValue, SpoeMessage, SpoeValue, ProcessingResult
      plugin_trait.rs       #[sabi_trait] SpoePlugin + PluginMod
examples/
  noop/                     Example: sets one variable
  delay/                    Test: configurable sleep
  echo/                     Test: echoes all args back
tests/integration/          Docker-based e2e tests
```

### Architecture

- **Protocol**: The `spop` crate handles SPOE wire protocol encoding/decoding (HAPROXY-HELLO, NOTIFY/ACK, DISCONNECT)
- **Plugin loading**: `abi_stable` provides type-safe FFI with load-time ABI verification via `load_from_file()`
- **Dispatch**: Single-plugin path uses `spawn_blocking` directly. Multi-plugin parallel groups use `spawn_blocking` per plugin + `join_all` with per-plugin timeouts
- **Registry**: `Arc<ArcSwap<PluginRegistry>>` enables lock-free reload — each connection loads the current registry snapshot when processing a NOTIFY frame. A reload swaps the new registry in atomically, then retires the old one in phases (`registry::swap_and_retire`): wait for in-flight dispatches still holding the pre-swap snapshot (quiesce), wait for plugins' fire-and-forget background work (drain), then shut the old set down
- **Execution plans**: Topological sort of plugin dependency graphs per SPOE message. Independent plugins form parallel groups, dependent plugins run sequentially

## License

Licensed under the [Apache License 2.0](LICENSE).
