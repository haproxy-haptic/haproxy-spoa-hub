# Tasks: HAProxy SPOA Plugin Hub

**Input**: Design documents from `/specs/001-spoa-plugin-hub/`
**Prerequisites**: plan.md (required), spec.md (required for user stories), research.md, data-model.md, contracts/

**Tests**: Included per constitution Principle II (Testing Standards): every behavioral change must be accompanied by automated tests.

**Organization**: Tasks are grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)
- Include exact file paths in descriptions

## Phase 1: Setup

**Purpose**: Project initialization and workspace structure

- [x] T001 Create workspace root Cargo.toml with members: crates/hub, crates/plugin-api, examples/noop
- [x] T002 Create hub crate Cargo.toml at crates/hub/Cargo.toml with dependencies: spop 0.11, tokio 1, tokio-util 0.7, futures 0.3, abi_stable 0.11, tracing 0.1, tracing-subscriber 0.3, metrics 0.24, metrics-exporter-prometheus 0.18, metrics-tracing-context 0.18, toml 0.8, serde 1, clap 4
- [x] T003 [P] Create plugin-api crate Cargo.toml at crates/plugin-api/Cargo.toml with dependencies: abi_stable 0.11
- [x] T004 [P] Create noop plugin crate Cargo.toml at examples/noop/Cargo.toml with crate-type=["cdylib"] and dependency on plugin-api
- [x] T005 [P] Configure clippy (clippy::pedantic) and rustfmt in workspace root (rustfmt.toml, .cargo/config.toml)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Plugin API types and hub infrastructure that ALL user stories depend on

**CRITICAL**: No user story work can begin until this phase is complete

- [x] T006 Define FFI-safe types (SpoeValue, SpoeMessage, ProcessingResult, TxnVariable, VarScope) with #[derive(StableAbi)] in crates/plugin-api/src/types.rs
- [x] T007 Define #[sabi_trait] SpoePlugin trait (init, process, name, version) and PluginMod root module struct with #[sabi(kind(Prefix))] in crates/plugin-api/src/plugin_trait.rs
- [x] T008 Implement define_plugin! macro that wraps process() in catch_unwind and generates #[export_root_module] boilerplate in crates/plugin-api/src/lib.rs
- [x] T009 Implement noop plugin using define_plugin! that sets one transaction variable in examples/noop/src/lib.rs
- [x] T010 [P] Define HubConfig, ListenerConfig, PluginConfig structs with serde Deserialize in crates/hub/src/config.rs
- [x] T011 [P] Set up structured JSON logging with tracing + tracing-subscriber (env-filter, JSON to stderr) in crates/hub/src/main.rs
- [x] T012 [P] Set up Prometheus metrics with metrics-exporter-prometheus (configurable HTTP endpoint, define counters/histograms/gauges) in crates/hub/src/metrics.rs
- [x] T013 Implement CLI argument parsing (--config, --check, --version) with clap in crates/hub/src/main.rs

**Checkpoint**: `cargo build --workspace` succeeds, noop plugin compiles to .so

---

## Phase 3: User Story 1 - Operator Deploys Hub with Plugins (Priority: P1)

**Goal**: Hub loads plugins, accepts one SPOP connection, dispatches messages to a single plugin, returns ACK with namespaced variables

**Independent Test**: Start hub with noop plugin, send SPOP NOTIFY via mock client, verify ACK contains `noop.result` variable

### Tests for User Story 1

> **NOTE: Write these tests FIRST, ensure they FAIL before implementation**

- [x] T014 [P] [US1] Unit tests for config validation (missing fields, invalid plugin_dir, duplicate names, non-existent .so) in crates/hub/src/config.rs
- [x] T015 [P] [US1] Unit tests for plugin_loader (successful load, ABI version mismatch rejection, invalid .so rejection) in crates/hub/src/plugin_loader.rs
- [x] T016 [P] [US1] Integration test: start hub with noop plugin, send NOTIFY via mock SPOP client, verify ACK variables in tests/integration/test_single_plugin.rs (create tests/integration/spop_client.rs mock helper and tests/common/mod.rs test fixtures)

### Implementation for User Story 1

