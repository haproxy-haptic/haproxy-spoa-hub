# Tasks: Hub Control Args for Per-Request Plugin Skip

**Input**: Design documents from `/specs/003-hub-control-args/`
**Prerequisites**: plan.md (required), spec.md (required), research.md, data-model.md, quickstart.md

**Tests**: Required by project constitution (Principle II: "New features MUST have unit tests covering the primary path and at least one error path").

**Organization**: Tasks are grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)
- Include exact file paths in descriptions

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: No new dependencies needed. This feature modifies only the hub crate.

- [x] T001 Store `depends_on` map as a field in `PluginRegistry` and add `dependencies_of()` and `all_dependencies()` accessors in `crates/hub/src/registry.rs`
- [x] T002 Add `record_plugin_skip()` counter and `describe_counter!` for `spoa_plugin_skips_total` in `crates/hub/src/metrics.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: No additional foundational work needed beyond Phase 1. The registry and metrics changes are the foundation.

**Checkpoint**: `cargo check -p haproxy-spoa-hub` passes with new registry field and metrics function.

---

## Phase 3: User Story 1 - Operator skips a plugin for allowlisted traffic (Priority: P1)

**Goal**: HAProxy sends `hub.skip_plugins` to skip named plugins per-request with zero processing overhead for skipped plugins

**Independent Test**: Send a message with `hub.skip_plugins` naming a plugin, verify its output variables do not appear in the result

### Tests for User Story 1

> **NOTE: Write these tests FIRST, ensure they FAIL before implementation**

- [x] T003 [P] [US1] Add unit test `test_extract_hub_args_parses_skip_plugins` verifying `"a,b,c"` produces skip set `{"a","b","c"}` in `crates/hub/src/dispatch.rs`
- [x] T004 [P] [US1] Add unit test `test_extract_hub_args_trims_whitespace` verifying `"a , b"` produces `{"a","b"}` in `crates/hub/src/dispatch.rs`
- [x] T005 [P] [US1] Add unit test `test_extract_hub_args_empty_when_missing` verifying no `hub.skip_plugins` arg produces empty skip set in `crates/hub/src/dispatch.rs`
- [x] T006 [P] [US1] Add unit test `test_extract_hub_args_ignores_non_string` verifying `hub.skip_plugins = Bool(true)` produces empty skip set in `crates/hub/src/dispatch.rs`

### Implementation for User Story 1

- [x] T007 [US1] Add `HUB_ARG_PREFIX` constant and `extract_hub_args()` function that returns `(HashSet<String>, SpoeMessage)` in `crates/hub/src/dispatch.rs`
- [x] T008 [US1] Modify `dispatch_message()` to call `extract_hub_args()` at entry, filter skipped plugins from each execution group, and use cleaned message for plugin calls in `crates/hub/src/dispatch.rs`
- [x] T009 [US1] Add integration test `test_dispatch_skips_named_plugin` using real noop plugin: set `hub.skip_plugins = "noop"`, verify empty result in `crates/hub/src/dispatch.rs`
- [x] T010 [US1] Run `cargo test --workspace` and `cargo clippy --workspace` -- all tests pass

**Checkpoint**: Sending `hub.skip_plugins` skips named plugins. No cascade yet.

---

## Phase 4: User Story 2 - Hub strips control args from plugin messages (Priority: P2)

**Goal**: All `hub.*` args are removed from the message before plugins see it

**Independent Test**: Send a message with both `hub.*` and regular args, verify plugin only sees regular args

### Tests for User Story 2

- [x] T011 [US2] Add unit test `test_extract_hub_args_strips_hub_prefix` verifying `hub.*` args are removed and regular args kept in cleaned message in `crates/hub/src/dispatch.rs`

### Implementation for User Story 2

- [x] T012 [US2] Verify `extract_hub_args()` already strips all `hub.*` args from the cleaned message (implemented in T007). If not, update the function in `crates/hub/src/dispatch.rs`
- [x] T013 [US2] Run `cargo test --workspace` -- all tests pass

**Checkpoint**: Plugins never see `hub.*` args regardless of how many HAProxy sends.

---

## Phase 5: User Story 3 - Skipped plugin cascades to its dependents (Priority: P3)

**Goal**: Skipping an upstream plugin automatically skips all its transitive dependents

**Independent Test**: Skip an upstream plugin, verify its dependent is also skipped without being explicitly named

### Tests for User Story 3

- [x] T014 [P] [US3] Add unit test `test_expand_skip_cascade_skips_direct_dependent` verifying skipping A cascades to B when B depends on A in `crates/hub/src/dispatch.rs`
- [x] T015 [P] [US3] Add unit test `test_expand_skip_cascade_preserves_independent` verifying skipping A does NOT cascade to unrelated C in `crates/hub/src/dispatch.rs`

### Implementation for User Story 3

- [x] T016 [US3] Add `expand_skip_cascade()` function that expands the skip set by adding plugins whose dependencies include any skipped plugin in `crates/hub/src/dispatch.rs`
- [x] T017 [US3] Call `expand_skip_cascade()` after processing each execution group in `dispatch_message()` in `crates/hub/src/dispatch.rs`
- [x] T018 [US3] Run `cargo test --workspace` and `cargo clippy --workspace` -- all tests pass

**Checkpoint**: Full skip with cascade works. All three user stories verified.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, cleanup, and full verification

- [x] T019 [P] Add `hub.` control args section to README.md after "Plugin dependencies" section with HAProxy ACL examples, cascade behavior, and `hub.skip_plugins` usage
- [x] T020 [P] Update CHANGELOG.md with new feature: `hub.skip_plugins` control arg, `hub.*` arg stripping, cascade behavior, `spoa_plugin_skips_total` metric
- [x] T021 Run `cargo clippy --workspace` and resolve any warnings
- [x] T022 Run `cd tests/integration && make test` for Docker integration tests

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies -- can start immediately
- **Foundational (Phase 2)**: Depends on Phase 1 completion
- **User Story 1 (Phase 3)**: Depends on Phase 1 (registry + metrics)
- **User Story 2 (Phase 4)**: Depends on US1 (stripping is part of `extract_hub_args()`)
- **User Story 3 (Phase 5)**: Depends on US1 (cascade extends the skip logic)
- **Polish (Phase 6)**: Depends on all user stories being complete

### Within Each User Story

- Tests written FIRST, verified to FAIL before implementation
- Functions before callers
- Story complete (cargo test passes) before moving to next priority

### Parallel Opportunities

- T001 and T002 can run in parallel (different files: registry.rs vs metrics.rs)
- T003, T004, T005, T006 can run in parallel (independent test functions in same file)
- T014 and T015 can run in parallel (independent test functions)
- T019 and T020 can run in parallel (different files: README.md vs CHANGELOG.md)

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup (registry + metrics)
2. Complete Phase 3: User Story 1 (skip named plugins)
3. **STOP and VALIDATE**: `cargo test --workspace` and `cargo clippy --workspace`
4. `hub.skip_plugins` works for explicit plugin names. No cascade, but functional.

### Incremental Delivery

1. Setup -> Registry stores depends_on, metrics ready
2. Add User Story 1 -> Skip named plugins works -> Test independently (MVP)
3. Add User Story 2 -> Verify arg stripping -> Test independently
4. Add User Story 3 -> Cascade works -> Test independently
5. Polish -> Docs, clippy, integration tests

---

## Notes

- [P] tasks = different files, no dependencies
- [Story] label maps task to specific user story for traceability
- US2 builds on US1 (arg stripping is part of the same extract function)
- US3 builds on US1 (cascade extends the skip filtering in dispatch_message)
- No plugin-api changes, no new dependencies, no hub config changes
- Commit after each task or logical group
