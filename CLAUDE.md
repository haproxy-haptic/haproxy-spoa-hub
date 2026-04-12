# haproxy-spoa-hub

A dynamic plugin hub for HAProxy SPOE agents. Loads Rust plugins as shared
libraries at runtime, dispatches SPOE messages (running independent plugins
in parallel), and returns aggregated variables to HAProxy.

## Workspace Structure

```
crates/hub/          Binary + library: the hub daemon
crates/plugin-api/   Library: published plugin API crate (haproxy-spoa-hub-plugin-api)
examples/noop/       Example plugin: sets one variable
examples/delay/      Test plugin: configurable sleep
examples/echo/       Test plugin: echoes all args back
tests/integration/   Docker-based integration tests (HAProxy + hub)
```

## Commands

```sh
cargo build --workspace              # Build everything
cargo test --workspace               # Unit + integration tests (83 tests)
cargo clippy --workspace             # Lint (pedantic)
cargo bench -p haproxy-spoa-hub      # Criterion benchmarks
cargo doc --workspace --no-deps      # Build rustdoc

cd tests/integration && make test    # Docker integration tests (SIGTERM, SIGHUP, e2e)
```

## Rust Conventions

- Edition 2024, MSRV 1.86
- `clippy::pedantic` enabled as warnings at workspace level
- `rustfmt.toml`: max_width = 100
- Error types: `thiserror::Error` for library errors, `anyhow` in main/tests
- Logging: `tracing` with structured JSON to stderr
- Metrics: `metrics` crate with Prometheus exporter

## Architecture

- **Plugin loading**: `abi_stable` 0.11 with `load_from_file()` — type-safe
  FFI with load-time ABI verification. Plugins are `cdylib` crates.
- **Protocol**: `spop` 0.11 codec for SPOE wire protocol (HAPROXY-HELLO,
  NOTIFY/ACK, DISCONNECT).
- **Dispatch**: Single-plugin path uses `spawn_blocking` directly. Multi-plugin
  parallel groups use `spawn_blocking` per plugin + `join_all` with per-plugin
  timeouts.
- **Registry**: `Arc<ArcSwap<PluginRegistry>>` enables lock-free SIGHUP reload.
- **Variable namespacing**: Plugin output variables are auto-prefixed with the
  plugin name (e.g., `noop.result`).
- **Execution plans**: Topological sort of plugin dependencies per SPOE message;
  independent plugins form parallel groups, dependent plugins run sequentially.

## Key Dependencies

| Crate | Role |
|-------|------|
| `spop` 0.11 | SPOE wire protocol codec |
| `abi_stable` 0.11 | FFI-safe dynamic plugin loading |
| `tokio` 1 | Async runtime |
| `arc-swap` 1 | Lock-free registry swap for SIGHUP |
| `tracing` + `tracing-subscriber` | Structured JSON logging |
| `metrics` + `metrics-exporter-prometheus` | Prometheus `/metrics` endpoint |
| `clap` 4 | CLI argument parsing |
| `toml` + `serde` | TOML configuration |
| `serde_json` 1 | TOML-to-JSON conversion for schema validation |
| `jsonschema` 0.45 | JSON Schema validation of plugin config |

## Known Gotchas

- **abi_stable warnings**: Plugin crates and the plugin-api crate must use
  `#![allow(non_camel_case_types, non_local_definitions)]` to suppress
  warnings from abi_stable's proc macros.
- **TOML ordering**: In hub config files, top-level keys (`plugin_dir`,
  `default_timeout_ms`, `log_level`) must appear before any `[[listeners]]`
  or `[[plugins]]` table arrays — TOML parses keys after a table array
  header as belonging to that table.
- **spop TypedData variants**: Use `UInt32`, `UInt64`, `IPv4`, `IPv6`
  (not `Uint32`, `Ipv4`, etc.).
- **spop codec limitation**: The `SpopCodec` encoder does not support
  `ListOfMessages` payloads (NOTIFY frames). The mock SPOP client in
  tests uses raw byte encoding instead.
- **abi_stable module caching**: `load_from_file()` caches by path. Tests
  that load different plugins must run in separate test binaries to avoid
  cross-contamination.