- [x] T017 [US1] Implement config validation (all 9 rules from contracts/configuration.md) with actionable error messages in crates/hub/src/config.rs
- [x] T018 [US1] Implement plugin loading via abi_stable load_from_file() with ABI version check and error reporting in crates/hub/src/plugin_loader.rs
- [x] T019 [US1] Implement PluginRegistry: store loaded plugins by name, build message_map (message name → plugin list) in crates/hub/src/registry.rs
- [x] T020 [US1] Implement Unix domain socket listener (bind, accept loop, cleanup stale socket) in crates/hub/src/listener.rs
- [x] T021 [US1] Implement per-connection SPOP handler: HAPROXY-HELLO/AGENT-HELLO handshake, NOTIFY frame processing, HAPROXY-DISCONNECT in crates/hub/src/connection.rs
- [x] T022 [US1] Implement single-plugin message dispatch: convert spop TypedData → SpoeValue, call plugin.process(), prefix variables with plugin name, build ACK in crates/hub/src/dispatch.rs
- [x] T023 [US1] Wire main.rs: parse CLI → load config → validate → init logging/metrics → load plugins → build registry → start listener → accept loop in crates/hub/src/main.rs

**Checkpoint**: Hub starts, loads noop plugin, accepts SPOP connection on Unix socket, dispatches NOTIFY to plugin, returns ACK with `noop.result` variable

---

## Phase 4: User Story 2 - Parallel Plugin Execution (Priority: P1)

**Goal**: Multiple independent plugins execute concurrently for the same message; dependent plugins execute after dependencies; per-plugin timeouts with partial results

**Independent Test**: Two test plugins each sleep 5ms and set distinct variables. Total time ~5ms (parallel), both variables in ACK.

### Tests for User Story 2

> **NOTE: Write these tests FIRST, ensure they FAIL before implementation**

- [x] T024 [P] [US2] Unit tests for ExecutionPlan builder: independent plugins → single parallel group, dependent plugins → ordered groups, cyclic dependency → error in crates/hub/src/registry.rs
- [x] T025 [P] [US2] Unit tests for parallel dispatch: timeout returns partial results, failed plugin doesn't block others, dependency data flow merges variables in crates/hub/src/dispatch.rs
- [x] T026 [P] [US2] Integration test: two parallel plugins with timing assertion, one failing plugin with partial results, dependency chain in tests/integration/test_parallel.rs (create examples/delay/ test plugin that sleeps configurable duration)

### Implementation for User Story 2

- [x] T027 [US2] Implement ExecutionPlan builder: topological sort of plugin dependency graph, group independent plugins for parallel execution in crates/hub/src/registry.rs
- [x] T028 [US2] Implement parallel dispatch: tokio::spawn per plugin within a group, join all, collect results in crates/hub/src/dispatch.rs
- [x] T029 [US2] Implement per-plugin timeout using tokio::time::timeout; on timeout, return partial results from completed plugins and log timeout in crates/hub/src/dispatch.rs
- [x] T030 [US2] Implement dependency data flow: merge upstream plugin's namespace-prefixed output variables into downstream plugin's SpoeMessage.args in crates/hub/src/dispatch.rs
- [x] T031 [US2] Implement plugin error isolation: catch RErr from plugin.process(), log error with plugin name, continue collecting results from other plugins in crates/hub/src/dispatch.rs
- [x] T032 [US2] Create delay test plugin (configurable sleep + variable set) for parallel execution testing in examples/delay/src/lib.rs and examples/delay/Cargo.toml

**Checkpoint**: Two independent plugins run in parallel (~5ms not ~10ms). Failed plugin returns partial results. Dependent plugin receives upstream output.

---

## Phase 5: User Story 3 - Unix Domain Socket and TCP Listening (Priority: P1)

**Goal**: Hub listens on both Unix socket and TCP simultaneously; both transports handle SPOP identically

**Independent Test**: Start hub with both transports configured, connect mock SPOP client to each, verify identical behavior

### Tests for User Story 3

- [x] T033 [P] [US3] Unit tests for listener config parsing and socket bind (unix path, tcp address, both) in crates/hub/src/listener.rs
- [x] T034 [P] [US3] Integration test: connect to Unix socket and TCP in parallel, send NOTIFY on both, verify identical ACK responses in tests/integration/test_transport.rs

### Implementation for User Story 3

- [x] T035 [US3] Implement TCP listener support in crates/hub/src/listener.rs
- [x] T036 [US3] Implement concurrent multi-listener accept loop: spawn a task per configured listener, all feed connections into shared connection handler in crates/hub/src/listener.rs
- [x] T037 [US3] Update main.rs to start all configured listeners concurrently in crates/hub/src/main.rs

**Checkpoint**: Hub accepts connections on both Unix socket and TCP simultaneously. SPOP messages processed identically on both transports.

---

## Phase 6: User Story 4 - Plugin Developer Creates a New Plugin (Priority: P2)

**Goal**: A developer can create, build, and deploy a plugin using only the published plugin-api crate and its documentation

**Independent Test**: Create a new plugin from scratch using only plugin-api docs, compile it, load it in the hub

