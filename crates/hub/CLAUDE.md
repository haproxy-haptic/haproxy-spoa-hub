# crates/hub

The main hub daemon. Has both a `lib.rs` (public modules for tests/benches)
and `main.rs` (CLI entry point).

## Modules

| Module | Responsibility |
|--------|---------------|
| `config.rs` | TOML config parsing, deserialization, validation (9 rules) |
| `listener.rs` | Unix socket + TCP listeners with `CancellationToken` shutdown |
| `connection.rs` | Per-connection SPOP lifecycle (handshake, NOTIFY/ACK, disconnect) |
| `dispatch.rs` | Message → plugin routing, parallel execution, variable prefixing |
| `registry.rs` | Plugin registry, `ExecutionPlan` builder (topological sort), `SharedRegistry` type |
| `plugin_loader.rs` | `abi_stable::load_from_file()` with ABI version checking |
| `metrics.rs` | Prometheus counters/histograms/gauges via `metrics` crate |

## Commands

```sh
cargo test -p haproxy-spoa-hub              # Unit tests (23 tests)
cargo test -p haproxy-spoa-hub --test test_single_plugin  # Noop plugin integration
cargo test -p haproxy-spoa-hub --test test_parallel       # Parallel execution
cargo bench -p haproxy-spoa-hub --bench dispatch_bench    # Dispatch latency
```

## Dispatch

`dispatch.rs` uses a single execution path for all group sizes: all
`spawn_blocking` calls are launched up front, then
`futures::future::join_all` with per-plugin timeouts. This gives true
thread-level parallelism on the blocking thread pool.

`spawn_blocking` (not direct inline calls) is used so that
`tokio::time::timeout` works even if a plugin blocks (e.g., `thread::sleep`).

## spop Crate Notes

- `TypedData` variants: `UInt32`, `UInt64`, `IPv4`, `IPv6` (capital letters)
- `FrameFlags::new(true, false)` for FIN flag — `Metadata::default()` sets
  flags to zero which causes parse failures
- `SpopCodec` decoder only handles HAProxy-side frame types (1, 2, 3). Agent
  frames (101, 102, 103) are not decoded — the mock client in tests uses raw
  byte parsing instead.
- `AgentHello` and `AgentDisconnect` are struct literals, not `::new()`.
  `HaproxyHelloFrame` and `HaproxyDisconnectFrame` are the `SpopFrame`
  implementors (not `HaproxyHello`/`HaproxyDisconnect`).
- `encode_payload` returns an error for `ListOfMessages` — NOTIFY frames
  cannot be serialized through the codec.

## Integration Tests

Tests in `tests/` load real plugin `.so` files from `target/debug/`. They
use a custom `SpopClient` (`tests/common/spop_client.rs`) that encodes
SPOP frames as raw bytes.

Tests that load different plugins **must be in separate test files**
(separate binaries) because abi_stable caches root modules per process.
