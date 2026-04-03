# Tasks: Nested Plugin Config with Schema Validation

**Input**: Design documents from `/specs/002-nested-config-schema/`
**Prerequisites**: plan.md (required), spec.md (required), research.md, data-model.md, contracts/

**Tests**: Required by project constitution (Principle II: "New features MUST have unit tests covering the primary path and at least one error path").

**Organization**: Tasks are grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)
- Include exact file paths in descriptions

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Add new dependencies needed by the hub crate

- [x] T001 Add `serde_json = "1"` and `jsonschema = { version = "0.45", default-features = false }` to `crates/hub/Cargo.toml`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Define the ConfigValue type and update PluginContext — MUST complete before any user story

- [x] T002 Define `ConfigValue` enum with 6 variants (String, Integer, Float, Bool, Array, Table) using `#[repr(u8)] #[derive(Debug, Clone, StableAbi)]` in `crates/plugin-api/src/types.rs`
- [x] T003 [P] Add accessor methods (`as_str`, `as_integer`, `as_float`, `as_bool`, `as_array`, `as_table`) on `ConfigValue` in `crates/plugin-api/src/types.rs`
- [x] T004 [P] Implement `PartialEq` manually for `ConfigValue` in `crates/plugin-api/src/types.rs` (needed for test assertions; `RHashMap` does not derive it)
- [x] T005 Update `PluginContext.config` type from `RHashMap<RString, RString>` to `RHashMap<RString, ConfigValue>` and update doc example in `crates/plugin-api/src/types.rs`
- [x] T006 Export `ConfigValue` from `crates/plugin-api/src/lib.rs`
- [x] T007 Change `PluginConfig.params` from `HashMap<String, String>` to `toml::Table` and remove unused `HashMap` import in `crates/hub/src/config.rs`

**Checkpoint**: ConfigValue type compiles, `cargo check --workspace` passes (tests may fail until US1 updates conversions)

---

## Phase 3: User Story 1 - Plugin author uses nested configuration (Priority: P1)

**Goal**: Plugins receive a typed config tree preserving all native TOML types across the FFI boundary

**Independent Test**: Write a TOML config with nested params and verify the plugin receives correctly typed values during `init()`

### Tests for User Story 1

> **NOTE: Write these tests FIRST, ensure they FAIL before implementation**

- [x] T008 [P] [US1] Add unit tests for `ConfigValue` accessor methods (each variant returns `Some` for correct type, `None` for wrong type) in `crates/plugin-api/src/types.rs`
- [x] T009 [P] [US1] Add unit tests for `toml_to_config_value()` covering all 7 TOML types (string, integer, float, bool, array, table, datetime) including nested structures in `crates/hub/src/plugin_loader.rs`

### Implementation for User Story 1

- [x] T010 [US1] Implement `toml_to_config_value()` and `toml_table_to_config()` recursive conversion functions in `crates/hub/src/plugin_loader.rs`
- [x] T011 [US1] Update `load_plugin()` to use `toml_table_to_config()` for params conversion (replace lines 62-65) in `crates/hub/src/plugin_loader.rs`
- [x] T012 [P] [US1] Update `test_plugin_params_parsed` to assert `toml::Value::String` in `crates/hub/src/config.rs`
- [x] T013 [P] [US1] Update `test_load_plugin_with_params` to use `toml::Table` instead of `HashMap<String, String>` in `crates/hub/src/plugin_loader.rs`
- [x] T014 [US1] Run `cargo test --workspace` and `cargo clippy --workspace` — all tests pass, no warnings

**Checkpoint**: Nested TOML configs are parsed and passed to plugins as typed ConfigValue trees. Flat string configs still work.

---

## Phase 4: User Story 2 - Plugin author declares a config schema (Priority: P2)

**Goal**: Plugins can optionally declare a JSON Schema; the hub validates config against it before calling `init()`

**Independent Test**: Provide a plugin with a schema, supply both valid and invalid configs, verify acceptance/rejection with appropriate errors

### Tests for User Story 2

> **NOTE: Write these tests FIRST, ensure they FAIL before implementation**

- [x] T015 [P] [US2] Add unit test: plugin with valid schema + valid config passes validation in `crates/hub/src/plugin_loader.rs`
- [x] T016 [P] [US2] Add unit test: plugin with valid schema + missing required field fails with `LoadError::Validation` in `crates/hub/src/plugin_loader.rs`
- [x] T017 [P] [US2] Add unit test: plugin with valid schema + wrong type fails validation in `crates/hub/src/plugin_loader.rs`
- [x] T018 [P] [US2] Add unit test: plugin with no schema (returns `RNone`) skips validation in `crates/hub/src/plugin_loader.rs`
- [x] T019 [US2] Add unit test: plugin with malformed schema string fails with schema parse error in `crates/hub/src/plugin_loader.rs`

### Implementation for User Story 2

