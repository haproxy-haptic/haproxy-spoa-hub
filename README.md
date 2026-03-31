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
- **Hot reload** — SIGHUP reloads configuration and swaps plugins without dropping connections
- **Graceful shutdown** — SIGTERM drains in-flight messages before exiting
- **Observability** — structured JSON logging and built-in Prometheus metrics endpoint

## Quick Start

### Prerequisites

- Rust toolchain (stable, >= 1.85)
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
haproxy-spoa-hub-plugin-api = "0.1"
abi_stable = "0.11"
```

### 2. Implement the plugin

```rust
#![allow(non_camel_case_types, non_local_definitions, clippy::unnecessary_wraps, clippy::wildcard_imports)]

use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, {
    fn new() -> Self {
        MyPlugin
    }

    fn init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Set up resources (database connections, lookup tables, etc.)
        Ok(())
    }

    fn name(&self) -> &str {
        "my-plugin"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        // Access pre-parsed message arguments
        let src = message.args.get(&"src".into());

        Ok(ProcessingResult {
            variables: vec![
                TxnVariable {
                    scope: VarScope::Session,
                    name: "result".into(),
                    value: SpoeValue::String("processed".into()),
                },
            ].into(),
        })
    }
});
```

The `define_plugin!` macro handles FFI boilerplate and wraps `process()` in `catch_unwind` so a panic in your plugin doesn't crash the hub.

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

Variable scopes control lifetime in HAProxy: `Process`, `Session`, `Transaction`, `Request`, `Response`.

### 4. Variable namespacing

You set variables with plain names (e.g., `"country_code"`). The hub automatically prefixes them with your plugin name. A plugin named `"geoip"` setting `"country_code"` produces the HAProxy variable `geoip.country_code`.

### 5. Plugin dependencies

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

### 6. Build and deploy

```bash
cargo build --release
cp target/release/libmy_spoa_plugin.so /opt/spoa-plugins/
```

Add a `[[plugins]]` entry to the hub config and either restart the hub or send SIGHUP to reload.

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
| `spoa_plugin_errors_total` | counter | `plugin` | Plugin processing errors |
| `spoa_plugin_timeouts_total` | counter | `plugin` | Plugin timeout events |

All metrics carry a `service="haproxy-spoa-hub"` global label.

## Development

### Prerequisites

- Rust >= 1.85 (edition 2024)
- Docker and Docker Compose (for integration tests)

### Commands

```bash
cargo build --workspace              # Build everything
cargo test --workspace               # Unit + integration tests (42 tests)
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
      types.rs              SpoeMessage, SpoeValue, ProcessingResult
      plugin_trait.rs       #[sabi_trait] SpoePlugin + PluginMod
plugins/
  noop/                     Example: sets one variable
  delay/                    Test: configurable sleep
  echo/                     Test: echoes all args back
tests/integration/          Docker-based e2e tests
```

### Architecture

- **Protocol**: The `spop` crate handles SPOE wire protocol encoding/decoding (HAPROXY-HELLO, NOTIFY/ACK, DISCONNECT)
- **Plugin loading**: `abi_stable` provides type-safe FFI with load-time ABI verification via `load_from_file()`
- **Dispatch**: Single-plugin path uses `spawn_blocking` directly. Multi-plugin parallel groups use `spawn_blocking` per plugin + `join_all` with per-plugin timeouts
- **Registry**: `Arc<ArcSwap<PluginRegistry>>` enables lock-free SIGHUP reload — each connection loads the current registry snapshot when processing a message
- **Execution plans**: Topological sort of plugin dependency graphs per SPOE message. Independent plugins form parallel groups, dependent plugins run sequentially

## License

Licensed under the [Apache License 2.0](LICENSE).
