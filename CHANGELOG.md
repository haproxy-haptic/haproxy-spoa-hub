# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Config-reload path now skips when the file content is byte-identical to the last successfully-loaded version. The previous behaviour ran a full `reload_plugins → arc_swap → shutdown_all` cycle on every file-watch event, including no-op rewrites where an upstream controller atomically renames the same content over the existing file. Reload mid-dispatch tears down in-flight plugin state — fire-and-forget dispatchers (the mirror plugin's `state.runtime.spawn` HTTP requests) lose their background tokio tasks, surfacing as gateway-API conformance `HTTPRouteRequestMultipleMirrors` timing out waiting for the mirror log to land. Reproduced against haptic's gateway-conformance suite: spoa-hub log showed `"configuration loaded"` mid-test (10:48:15.713) between request dispatch (10:48:14.27) and the test's mirror-log poll. Skipping byte-identical reloads avoids the teardown for the common no-op-rewrite case. (Note: this is the workaround. The fundamental "reload must drain in-flight requests before swapping" is a separate, deeper fix that should land independently.)

### Added

- `spoa_hub_config_reload_skipped_total` counter exposes the dedup decision so operators can quantify how many of their reload triggers were no-op rewrites. A growing counter with low (or zero) success/failure activity is a signal that the upstream writer is doing unnecessary work.

## [0.6.1] - 2026-05-23

### Fixed

- `MetricRecorder::install()` now REPLACES the recorder on every call instead of being silently no-op on the second-and-later invocations. The previous `OnceLock`-based behaviour caused a use-after-free on plugin reload: plugins declare `pub(crate) static METRICS: MetricRecorder = MetricRecorder::new();`, which lives in the .so's BSS and survives reload because the hub keeps the library mapped via libloading refcount. The NEW plugin's `set_metric_recorder` thunk called `install(NEW_ctx)`, but `OnceLock::set` returned `Err` (already initialised), the NEW ctx was dropped on the floor, and the static `METRICS` kept pointing at the OLD ctx. When the hub later dropped the OLD plugin's `Box<MetricCtx>` (after the OLD plugin's last in-flight `process()` retired), the next metric emission anywhere in the .so dereferenced freed memory → SIGSEGV (exit 139), tearing down every in-flight SPOP connection and every queued NOTIFY. Reproduced against haptic's gateway-conformance suite, which writes the hub's general-storage directory in rapid bursts (`reconciliationDebounceInterval=10ms`) and pushes the hub through dozens of plugin reloads per conformance shard. Storage switched to `RwLock<Option<RecorderInner>>` so install actually swaps and emit holds a read guard across the FFI callback. Regression tests: `metrics::tests::install_replaces_previous_recorder` pins the new contract; `install_during_concurrent_emit_does_not_tear` stresses the lock against 10k concurrent emissions and 1k concurrent installs.

### Added

- `tests/abi-matrix-versions.toml` is now populated with every published plugin .so the workspace cares about (5 plugins × 12 versions: every `mirror` v0.1.0 → v0.3.0, every `external-auth` / `fingerprinting` / `maxmind` / `otel` v1-baseline → v2-consumer release). The `abi_matrix` test downloads each .so from the GitLab Generic Package Registry, loads it against the current hub binary, and asserts (a) `dlopen` succeeds, (b) `api_version()` matches the toml's declared value, (c) v1-baseline accessors (name / plugin_version) work, (d) drop runs cleanly. Catches accidental field reorder or removal in `PluginVTable`, missed `PLUGIN_API_VERSION` bumps after appending a field, and `abi_stable` layout drift — exactly the failure modes the hand-rolled vtable migration was designed against. `coraza` and `sso-auth` are deferred (Go-FFI signal-handler init aborts in the test harness for coraza; sso-auth needs `libxmlsec1` + `libxml2` which the vanilla `rust:bookworm` test image doesn't ship); both have re-enable conditions documented inline.
- `abi_matrix` test now prints a breadcrumb line (`↓ loading <plugin>@<ver> from <path>`) before each load attempt. The summary report only flushes at the end, so without this trail a SIGSEGV or abort in one plugin's init left the operator with no visible indication of which plugin caused it.

## [0.6.0] - 2026-05-15

### Added

