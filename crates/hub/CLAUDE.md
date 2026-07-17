# crates/hub

The main hub daemon. Has both a `lib.rs` (public modules for tests / benches)
and `main.rs` (CLI entry point).

## Read this before changing how plugins are loaded

The hub talks to plugins through a hand-rolled `#[repr(C)]` vtable
defined in `crates/plugin-api/src/vtable.rs`. **Adding a new method
on the host side requires version-gating** so the new accessor only
runs against plugins built with that field present. Skipping the
gate reads past the end of older plugins' allocations (UB).

The full design rationale lives at `docs/adr/0001-abi-evolution.md`
and the operator-facing reference at `docs/abi-evolution.md`. Short
version below.

### How to add a host-side accessor for a new plugin method

1. Add the new vtable field in `plugin-api/src/vtable.rs` AFTER the v1
   baseline marker and bump `PLUGIN_API_VERSION_V*` + `PLUGIN_API_VERSION`.
2. In `crates/hub/src/plugin.rs::Plugin`, add an accessor method that
   gates on the new version constant. Mirror this pattern (which the
   `validate` accessor will move to once we add a v2 method):

   ```rust
   pub fn metrics(&self) -> Option<RVec<MetricSample>> {
       if self.api_version < PLUGIN_API_VERSION_V2 {
           return None; // older plugin doesn't expose this method
       }
       // SAFETY: api_version >= V2, so the metrics field is in-bounds.
       let f = unsafe { (*self.vtable).metrics };
       Some(f(self.state))
   }
   ```

   The `if self.api_version < PLUGIN_API_VERSION_V*` guard is what
   makes the subsequent `unsafe { (*self.vtable).metrics }` field read
   sound. Without it, you would be reading past the plugin's
   allocation for older plugins.

3. Update `crates/hub/tests/abi_matrix.rs` so the matrix expects the
   fallback path on plugin versions that predate the new field.

### Why we don't borrow `&PluginVTable` directly

Plugin handles store `vtable: *const PluginVTable` rather than
`&'static PluginVTable`. Field reads go through `(*self.vtable).field`
which only touches the bytes for that specific field. Creating a
`&PluginVTable` borrow would semantically cover the whole struct,
which on an older plugin would extend past its allocation — the kind
of subtle UB that blesses the reference even when no field beyond
the v1 baseline is read.

## Modules

| Module | Responsibility |
|--------|---------------|
| `config.rs` | TOML config parsing, deserialization, validation (9 rules) |
| `listener.rs` | Unix socket + TCP listeners with `CancellationToken` shutdown |
| `connection.rs` | Per-connection SPOP lifecycle (handshake, NOTIFY/ACK, disconnect) |
| `dispatch.rs` | Message → plugin routing, parallel execution, variable prefixing |
| `registry.rs` | Plugin registry, `ExecutionPlan` builder (topological sort), `SharedRegistry` type |
| `plugin.rs` | `Plugin` handle: owns the dlopened `Library`, vtable pointer, state pointer; method accessors do the version-gating |
| `plugin_loader.rs` | Drives `Plugin::load` with config, runs schema + `validate()` checks, calls `init()`, returns `LoadedPlugin` |
| `validate/` | `--validate-socket` mode: a separate loader that skips `init()` purity contract, plus an orchestrator that dispatches per-file validation requests |
| `metrics.rs` | Prometheus counters/histograms/gauges via `metrics` crate |

## Commands

```sh
cargo test -p haproxy-spoa-hub                            # Unit tests
cargo test -p haproxy-spoa-hub --test test_single_plugin  # Noop plugin integration
cargo test -p haproxy-spoa-hub --test test_parallel       # Parallel execution
cargo test -p haproxy-spoa-hub --test abi_matrix          # ABI compat matrix (gated by network)
cargo bench -p haproxy-spoa-hub --bench dispatch_bench    # Dispatch latency
```

## Dispatch

`dispatch.rs` uses a single execution path for all group sizes. Each plugin
first passes a bounded per-plugin admission gate, then admitted calls are
launched with `spawn_blocking` and independent calls are joined with
`futures::future::join_all`. This gives true thread-level parallelism without
an unbounded blocking queue.

`spawn_blocking` (not direct inline calls) is used so that
`tokio::time::timeout` works even if a plugin blocks (e.g.,
`thread::sleep`). Tokio cannot cancel a blocking call: after an async timeout,
the call retains its admission permit and in-flight metric until the blocking
closure actually exits.

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

Tests in `tests/` load real plugin `.so` files from `target/debug/`.
They use a custom `SpopClient` (`tests/common/spop_client.rs`) that
encodes SPOP frames as raw bytes.

Tests that load different plugins **may be in the same test binary
now** (post-v0.4.0): the hand-rolled vtable does not cache by path
the way `abi_stable`'s `load_from_file` did. Cross-test isolation is
nice-to-have but no longer required for correctness.

## ABI compat matrix test

`tests/abi_matrix.rs` is the regression net for the layout invariants
documented above. It downloads every published plugin `.so` listed in
`tests/abi-matrix-versions.toml`, loads each against the current hub
binary, and asserts the expected outcome (compatible / forward / hard
incompatible). When you add a new plugin release, append an entry to
the toml; when you bump `PLUGIN_API_VERSION`, update the expected
column for older cells (they should remain "compatible" — that's the
whole point of the migration).
