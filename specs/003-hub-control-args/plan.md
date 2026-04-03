# Implementation Plan: Hub Control Args for Per-Request Plugin Skip

**Branch**: `003-hub-control-args` | **Date**: 2026-04-03 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/003-hub-control-args/spec.md`

## Summary

Add a `hub.`-prefixed control arg namespace to SPOE messages. The hub strips all `hub.*` args before passing messages to plugins. The first control arg, `hub.skip_plugins`, allows HAProxy to skip specific plugins per-request using its native ACL engine, with automatic transitive cascade to downstream dependents.

## Technical Context

**Language/Version**: Rust, Edition 2024, MSRV 1.85
**Primary Dependencies**: `abi_stable` 0.11 (FFI), `spop` 0.11 (SPOE protocol), `tokio` 1 (async runtime)
**Storage**: N/A
**Testing**: `cargo test --workspace` (unit + integration), Docker integration tests (`tests/integration/`)
**Target Platform**: Linux server (x86_64)
**Project Type**: Library + daemon (Cargo workspace)
**Performance Goals**: Zero overhead for skipped plugins (no spawn_blocking, no FFI). Arg stripping is O(n) in number of args per message, negligible vs plugin execution.
**Constraints**: No new dependencies. No plugin-api changes (zero ABI impact). No hub config changes.
**Scale/Scope**: 3 files modified in hub crate, README and CHANGELOG updates

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Code Quality | Pass | `extract_hub_args()` and `expand_skip_cascade()` are single-responsibility functions. Public behavior documented in README. |
| II. Testing Standards | Pass | Unit tests for arg extraction (primary + error paths). Integration test for skip behavior with real plugins. |
| III. User Experience Consistency | Pass | Debug-level structured log for each skipped plugin (FR-007). `hub.` prefix follows existing dot-notation convention (variable namespacing). Changelog updated. |
| IV. Performance Requirements | Pass | Skip check is a string comparison per plugin per group — negligible. Skipped plugins incur zero spawn_blocking/FFI overhead. Benchmark regression not expected (dispatch hot path adds one HashSet lookup per plugin). |

No violations. Complexity Tracking not needed.

## Project Structure

### Documentation (this feature)

```text
specs/003-hub-control-args/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0: no unknowns, documents design decisions
├── data-model.md        # Phase 1: skip set and hub control arg model
├── quickstart.md        # Phase 1: HAProxy configuration examples
├── checklists/
│   └── requirements.md  # Spec quality checklist
└── tasks.md             # Phase 2 output (created by /speckit.tasks)
```

### Source Code (repository root)

```text
crates/hub/
├── src/
│   ├── dispatch.rs          # extract_hub_args(), expand_skip_cascade(), modified dispatch_message()
│   ├── registry.rs          # Store depends_on map, add dependencies_of() accessor
│   └── metrics.rs           # Add record_plugin_skip() counter
└── tests/                   # Existing integration tests (unchanged)
```

**Structure Decision**: All changes are within the hub crate. No new modules or files needed. No plugin-api changes.