- **Plugin metrics** (`PLUGIN_API_VERSION` 1→2). Plugins can now emit Prometheus counters, gauges, and histograms through the hub's existing `/metrics` endpoint, alongside the hub's own `spoa_*` and `spoa_hub_*` metrics. Each metric is namespaced as `plugin_<plugin>_<metric>` so operators can tell which plugin produced which sample at a glance.
- New `set_metric_recorder` vtable entry (v2 baseline). The hub allocates a per-plugin `MetricCtx` carrying the plugin's name and installs a function pointer to its own dispatcher; the plugin invokes `record_metric(ctx, name, labels, value, kind)` for every event. The dispatcher routes into the `metrics` crate macros (`counter!` / `gauge!` / `histogram!`) which the existing Prometheus exporter already serves.
- Plugin-side helper `MetricRecorder` in `haproxy-spoa-hub-plugin-api`. Plugins declare a `static METRICS: MetricRecorder = MetricRecorder::new();` and reference it from the `define_plugin!` block via an optional `metrics_static = METRICS;` arm. Anywhere in the plugin's code, `METRICS.counter("dispatches_total", &[("kind", "ok")], 1)`, `METRICS.gauge(...)`, and `METRICS.histogram(...)` emit through the hub. The macro emits a no-op `set_metric_recorder` thunk when the `metrics_static` arm is omitted, so plugins that don't want metrics build unchanged. (The static-not-`self.field` design works around a Rust macro hygiene limitation where `$body:block` substitution can't bind `self` from the synthesised inherent method; every existing plugin in the repo already uses a static for the same reason.)
- New `examples/metrics-noop` example plugin that opts into the metrics path: declares the static recorder, emits one counter / one gauge / one histogram per `process()`, and serves as the canonical reference for plugin authors. Loaded by the new `plugin_metrics_e2e` integration test to verify the dispatcher end-to-end.

### ABI compatibility

- v1 plugins continue to load unchanged on the v2 hub. The hub gates `set_metric_recorder` access on `api_version >= 2` (new `PluginVTable::read_set_metric_recorder` accessor) and silently skips installation for v1 plugins — they never had the field in their allocation, and reading past the v1 baseline would be UB. Same fix-direction guarantee in reverse: a v2 plugin loaded by a hypothetical v1 hub keeps working because the v1 hub doesn't know to read the new field.
- The hand-rolled vtable's append-only evolution discipline (see `crates/plugin-api/src/vtable.rs` doc) gets its first real exercise here; future versions add fields below the v2 marker and follow the same gating pattern.

### Lifetime contract for plugin authors

- The recorder callback and its `ctx` pointer remain valid from the hub's `set_metric_recorder` call until that plugin's `destroy` returns. Plugins MUST NOT call the recorder after returning from `destroy`. The hub guarantees the `MetricCtx` box is freed AFTER `destroy` (Rust field drop order in `Plugin`), so the recorder ctx pointer never dangles while the plugin can observe it.

## [0.5.3] - 2026-05-05

### Changed

- The validator orchestrator now dispatches each request's `[[plugins]]` entries to their plugins **in parallel** via `std::thread::scope` instead of looping serially. Total wall-clock per validate request is now `max` of per-plugin times rather than `sum`. Matters most for configs carrying multiple plugins with non-trivial `validate()` overrides — e.g. coraza compiles operator-supplied SecLang via FFI into Coraza's Go-side parser; previously each plugin paid for the previous plugin's compile latency on top of its own. Diagnostic ordering is unchanged: per-plugin diagnostics are still emitted in input-array order. Plugin thread-safety contract (`Plugin: Send + Sync`, concurrent `&self` calls) is unchanged from the existing concurrent-connection case where multiple connection tasks already call the same plugin instance in parallel. New regression test `parallel_dispatch_within_request_runs_concurrently` pins the contract by injecting a 100ms sleep into two `[[plugins]]` entries and asserting wall-clock < 170ms (serial would be ~200ms+).

## [0.5.2] - 2026-05-05

### Added

- The `--validate-socket` orchestrator now schema-checks every plugin's `params` subtree against the plugin's own `config_schema()` BEFORE dispatching to `plugin.validate()`, mirroring the load-time path (`plugin_loader::load_plugin` already did this). Schema errors short-circuit and surface as `WireDiagnostic`s without ever calling the plugin's `validate()` body. Plugin authors writing `validate()` overrides can therefore rely on the `PluginContext` being structurally valid and limit themselves to *semantic* validation (e.g. compiling SecLang directives via Coraza). See the new contract section in the `define_plugin!` macro docstring (`crates/plugin-api/src/lib.rs`). Closes the gap that let the coraza plugin's `applications`-as-array-vs-table schema drift in v0.4.0 / v0.4.1 sneak past admission-time validation: the validator-socket path was reading raw `PluginContext` while the runtime parser disagreed about the schema shape, and there was no central enforcement.

### Changed

- `plugin_loader::collect_schema_errors` is the new shared helper for plugin schema validation. The pre-existing `validate_config_schema` (load-time) is now a thin wrapper around it; the orchestrator (validator-socket path) calls it directly. This is the canonical place for any future schema-related changes — keep it that way; do NOT add ad-hoc structural checks back into individual plugins.

## [0.5.1] - 2026-05-05

### Fixed

- Validator orchestrator now accepts the canonical `[[plugins]]` array-of-tables schema (same shape `HubConfig` deserialises at runtime startup), and only that shape. The previous code path called `as_table()` on `plugins`, which returns `None` when `plugins` is an array, causing the dispatcher to silently return zero diagnostics. Downstream HAPTIC's webhook accepted Ingresses carrying invalid `nginx.ingress.kubernetes.io/modsecurity-snippet` SecRules because the coraza plugin's `validate()` was never called. The whole point of `validate()` is to catch that at admission time.

### Changed

- **BREAKING (validator-mode TOML schema)**: dropped support for the flat `[plugins.params.<name>]` schema in validator-mode requests. The validator now only walks `[[plugins]]` arrays, matching what `HubConfig` parses at runtime startup. The legacy shape was unique to the original validate-socket spec and never matched the runtime hub config — keeping it would have meant validator-mode TOMLs that cannot be loaded by the runtime hub, defeating the point of validating with the same plugin set. Callers sending the legacy shape now receive a structured schema-shape error diagnostic. JSON wire protocol (`PROTOCOL_VERSION`) is unchanged; this only affects the TOML *content* shape inside `RequestFile`.

## [0.5.0] - 2026-05-04

### Changed

- **BREAKING (one-time ABI migration)**: `haproxy-spoa-hub-plugin-api` v0.5.0 replaces `abi_stable`'s `#[sabi_trait] SpoePlugin` + `PluginMod` prefix-type + `RootModule` machinery with a hand-rolled `#[repr(C)] PluginVTable` struct. Plugin source code is largely unchanged — `define_plugin!` keeps the same trait-impl-style block — but every existing plugin `.so` must be rebuilt against plugin-api 0.5.0+ to load on the new hub. The new model unblocks host-grows ABI compat: from 0.5.0 onward, adding a method to plugin-api keeps existing plugin `.so` files loadable. See `docs/adr/0001-abi-evolution.md` for design rationale and `docs/abi-evolution.md` for the contributor reference. Plugin authors: drop the direct `abi_stable = "0.11"` dependency from your `Cargo.toml`; it's no longer required (the macro routes through this crate's re-exports).
- **BREAKING for plugin authors**: `name()` and `version()` in the `define_plugin!` block must now return `&'static str`. In practice this means string literals (`"my-plugin"`) or `env!("CARGO_PKG_VERSION")`. Plugins that previously returned `&str` borrowed from instance fields (e.g. `&self.name_field`) need to either store the string as a `&'static str` (via `Box::leak(...)` at construction time) or convert it at compile time. The constraint comes from the FFI thunk's signature — `extern "C" fn` cannot carry the `&self` lifetime. None of the in-tree examples needed adjustment; check your own plugins.
- **Panic safety hardened**: every FFI thunk the macro emits — `create`, `init`, `process`, `name`, `version`, `config_schema`, `validate`, `shutdown`, `destroy` — is now wrapped in `std::panic::catch_unwind`. Previously only `process` was wrapped. A panic in any other plugin method would have aborted the entire hub (Rust treats unwinding through `extern "C"` as abort). This was especially exposed in `--validate-socket` mode, where the hub must survive a buggy plugin's `validate()` and return a structured diagnostic instead of crashing. Each thunk has a documented fallback for the catch case; see `crates/plugin-api/CLAUDE.md` for the table.
- `abi_stable` pinned to exact version `=0.11.3` in plugin-api and hub crates. `RString` / `RVec` / `RHashMap` layouts are now part of the FFI contract since the load-time check no longer runs on them; bumping the pin is a coordinated rollout.

### Added

- `crates/hub/src/plugin.rs::Plugin`: new loaded-plugin handle that owns the dlopened library, the vtable pointer, and the opaque state pointer. All FFI calls into plugins go through this struct's accessors so version-gating for future fields lives in one place.
- `docs/adr/0001-abi-evolution.md` and `docs/abi-evolution.md`: design rationale and contributor reference for the hand-rolled vtable model. Worked example for adding a new method without breaking old plugins.
- `crates/hub/tests/abi_matrix.rs`: regression test that loads every published plugin version against the current hub. Catches accidental field reorders, missed version bumps, and `abi_stable` layout drift.

### Removed

- `SpoePlugin` trait, `SpoePlugin_TO`, `PluginBox`, `PluginMod`, `PluginMod_Ref`, `RootModule for PluginMod_Ref`, `package_version_strings!()`, `declare_root_module_statics!()`, `abi_stable::library::lib_header_from_path` — replaced by the hand-rolled vtable. Plugin source code is unaffected; the macro's emitted FFI surface is what changed.
- `crates/plugin-api/tests/test_panic_safety.rs` — exercised the old `SpoePlugin` trait directly. Panic safety is now structurally enforced by `define_plugin!`'s `catch_unwind` wrapper around `process` and is the matrix test's responsibility to keep regression-testing.

## [0.4.0] - 2026-05-03

### Changed

- `--validate-socket` server now serves persistent keep-alive connections instead of closing after every request-response cycle. Each accepted connection loops on read-frame / dispatch / write-response until the client closes, the connection goes idle past the idle timeout (default 60s), shutdown is signalled, or a transport-level error poisons the byte stream. Application-level errors (malformed JSON, validation timeout) keep the connection open; only framing failures close it. Multiple connections from the same client run in parallel (one tokio task per accepted connection); within a single connection, requests are sequential pipelined per the HAPTIC-owned wire-protocol contract. Validation work now runs on tokio's blocking pool via `spawn_blocking` so a slow Coraza WAF init can't pin one of the small number of async I/O worker threads.
- The `specs/004-validate-mode/contracts/validate-socket-protocol.md` document is now a one-line pointer to the authoritative HAPTIC-owned wire-protocol spec at `haproxy-template-ic docs/development/validator-protocol.md`. The protocol's evolution rules, request/response schema, and connection-lifecycle semantics are decided in HAPTIC; this hub's `--validate-socket` server conforms.

## [0.3.0] - 2026-05-03

### Added

- `haproxy-spoa-hub-plugin-api` v0.3.0: `Diagnostic` and
  `DiagnosticSeverity` types in the public API for plugin-validation
  reporting, and a new `validate(&self, &PluginContext) -> RVec<Diagnostic>`
  trait method on `SpoePlugin` with a default empty-vec impl. Plugins
  that override the method surface line/column-precise diagnostics in
  the production hub's startup logs (and, once the `--validate-socket`
  mode lands, in admission webhook responses via HAPTIC's pluggable-
  validator integration). The `define_plugin!` macro accepts an
  optional `validate(&self, _: &PluginContext) -> Vec<Diagnostic>` body
  alongside the existing optional `config_schema`. See
  `specs/004-validate-mode/` for the full design.
- New `--validate-socket <path>` CLI flag puts the hub in validator-
  sidecar mode: binds a unix socket at the given path, accepts
  length-prefixed JSON requests carrying TOML config files, dispatches
  each `[plugins.params.<name>]` subtree to the matching plugin's
  `validate()` (without calling `init()`), and returns aggregated
  diagnostics. The SPOE listener is suppressed in this mode. Drives
  HAPTIC's pluggable-validator feature so broken plugin configs get
  rejected at admission time with line/column-precise feedback. Wire
  protocol documented in `specs/004-validate-mode/contracts/`.
- Production-mode hub startup now calls each plugin's `validate()`
  before its `init()`. Plugins that override `validate` get sharper
  startup errors (line and column where the plugin's parser supports
  it); plugins that don't override see no behaviour change.

### Changed

- `define_plugin!` macro restructured from two arms into a single arm
  with `$()?` matchers for both optional methods. Existing plugin code
  using the macro continues to work without changes — the macro accepts
  the same input it did before.

## [0.2.2] - 2026-04-28

### Fixed

- Release artifact-staging, blob-signing, and Generic Package Registry
  upload scripts now run on POSIX `/bin/sh` (Alpine `ash`) instead of
  requiring `bash`. The v0.2.1 tag pipeline failed at `sign-blobs` because
  the alpine signer image has no bash; the binary release artifacts were
  not published. Container image signing and SBOM attestation succeeded
  on v0.2.1 — only the binary publish path was affected.

## [0.2.1] - 2026-04-28

### Added

- Release artifacts (hub binary and example plugin `.so` files) published to
  the GitLab Generic Package Registry under
  `packages/generic/spoa-hub/<tag>/` with stable URLs
- Per-file `<artifact>.sha256` sidecars and a combined `SHA256SUMS` for
  batch verification with `sha256sum -c SHA256SUMS`
- Cosign keyless signatures (`<artifact>.cosign.bundle`) for every release
  artifact, signed via the GitLab OIDC id token (Sigstore Fulcio + Rekor)
- Cosign signature on the published multi-arch container image (signed by
  digest, never by mutable tag)
- CycloneDX SBOM attestation on the container image, generated with syft
  and signed via the same keyless flow
- `make verify-release VERSION=vX.Y.Z` target plus `scripts/verify-release.sh`
  for end-to-end consumer verification (downloads `SHA256SUMS`, checks all
  hashes, runs `cosign verify-blob` against the project's tag identity)
- Unsigned SLSA v0.2 provenance metadata emitted next to build artifacts
  via `RUNNER_GENERATE_ARTIFACTS_METADATA` (free SLSA-L1)

### Changed

- Release builds now use `cargo auditable build --release`, embedding a
  compressed `Cargo.lock`-equivalent in every binary so SBOM tools can
  enumerate Rust dependencies without source access

## [0.2.0] - 2026-04-12

### Added

- Plugin API convenience helpers: `SpoeMessage::get()`, `get_string()`,
  `get_int()`, `get_bool()`, `get_ipv4()`, `get_ipv6()`, `get_binary()`
  for O(1) typed argument lookup
- `TxnVariable::transaction()`, `session()`, `new()` scope-specific
  constructors
- `ProcessingResult::empty()`, `single()`, `from_vars()` result constructors
- `PluginContext::get_config()` for config key lookup accepting `&str`
- Re-exports of `RString`, `RVec`, `RHashMap`, `Tuple2` from the plugin
  API crate

### Changed

- Plugin API crate bumped to 0.2.0 (fully backward compatible with 0.1.0
  plugins)
- Updated example plugins to use the new convenience helpers

### Fixed

- README "Writing a Plugin" section now matches the actual current API

## [0.1.1] - 2026-04-08

### Changed

- Release builds now use LTO and single codegen unit for 6-30% better
  performance on type conversion and dispatch hot paths

## [0.1.0] - 2026-04-08

### Added

- Core SPOA hub binary with plugin dispatch architecture
- Plugin API crate (`haproxy-spoa-hub-plugin-api`) for building
  plugins as independently compiled shared libraries
- `define_plugin!` macro with automatic panic safety via
  `catch_unwind`
- SPOP protocol support (HAPROXY-HELLO, NOTIFY/ACK, DISCONNECT)
  via the `spop` crate
- Unix domain socket and TCP listener support
- Parallel plugin execution for independent plugins
- Plugin dependency declarations with topological execution ordering
- Per-plugin configurable timeouts with partial result return
- Automatic variable namespace prefixing (`<plugin>.<varname>`)
- TOML configuration with startup validation
- Structured JSON logging to stderr via `tracing`
- Prometheus metrics endpoint (`/metrics`) with per-plugin latency
  histograms
- ABI version checking at plugin load time via `abi_stable`
- Example plugins: noop, echo, delay
- Graceful shutdown on SIGTERM/SIGINT
- Per-request plugin skip via `hub.skip_plugins` SPOE message arg:
  HAProxy can skip specific plugins using ACLs, with zero processing
  overhead for skipped plugins. Transitive dependents are automatically
  cascaded.
- `hub.`-prefixed control arg namespace: all `hub.*` args are consumed
  by the hub and stripped from messages before plugins see them,
  providing a clean extensibility mechanism for future hub features
- `spoa_plugin_skips_total` Prometheus counter with `plugin` label
- Nested plugin configuration: `[plugins.params]` now supports all
  native TOML value types (integers, floats, booleans, arrays, nested
  tables) in addition to strings
- `ConfigValue` type in the plugin API for accessing typed configuration
  values with `as_str()`, `as_integer()`, `as_float()`, `as_bool()`,
  `as_array()`, and `as_table()` accessor methods
- Optional JSON Schema validation for plugin configuration: plugins can
  declare a schema via `config_schema()` and the hub validates config
  at startup before calling `init()`
- `define_plugin!` macro now accepts an optional `fn config_schema()`
  block for declaring a JSON Schema
- Forward-compatible `__Other` catch-all variants on `ConfigValue`,
  `SpoeValue`, and `VarScope` enums — new variants can now be added
  in minor versions without breaking existing plugins
