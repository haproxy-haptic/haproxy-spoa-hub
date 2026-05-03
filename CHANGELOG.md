# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