- [x] T020 [US2] Add `config_schema(&self) -> ROption<RString>` method with default `RNone` after `shutdown()` in `crates/plugin-api/src/plugin_trait.rs`
- [x] T021 [US2] Add second `define_plugin!` macro arm supporting optional `fn config_schema(&self) -> Option<&str>` in `crates/plugin-api/src/lib.rs`
- [x] T022 [US2] Add `LoadError::Validation` variant to the `LoadError` enum in `crates/hub/src/plugin_loader.rs`
- [x] T023 [US2] Implement schema validation in `load_plugin()`: after `new()`, call `config_schema()`; if `RSome`, parse as JSON Schema, convert `toml::Table` to `serde_json::Value`, validate, return `LoadError::Validation` on failure in `crates/hub/src/plugin_loader.rs`
- [x] T024 [US2] Run `cargo test --workspace` and `cargo clippy --workspace` — all tests pass

**Checkpoint**: Schema validation works end-to-end. Plugins without schema unaffected.

---

## Phase 5: User Story 3 - Hub operator troubleshoots config errors (Priority: P3)

**Goal**: Schema validation errors include plugin name, field path, and human-readable constraint description

**Independent Test**: Provide invalid config and verify error output includes plugin name, field path, and constraint violated

### Tests for User Story 3

- [x] T025 [US3] Add unit test verifying validation error message includes plugin name, JSON path of failing field, and constraint description in `crates/hub/src/plugin_loader.rs`
- [x] T026 [US3] Add unit test verifying `load_all_plugins` skips plugin with invalid config and successfully loads other plugins in `crates/hub/src/plugin_loader.rs`

### Implementation for User Story 3

- [x] T027 [US3] Format `LoadError::Validation` display to include plugin name, all validation error paths, and constraint descriptions in `crates/hub/src/plugin_loader.rs`
- [x] T028 [US3] Run `cargo test --workspace` and `cargo clippy --workspace` — all tests pass

**Checkpoint**: Operators see actionable error messages when plugin config is invalid.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, cleanup, and full verification

- [x] T029 [P] Verify dead code removal: check for unused `HashMap` import in `crates/hub/src/config.rs` and unused `RString` import in `crates/hub/src/plugin_loader.rs`
- [x] T030 [P] Update CHANGELOG with breaking API change: `PluginContext.config` type changed from `RHashMap<RString, RString>` to `RHashMap<RString, ConfigValue>`; new optional `config_schema()` trait method
- [x] T031 Run `cargo clippy --workspace` and resolve any remaining warnings
- [x] T032 Run `cd tests/integration && make test` for Docker integration tests

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — can start immediately
- **Foundational (Phase 2)**: Depends on Setup completion — BLOCKS all user stories
- **User Story 1 (Phase 3)**: Depends on Foundational — BLOCKS US2 (schema validation needs ConfigValue conversion)
- **User Story 2 (Phase 4)**: Depends on US1 completion (schema validation uses the TOML→JSON conversion path)
- **User Story 3 (Phase 5)**: Depends on US2 completion (error formatting refines validation errors)
- **Polish (Phase 6)**: Depends on all user stories being complete

### Within Each User Story

- Tests written FIRST, verified to FAIL before implementation
- Conversion functions before callers
- Core implementation before integration
- Story complete (cargo test passes) before moving to next priority

### Parallel Opportunities

- T003 and T004 can run in parallel (both add impl blocks to ConfigValue)
- T008 and T009 can run in parallel (different files: types.rs vs plugin_loader.rs)
- T012 and T013 can run in parallel (different files: config.rs vs plugin_loader.rs)
- T015, T016, T017, T018 can run in parallel (independent test functions in same file)
- T029 and T030 can run in parallel (independent tasks)

---

## Parallel Example: User Story 1

```text
# After Phase 2, launch US1 tests in parallel:
T008: "Unit tests for ConfigValue accessors in crates/plugin-api/src/types.rs"
T009: "Unit tests for toml_to_config_value() in crates/hub/src/plugin_loader.rs"

# After T010-T011 implementation, update existing tests in parallel:
T012: "Update test_plugin_params_parsed in crates/hub/src/config.rs"
T013: "Update test_load_plugin_with_params in crates/hub/src/plugin_loader.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup (add dependencies)
2. Complete Phase 2: Foundational (ConfigValue type + PluginContext change)
3. Complete Phase 3: User Story 1 (TOML conversion + tests)
4. **STOP and VALIDATE**: `cargo test --workspace` and `cargo clippy --workspace`
5. Nested config works end-to-end; schema validation not yet available

### Incremental Delivery

1. Setup + Foundational -> ConfigValue type available
2. Add User Story 1 -> Nested config works -> Test independently (MVP)
3. Add User Story 2 -> Schema validation works -> Test independently
4. Add User Story 3 -> Error messages refined -> Test independently
5. Polish -> Full verification including Docker integration tests

---

## Notes

- [P] tasks = different files, no dependencies
- [Story] label maps task to specific user story for traceability
- US2 depends on US1 (schema validation uses conversion functions from US1)
- US3 depends on US2 (error formatting refines validation errors from US2)
- All 13 existing test sites using `params: Default::default()` compile unchanged (`toml::Table` has `Default`)
- Commit after each task or logical group
