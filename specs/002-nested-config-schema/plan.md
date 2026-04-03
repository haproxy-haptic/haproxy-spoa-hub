# Implementation Plan: Nested Plugin Config with Schema Validation

**Branch**: `002-nested-config-schema` | **Date**: 2026-04-03 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/002-nested-config-schema/spec.md`

## Summary

Plugin parameters are currently flat string-to-string maps. This feature replaces them with a recursive `ConfigValue` type that preserves all native TOML types (strings, integers, floats, booleans, arrays, nested tables) across the FFI boundary. Additionally, plugins can optionally declare a JSON Schema that the hub validates against before calling `init()`.

## Technical Context

**Language/Version**: Rust, Edition 2024, MSRV 1.85
**Primary Dependencies**: `abi_stable` 0.11 (FFI), `toml` 1.0 (config), `serde_json` 1.0 (schema conversion), `jsonschema` 0.45 (validation)
**Storage**: N/A (TOML config files, read-only at startup)
**Testing**: `cargo test --workspace` (unit + integration), Docker integration tests (`tests/integration/`)
**Target Platform**: Linux server (x86_64)
**Project Type**: Library + daemon (Cargo workspace with `plugin-api` library crate and `hub` binary crate)
**Performance Goals**: Config loading is startup-only; no impact on request-path latency
**Constraints**: All types crossing FFI must derive `StableAbi`; recursive types require heap-allocated containers (`RVec`, `RHashMap`)
**Scale/Scope**: ~10 files modified, 2 new dependencies in hub crate

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Code Quality | Pass | Public `ConfigValue` API will be documented. Single responsibility maintained: types.rs defines types, plugin_loader.rs handles conversion. |
| II. Testing Standards | Pass | Unit tests for `ConfigValue` accessors and TOML-to-ConfigValue conversion. Unit tests for schema validation pass/fail. Integration tests via existing Docker suite. |
| III. User Experience Consistency | Pass | Schema validation errors at startup with plugin name + field path + constraint description (FR-006). Config validated at startup, not first use (constitution III rule 4). Breaking change documented. |
| IV. Performance Requirements | Pass | Config parsing is startup-only, not on the request path. No impact on p99 latency. No new benchmarks required (not critical path). |

No violations. Complexity Tracking section not needed.

## Project Structure

### Documentation (this feature)

```text
specs/002-nested-config-schema/
├── plan.md              # This file
├── spec.md              # Feature specification
├── research.md          # Phase 0: dependency research
├── data-model.md        # Phase 1: ConfigValue type model
├── contracts/
│   └── plugin-api.md    # Phase 1: updated plugin API contract
├── checklists/
│   └── requirements.md  # Spec quality checklist
└── tasks.md             # Phase 2 output (created by /speckit.tasks)
```

### Source Code (repository root)

```text
crates/plugin-api/
├── Cargo.toml                    # No new deps (ConfigValue uses abi_stable only)
├── src/
│   ├── lib.rs                    # Export ConfigValue; update define_plugin! macro
│   ├── types.rs                  # Add ConfigValue enum + accessors; update PluginContext
│   └── plugin_trait.rs           # Add config_schema() method after shutdown()
└── tests/
    └── test_panic_safety.rs      # Add config_schema to manual SpoePlugin impls

crates/hub/
├── Cargo.toml                    # Add serde_json, jsonschema
├── src/
│   ├── config.rs                 # Change params: HashMap<String,String> → toml::Table
│   └── plugin_loader.rs          # TOML→ConfigValue conversion; schema validation
└── tests/                        # Existing integration tests (params: Default::default() unchanged)

examples/noop/src/lib.rs          # No change (ignores context)
examples/delay/src/lib.rs         # No change (ignores context)
examples/echo/src/lib.rs          # No change (ignores context)
```

**Structure Decision**: Existing workspace structure. Changes span the `plugin-api` and `hub` crates. No new crates or modules needed.
