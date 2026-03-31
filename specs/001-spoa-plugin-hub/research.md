# Research: HAProxy SPOA Plugin Hub

**Date**: 2026-03-30
**Feature**: 001-spoa-plugin-hub

## Decision 1: Plugin Loading System

**Decision**: Use `abi_stable` 0.11.3 for type-safe Rust-to-Rust dynamic
plugin loading with load-time layout verification.

**Rationale**: The spec requires Rust-only plugins for v1 and a
"robust and standardized plugin load system." abi_stable provides:
- Recursive type layout verification at load time (catches ABI mismatches
  as errors, not segfaults)
- FFI-safe replacements for standard types: `RVec`, `RString`,
  `RHashMap`, `RResult`, `ROption`, etc.
- Prefix types for forward-compatible plugin interfaces (new methods
  can be added in minor versions without breaking old plugins)
- Semver-aware version checking (major must match; for 0.x, minor must
  be >=)
- ~3.2ns per-call overhead (negligible for SPOE processing where a
  single syscall costs ~1μs)

**Alternatives considered**:
- `extern "C"` + `libloading`: ~1.9ns per call, language-agnostic,
  but no type safety — caller must manually ensure signature
  correctness. Risk of silent memory corruption on mismatch. Could be
  added later for C/Go plugin support.
- `stabby`: Newer alternative with better enum representation, but
  less battle-tested and smaller ecosystem.
- WASM (wasmtime/wasmer): True sandbox with crash isolation, but ~3x
  slower than native. Could be added as secondary plugin runtime later.
- `dlopen2`: Same ABI limitations as libloading, slightly nicer API.

**Panic safety concern**: abi_stable's `#[sabi_trait]` uses AbortBomb
semantics — a plugin panic inside a trait method aborts the host
process. Mitigation: the plugin API crate will provide a
`define_plugin!` proc macro that wraps the plugin's `process`
implementation in `std::panic::catch_unwind`, converting panics to
`RResult::RErr` before they reach the FFI boundary. This satisfies
FR-013 at the tooling level.

## Decision 2: SPOP Protocol Implementation

**Decision**: Use the `spop` crate 0.11.0 for SPOE wire protocol
encoding/decoding.

**Rationale**:
- Only Rust SPOE/SPOP implementation available.
- Actively maintained (latest release 2026-03-09, edition 2024).
- Transport-agnostic codec (`SpopCodec` implements
  `tokio_util::codec::Decoder` + `Encoder`).
- Full protocol coverage: all 6 frame types (HAPROXY-HELLO,
  AGENT-HELLO, NOTIFY, ACK, HAPROXY-DISCONNECT, AGENT-DISCONNECT).
- Typed data support: `TypedData` enum covers all SPOE types (Null,
  Bool, Int32, UInt32, Int64, UInt64, IPv4, IPv6, String, Binary).
- Builder pattern for ACK responses with variable scoping.
- Already proven in the haproxy-fingerprinting reference project.

**Alternatives considered**:
- Implement SPOP from scratch: Unnecessary given spop's complete
  coverage. Would duplicate ~2000 lines of protocol parsing code.
- No alternatives exist on crates.io.

## Decision 3: Async Runtime

**Decision**: Use `tokio` 1.x as the async runtime.

**Rationale**: The `spop` crate depends on tokio and tokio-util. The
reference implementation uses tokio for Unix socket listening.
Tokio is the de facto standard for async Rust with the largest
ecosystem (hyper, tower, tonic all build on it).

**Alternatives considered**:
- `async-std`: Less ecosystem support, would conflict with spop's
  tokio dependency.
- Synchronous (thread-per-connection): Higher resource usage at scale,
  incompatible with spop's codec design.

## Decision 4: Structured Logging

**Decision**: Use `tracing` 0.1 + `tracing-subscriber` 0.3 with JSON
formatter and env-filter.

**Rationale**: Current Rust ecosystem standard for structured logging.
First-class async support (spans track execution across `.await`
points). The `slog` crate's own README recommends considering tracing
instead. JSON output to stderr with configurable log levels via
`RUST_LOG` environment variable.

**Alternatives considered**:
- `slog`: Effectively in maintenance mode, author recommends tracing.
- `log` + `env_logger`: No structured/JSON output support.

## Decision 5: Prometheus Metrics

**Decision**: Use `metrics` 0.24 + `metrics-exporter-prometheus` 0.18
with built-in Hyper HTTP server for the `/metrics` endpoint.

**Rationale**: Facade pattern matching tracing's design philosophy.
Built-in HTTP server (Hyper 1.x) spawns a lightweight listener on a
configurable port — no external HTTP framework needed. Integrates with
tracing via `metrics-tracing-context` for automatic span-to-label
propagation.

**Alternatives considered**:
- `prometheus` (tikv): More downloads but requires bringing your own
  HTTP server.
- `prometheus-client` (official): Smallest ecosystem, also requires
  own HTTP server.

## Decision 6: Configuration Format

**Decision**: TOML via `toml` crate + `serde`.

**Rationale**: Decided during specification clarification. Rust
ecosystem standard (Cargo.toml, rustfmt.toml). Clean syntax for
nested configuration. First-class serde support.

## Decision 7: Workspace Structure

**Decision**: Cargo workspace with three crates:

```
Cargo.toml              (workspace root)
crates/
  hub/                  (binary: haproxy-spoa-hub)
  plugin-api/           (library: haproxy-spoa-hub-plugin-api, published to crates.io)
examples/
  noop/                 (cdylib: example no-op plugin for testing)
```

**Rationale**: The plugin API crate must be independently publishable
so third-party developers can depend on it without the hub source.
The noop plugin serves as both a test fixture and a template for
plugin authors. Workspace ensures consistent dependency versions.

## Decision 8: Plugin Directory Layout

**Decision**: Use `abi_stable`'s `load_from_file()` (not
`load_from_directory()`) to load plugin `.so` files directly from
a flat directory.

**Rationale**: `load_from_directory()` uses a caching mechanism that
returns the first loaded module for subsequent calls of the same
`RootModule` type. Since all plugins share the same `PluginMod_Ref`
type, this cache would prevent loading multiple plugins.
`load_from_file()` with explicit paths avoids this issue and allows
a simpler flat directory layout:

```
/etc/haproxy-spoa-hub/plugins/
  libja3_fingerprint.so
  libgeoip_lookup.so
  libbot_detection.so
```

## Key Dependencies Summary

| Crate | Version | Purpose |
|-------|---------|---------|
| `spop` | 0.11 | SPOE wire protocol codec |
| `abi_stable` | 0.11 | Type-safe dynamic plugin loading |
| `tokio` | 1 | Async runtime |
| `tokio-util` | 0.7 | Codec framing |
| `futures` | 0.3 | Stream/Sink extensions |
| `tracing` | 0.1 | Structured logging facade |
| `tracing-subscriber` | 0.3 | JSON formatter, env-filter |
| `metrics` | 0.24 | Metrics facade |
| `metrics-exporter-prometheus` | 0.18 | Prometheus HTTP endpoint |
| `metrics-tracing-context` | 0.18 | Span-to-label propagation |
| `toml` | 0.8 | Configuration parsing |
| `serde` | 1 | Serialization framework |
| `clap` | 4 | CLI argument parsing |