### Tests for User Story 4

- [x] T038 [P] [US4] Unit tests for define_plugin! macro: panic inside process() returns RErr not abort, plugin factory creates valid instance in crates/plugin-api/tests/
- [x] T039 [P] [US4] Integration test: load an independently compiled echo plugin that returns all args as variables in tests/integration/test_echo_plugin.rs

### Implementation for User Story 4

- [x] T040 [US4] Add comprehensive rustdoc with examples to all public types (SpoeValue, SpoeMessage, ProcessingResult, TxnVariable, VarScope) in crates/plugin-api/src/types.rs
- [x] T041 [US4] Add comprehensive rustdoc with examples to SpoePlugin trait and define_plugin! macro in crates/plugin-api/src/plugin_trait.rs and crates/plugin-api/src/lib.rs
- [x] T042 [US4] Create echo example plugin that exercises all SpoeValue variants and returns them as variables in examples/echo/src/lib.rs and examples/echo/Cargo.toml
- [x] T043 [US4] Verify plugin-api crate compiles standalone (simulate crates.io publish with cargo package --allow-dirty) in crates/plugin-api/

**Checkpoint**: Echo plugin compiles using only plugin-api dependency. Hub loads it without recompilation. Rustdoc builds cleanly with `cargo doc --no-deps`.

---

## Phase 7: User Story 5 - Hub Provides Pre-Parsed Data to Plugins (Priority: P2)

**Goal**: Plugins receive all SPOE message arguments as correctly typed SpoeValue variants and connection metadata as structured fields

**Independent Test**: Send SPOE message with all type variants (int, string, binary, IPv4, IPv6). Verify echo plugin returns them correctly.

### Tests for User Story 5

- [x] T044 [P] [US5] Unit tests for spop TypedData → SpoeValue conversion covering all 10 variants (Null, Bool, Int32, Uint32, Int64, Uint64, Ipv4, Ipv6, String, Binary) in crates/hub/src/dispatch.rs
- [x] T045 [P] [US5] Integration test: send NOTIFY with all typed args, verify echo plugin receives correct types and values in tests/integration/test_echo_plugin.rs

### Implementation for User Story 5

- [x] T046 [US5] Ensure spop TypedData → SpoeValue conversion handles all type variants including edge cases (empty strings, zero-length binary, IPv4-mapped IPv6) in crates/hub/src/dispatch.rs
- [x] T047 [US5] Extract and pass connection metadata (stream_id, frame_id) from SPOP NOTIFY frame to SpoeMessage in crates/hub/src/connection.rs

**Checkpoint**: Echo plugin receives all argument types correctly. stream_id and frame_id are populated.

---

## Phase 8: User Story 6 - Graceful Shutdown and Reload (Priority: P3)

**Goal**: SIGTERM/SIGINT drains in-flight messages before exit. SIGHUP reloads config and plugins without dropping connections.

**Independent Test**: Send SIGTERM during active request processing. Verify in-flight messages complete and hub exits cleanly.

### Tests for User Story 6

- [x] T048 [P] [US6] Integration test: send SIGTERM while requests in flight, verify in-flight messages complete and hub exits with code 0 in tests/integration/scripts/test_sigterm.sh
- [x] T049 [P] [US6] Integration test: send SIGHUP to add new plugin, verify new plugin available for subsequent requests in tests/integration/scripts/test_sighup.sh

### Implementation for User Story 6

- [x] T050 [US6] Implement SIGTERM/SIGINT handler using tokio::signal: stop accepting new connections, drain in-flight messages with configurable timeout, exit cleanly in crates/hub/src/main.rs
- [x] T051 [US6] Implement SIGHUP handler: re-read config, diff plugins (added/removed/unchanged), load new plugins, unload removed plugins, swap registry atomically in crates/hub/src/main.rs
- [x] T052 [US6] Make PluginRegistry swappable via ArcSwap or RwLock for atomic hot-swap during SIGHUP reload in crates/hub/src/registry.rs

**Checkpoint**: SIGTERM drains in-flight work. SIGHUP loads new plugins without restart.

---

## Phase 9: Polish & Cross-Cutting Concerns

**Purpose**: Performance validation, documentation, security hardening

- [x] T053 [P] Add criterion benchmarks for dispatch loop (single plugin, parallel plugins, variable aggregation) in benches/dispatch_bench.rs
- [x] T054 [P] Add criterion benchmarks for TypedData → SpoeValue conversion in benches/conversion_bench.rs
- [x] T054b [P] Load test: verify 10k concurrent SPOP connections with stable p99 latency (SC-004) using a custom SPOP load generator or shell script in tests/integration/scripts/test_load.sh
- [x] T055 [P] Create CHANGELOG.md with initial release notes at repository root
- [x] T056 Security hardening: set Unix socket permissions (0o660), validate config file permissions at startup in crates/hub/src/listener.rs and crates/hub/src/config.rs
- [x] T057 Run quickstart.md validation end-to-end (build, configure, run hub, send traffic via curl) in tests/integration/scripts/test_quickstart.sh
- [x] T058 Code cleanup: remove dead code, consolidate duplicates, verify naming accuracy across all modules

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — can start immediately
- **Foundational (Phase 2)**: Depends on Setup completion — BLOCKS all user stories
- **US1 (Phase 3)**: Depends on Foundational — core hub operation
- **US2 (Phase 4)**: Depends on US1 — extends dispatch with parallelism
- **US3 (Phase 5)**: Depends on US1 — extends listener with TCP
- **US4 (Phase 6)**: Depends on US1 — documents and validates plugin API
- **US5 (Phase 7)**: Depends on US1 — ensures comprehensive type handling
- **US6 (Phase 8)**: Depends on US1, US2 — adds lifecycle management
- **Polish (Phase 9)**: Depends on all desired user stories being complete

### User Story Dependencies

- **US1 (P1)**: Can start after Foundational (Phase 2) — no dependencies on other stories
- **US2 (P1)**: Depends on US1 — extends the dispatch module
- **US3 (P1)**: Can start after US1 — independent from US2. Can run in parallel with US2.
- **US4 (P2)**: Can start after US1 — independent from US2/US3
- **US5 (P2)**: Can start after US1 — independent from US2/US3/US4
- **US6 (P3)**: Depends on US1 and US2 — needs registry and dispatch to be complete

### Within Each User Story

- Tests MUST be written and FAIL before implementation
- Config/types before services
- Services before dispatch/connection
- Core implementation before integration wiring
- Story complete before moving to next priority

### Parallel Opportunities

- Setup: T003, T004, T005 can run in parallel
- Foundational: T010, T011, T012 can run in parallel (after T006-T009)
- US1: T014, T015, T016 tests can run in parallel
- US2: T024, T025, T026 tests can run in parallel
- US3 and US4 can run in parallel (both depend only on US1)
- US5 can run in parallel with US3/US4
- Polish: T053, T054, T055 can run in parallel

---

## Parallel Example: User Story 2

```bash
# Launch all tests for User Story 2 together:
Task: "Unit tests for ExecutionPlan builder in crates/hub/src/registry.rs"
Task: "Unit tests for parallel dispatch in crates/hub/src/dispatch.rs"
Task: "Integration test for parallel execution in tests/integration/test_parallel.rs"

# After tests fail, launch parallel implementations:
Task: "ExecutionPlan builder in crates/hub/src/registry.rs"
Task: "Create delay test plugin in examples/delay/"
# Then sequentially:
Task: "Parallel dispatch in crates/hub/src/dispatch.rs"
Task: "Per-plugin timeout in crates/hub/src/dispatch.rs"
Task: "Dependency data flow in crates/hub/src/dispatch.rs"
Task: "Error isolation in crates/hub/src/dispatch.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup
2. Complete Phase 2: Foundational (CRITICAL — blocks all stories)
3. Complete Phase 3: User Story 1
4. **STOP and VALIDATE**: Hub loads a plugin, dispatches one message, returns ACK
5. Deploy/demo if ready

### Incremental Delivery

1. Setup + Foundational → Foundation ready
2. Add US1 → Single-plugin hub works (MVP!)
3. Add US2 → Parallel plugin execution
4. Add US3 → TCP transport support
5. Add US4 → Plugin developer documentation
6. Add US5 → Comprehensive type handling
7. Add US6 → Production lifecycle management
8. Polish → Benchmarks, security, docs

### Parallel Team Strategy

With multiple developers:

1. Team completes Setup + Foundational together
2. One developer: US1 (must complete first)
3. After US1:
   - Developer A: US2 (parallel execution)
   - Developer B: US3 (TCP) + US4 (plugin docs)
   - Developer C: US5 (type handling)
4. After US2: Developer A → US6 (shutdown/reload)
5. Everyone: Polish phase

---

## Notes

- [P] tasks = different files, no dependencies
- [Story] label maps task to specific user story for traceability
- Each user story should be independently completable and testable
- Verify tests fail before implementing
- Commit after each task or logical group
- Stop at any checkpoint to validate story independently
- The constitution requires tests for all behavioral changes — tests are NOT optional
- The define_plugin! macro is critical for panic safety (FR-013) — implement early
