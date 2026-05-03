# Feature Specification: Validator Sidecar Mode and Deep Plugin Validate

**Feature Branch**: `004-validate-mode`
**Created**: 2026-05-03
**Status**: Draft
**Input**: User description: "Validator sidecar mode and deep plugin validate API for pre-push config validation"

## Background

Operators downstream of the hub — primarily the HAProxy Template Ingress Controller (HAPTIC) — need to validate hub configuration *before* pushing it to a production hub. Today HAPTIC renders `haproxy.cfg` and validates it via `haproxy -c` before pushing to the dataplane API; the same kind of round-trip doesn't exist for hub TOML. As a result:

- A broken `[plugins.params]` subtree (typo'd directive, malformed Coraza SecLang, mis-spelled OTLP endpoint) silently degrades the hub when pushed.
- HAPTIC's admission webhook can't reject Ingress changes that would render an invalid hub config, so users only discover the breakage in production logs.
- Surfacing precise line/column-level errors back to operators (`kubectl apply` output, CRD status) requires a validation channel that knows how to parse plugin-specific config.

This feature adds two coupled pieces:

1. **A pure `validate(context)` method on the plugin API.** Plugins that override it perform deep validation of their own `[plugins.params]` subtree without side effects (no tokio tasks, no network I/O, no file handles beyond the inputs) and return per-finding diagnostics with line, column, and message. Default implementation returns empty diagnostics so existing plugins are unaffected. Spec 002's static `config_schema()` (JSON Schema) check remains; this adds a deeper, plugin-specific tier.

2. **A `--validate-socket <path>` hub binary mode.** The same hub binary, launched with a socket path argument, loads its configured plugins, opens a unix socket on the given path, and accepts JSON requests containing one or more files to validate. Each request is parsed, dispatched to the matching plugin's `validate()`, and the aggregated diagnostics returned. The sidecar runs alongside the haptic-controller pod (chart wires the same image and tag as the production hub container, ensuring lockstep). HAPTIC's admission webhook and reconcile loop call this socket before pushing config or admitting Ingresses.

The validator sidecar reuses the same hub binary, the same plugin .so files, and the same plugin code paths as production. It diverges only in not calling `init()` (which has side effects). Drift is structurally impossible: validation runs the same code as production, just without the post-validate step.

## Clarifications

### Session 2026-05-03

- Q: Should the validator sidecar be a separate binary, a subcommand of the hub, or a flag on the existing entry point? → A: A flag (`--validate-socket <path>`) on the existing hub binary. Reuses the binary, the plugin loader, and the config-parsing code. The chart can launch the same image with different args.
- Q: How does the hub-side `init() = validate() + side-effects` invariant get enforced? → A: The hub's startup orchestrator wraps every plugin's init in a "validate first, init second" sequence. Plugin authors don't need to remember to call validate themselves; production hub always validates before init. Because validator sidecar only calls validate (never init), and production hub always validates before init, the same `validate()` code runs in both places — no drift possible.
- Q: How are diagnostics framed for files where the parser doesn't have line/column information? → A: Use line `0` and column `0` to indicate "unknown / file-level". Validators MUST populate line/column when the underlying parser supports it (Go `toml` does, Coraza's SecLang parser supports line via captured logger but not column). For purely file-level diagnostics (e.g., "plugin not loaded"), `line=0, column=0` is the convention.
- Q: How does the validator sidecar bootstrap its own plugin set? → A: The chart renders the validator sidecar's `--config` argument from the same plugin list as the production hub container. Both use the same image and tag, both load the same .so files. The validator's bootstrap TOML lists `[[plugins]]` declarations only — no `[plugins.params]` (params come per-validation-request from HAPTIC). Production hub and validator sidecar are guaranteed to have the same plugin set by chart construction.
- Q: One plugin instance per request, or shared across requests? → A: Shared. Plugins are loaded at sidecar startup, one instance per plugin, reused for all validate calls. `validate(&self, context)` takes `&self`, so concurrent requests don't conflict. Plugins that need per-request mutable state must use interior mutability.
- Q: What happens if a plugin's `validate` panics? → A: Same as `process` — caught by the hub's `define_plugin!` panic guard, returned as a single error diagnostic with `line=0, column=0` and a message indicating internal validator failure. The sidecar continues serving subsequent requests.
- Q: What happens when no files in a request match any plugin's expected config layout? → A: The hub-level validator runs structural checks on the TOML (well-formed, known plugin names, `[hub]` block sane) and dispatches each `[plugins.params.<name>]` subtree to the corresponding plugin's `validate()`. A request that contains TOML that doesn't reference any loaded plugin produces an error diagnostic ("unknown plugin name 'xyz'") rather than silently being treated as valid.
- Q: Does the validator sidecar persist any state? → A: No. The sidecar is process-stateless. Plugins MAY hold internal caches (e.g., Coraza's compiled-rule LRU keyed by content hash) for performance, but those caches are in-process memory only and disappear on restart. HAPTIC's own LRU cache (path + content hash → result) is the durable layer.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - HAPTIC pre-validates a Coraza snippet on Ingress admission (Priority: P1)

A platform user adds a `nginx.ingress.kubernetes.io/modsecurity-snippet: "SecRulRemoveById 942100"` annotation to an Ingress (typo: `SecRul` instead of `SecRule`). HAPTIC's admission webhook receives the Ingress AdmissionReview, renders what the new hub config TOML would be, and dispatches the rendered file to the validator sidecar via its unix socket. The sidecar's coraza plugin's `validate()` runs `coraza.NewWAF(WithDirectives(...))` on the snippet, captures the parser's WARN log line `unknown directive "secrulremovebyid"` along with `currentLine`, returns a diagnostic `{path: "config.toml", line: 47, column: 0, message: "unknown directive \"secrulremovebyid\""}`. HAPTIC translates this into an `AdmissionReview` response with `Allowed: false` and a clear reject message including the file/line context. The user's `kubectl apply` fails with a precise error.

**Why this priority**: This is the immediate user-experience win that justifies the entire feature. Without it, broken modsecurity snippets silently degrade the route; with it, they fail loudly at admission time with line-level feedback.

**Independent Test**: Run the validator sidecar with the coraza plugin loaded; send a JSON request over the unix socket containing a TOML file with a malformed SecLang directive; assert the response carries `result: error`, exactly one entry in `errors`, with the line number of the bad directive and a message naming the unknown directive.

**Acceptance Scenarios**:

1. **Given** a TOML with a syntactically-valid Coraza snippet (a few `SecRule` lines), **When** dispatched to the sidecar, **Then** the response is `{result: "valid", warnings: [], errors: []}` and the round-trip completes in under 5 ms.
2. **Given** a TOML with a typo'd directive (`SecRulRemoveById`), **When** dispatched, **Then** the response is `{result: "error", warnings: [], errors: [{path, line, column: 0, message}]}` and the message identifies the unknown directive.
3. **Given** a TOML with a syntactically-valid Coraza snippet but a malformed regex (e.g., `@rx (foo`), **When** dispatched, **Then** the response is `{result: "error", ...}` with the line number where the rule containing the bad regex was defined and a message naming the regex compilation failure.
4. **Given** a TOML that references a plugin name not loaded into the validator sidecar (`[plugins.params.unknown_plugin]`), **When** dispatched, **Then** the response is `{result: "error", errors: [{path, line: <of header>, column: 0, message: "unknown plugin name 'unknown_plugin'"}]}`.
5. **Given** a malformed TOML (unclosed table header), **When** dispatched, **Then** the response is `{result: "error"}` with the line/column from the TOML parser and a message describing the parse failure. No plugin's `validate()` is called for that request because TOML parsing failed.

---

### User Story 2 - Production hub auto-validates plugin config at init (Priority: P2)

The same `validate()` method that powers the validator sidecar is called by the production hub before each plugin's `init()`. An operator who deploys the production hub directly (without HAPTIC) with a malformed plugin params block sees the hub fail to start with the same line/column-level diagnostic, instead of failing later inside `init()` with a less precise message. This makes the production hub's startup error reporting better for non-HAPTIC users without any extra work.

**Why this priority**: P2 because the production hub already has *some* error reporting via init failures. The improvement is sharper messages, not new capability. Ships in the same release as P1 since it falls out of the same plugin-trait change.

**Independent Test**: Boot the production hub with a TOML containing a malformed Coraza snippet; assert the hub fails to start, logs include the line number of the bad directive, and exits with non-zero status.

**Acceptance Scenarios**:

1. **Given** a hub config with valid plugin params for all loaded plugins, **When** the hub starts, **Then** validate succeeds for every plugin, init succeeds for every plugin, and the hub serves SPOE traffic as today.
2. **Given** a hub config with one plugin's params containing a malformed Coraza snippet, **When** the hub starts, **Then** validate fails for that plugin with line-level diagnostics, init is NOT called, the hub logs the diagnostics in structured form, and the hub exits with non-zero status. (Behaviour change from today: earlier failure at validate, not later at init.)
3. **Given** a hub config with one plugin's params validating successfully but its init failing (e.g., OTLP collector unreachable), **When** the hub starts, **Then** validate succeeds, init fails as today, the hub logs the init error, and exits. Validator-side success does not mask init-time failure of side-effecting setup.

---

### User Story 3 - Future plugin gains validation by overriding one method (Priority: P3)

A new plugin author (e.g., for sso-auth or fingerprinting) wants their plugin's per-Ingress config to surface line/column-precise errors at `kubectl apply` time. They override `validate()` on their plugin trait to do whatever deep checks make sense (parse OIDC discovery URL format, check fingerprint algorithm name validity, etc.) and return diagnostics. They do not need to touch the hub, the validator sidecar, the protocol, or HAPTIC.

**Why this priority**: P3 because we don't have a concrete plugin author waiting on this. It's a forward-compatibility statement: the design must allow a future plugin author to plug in without modifications elsewhere.

**Independent Test**: Add a stub plugin that overrides `validate()` to return one error for any input containing the string `"BAD"`. Load it in the validator sidecar. Send a TOML containing `[plugins.params.stub] x = "BAD"`. Assert the response contains the stub's error with the correct file/line/column.

---

### Edge Cases

- **TOML parse failure.** If the incoming TOML doesn't parse, no plugin's `validate()` is called. The response carries the TOML parser's line/column-level diagnostics directly. This is hub-level, not plugin-level.
- **Plugin missing from validator sidecar but referenced in TOML.** The TOML mentions `[plugins.params.coraza]` but the validator sidecar wasn't built with the coraza plugin loaded. Response includes `{line: <of header>, column: 0, message: "plugin 'coraza' is not loaded by this validator"}`. This MUST NOT be a panic — chart-level mistakes (validator sidecar plugin set drifted from production hub) should fail loudly via diagnostics, not crash.
- **Plugin loaded but produces no `validate()` override.** Plugin uses the default impl, which returns empty diagnostics. The hub's structural checks still run — this is the "shallow validation only" tier. Existing plugins (external-auth pre-bump) continue to work unchanged: schema check via `config_schema()` if they declare one, no deep check.
- **Plugin's `validate()` panics.** Caught by the hub's panic guard. Returned as a single error diagnostic `{line: 0, column: 0, message: "internal validator error: ..."}` so HAPTIC sees a fail-closed result rather than a hung socket. The sidecar logs the panic at ERROR with backtrace and continues serving subsequent requests.
- **Concurrent validate calls.** `validate(&self, ...)` takes `&self`. The sidecar's tokio server dispatches concurrent unix-socket connections to per-request tasks; each calls `validate` against shared plugin instances. Plugins must use interior mutability for any per-call state. Default impl is a constant function — trivially concurrent-safe.
- **Validator sidecar restart.** On restart, all in-process plugin caches reset (e.g., Coraza's compiled-rules LRU). HAPTIC's external LRU cache is the durable layer; validator restarts cause a flurry of cache-miss validations until HAPTIC's cache fills again. Acceptable transient cost.
- **Slow validate (rule compilation under load).** Validator caps each request at a configurable timeout (default 5 s); on overrun, return a single error diagnostic indicating timeout. The HAPTIC LRU prevents most repeats, so timeouts should be rare. Plugins MAY internally cache to avoid re-paying compile cost across calls.
- **Protocol version mismatch.** Each request and response carries `protocol_version: 1`. If the validator sidecar receives a request with a higher version than it understands, response is a single error diagnostic `{line: 0, column: 0, message: "validator protocol version N not supported (max: 1)"}` and the connection closes cleanly. Forward-compatibility is preserved.
- **Multiple files in one request.** The protocol supports a list of files. The hub-level validator processes each file independently; per-file diagnostics are returned with the originating `path`. This is the future hook for haproxy.cfg validation, gateway-config validation, etc. being routed to separate validators.

## Requirements *(mandatory)*

### Functional Requirements

#### Plugin API change (P1)

- **FR-001**: The `SpoePlugin` trait MUST gain a new method `validate(&self, context: &PluginContext) -> RVec<Diagnostic>` after the existing `config_schema()` method (so all `prefix_fields` boundaries remain ABI-stable). The default impl returns an empty `RVec`.
- **FR-002**: A new public type `Diagnostic` MUST be added to `haproxy_spoa_hub_plugin_api`, ABI-stable, with the fields:
  - `severity: DiagnosticSeverity` — enum with variants `Warning`, `Error`.
  - `path: RString` — the file the diagnostic refers to (e.g., `config.toml`). **Hub-owned**: plugins MUST leave this empty when constructing diagnostics; the hub overwrites it post-hoc with the file identity from its surrounding context (request file path in validator-sidecar mode, `--config` value in production mode). Plugins do not have access to file identity and don't need it — their input is the parsed `[plugins.params.<name>]` subtree, decoupled from the file it came from.
  - `line: u32` — 1-based line number, or `0` if unknown.
  - `column: u32` — 1-based column number, or `0` if unknown.
  - `message: RString` — human-readable explanation.
- **FR-003**: `validate()` implementations MUST be pure: no tokio tasks, no goroutines, no network I/O, no file I/O beyond reading inputs from the `PluginContext`, no global state mutation. Plugins MAY use process-internal caches, scoped to plugin-instance lifetime.
- **FR-004**: The hub's plugin startup orchestrator (production mode) MUST call `validate()` for **every** loaded plugin before calling `init()` for **any** plugin. Diagnostics from every plugin MUST be aggregated into a combined warnings/errors set. If any aggregated diagnostic has `severity = Error`, `init()` MUST NOT be called for any plugin and the hub MUST refuse to start with the full diagnostic set emitted as structured logs and the process exiting non-zero. Fail-fast on the first plugin's error is explicitly NOT acceptable — operators with multiple plugin-config bugs must see all of them in one startup attempt.
- **FR-005**: Diagnostics with `severity = Warning` MUST NOT block `init()`. They MUST be emitted as structured WARN logs.

#### Validator sidecar mode (P1)

- **FR-006**: The hub binary MUST accept a new flag `--validate-socket <path>`. When present, the hub starts in validator mode: loads plugins (via the existing `--config <toml>` flag), opens a unix domain socket at `<path>`, and serves requests instead of running the SPOE listener.
- **FR-007**: In validator mode, the hub MUST NOT call `init()` on any plugin. Plugins are constructed via their factory `new()` and held for the sidecar's lifetime; only `validate()`, `config_schema()`, and metadata methods (`name`, `version`) are invoked.
- **FR-008**: The unix socket MUST accept multiple concurrent connections; each connection runs in its own tokio task so slow validations on one don't block others. Connections are persistent: a client opens once and pipelines multiple sequential request frames over the same socket, with the server writing one response frame per request in arrival order. Either side closes at inter-frame boundaries (idle-timeout from the server, controller pool-eviction or shutdown from the client). Within a single connection, requests are sequential pipelined; concurrency comes from the controller's per-validator connection pool. The wire protocol contract is owned by HAPTIC (`docs/development/validator-protocol.md`); this implementation conforms.
- **FR-009**: The protocol MUST be length-prefixed JSON: each frame is a 4-byte big-endian length followed by that many bytes of JSON. Maximum frame size MUST be configurable, default 1 MiB.
- **FR-010**: Each request MUST contain `protocol_version: 1`. The validator sidecar MUST reject requests with unrecognised protocol versions via an error diagnostic.
- **FR-011**: A request MUST contain a `files` array. Each file is `{path: string, content: string}`. The hub-level validator parses each file as TOML; for each file: TOML parse → structural validation of `[hub]` and `[[plugins]]` → for each `[plugins.params.<name>]` subtree, dispatch to the matching plugin's `validate()` → aggregate diagnostics under that file's path.
- **FR-012**: The response MUST contain `protocol_version: 1`, `result: "valid" | "warning" | "error"`, `warnings: [...]`, `errors: [...]`. `result` is computed as: `error` if any diagnostic has `severity = Error`; else `warning` if any has `severity = Warning`; else `valid`. `warnings` and `errors` are populated independently per their severities. Both lists are always present (possibly empty).
- **FR-013**: The validator sidecar MUST cap each request's processing time at a configurable timeout (default 5 s). On timeout, a single error diagnostic is returned and the connection is closed (the byte-stream state after a mid-write timeout is no longer trustworthy; the controller's pool reopens transparently). Idle connections (no incoming request frames for `--validate-idle-timeout-ms`, default 60 s) MUST be closed cleanly so file descriptors don't accumulate; the controller's pool sees EOF on next acquire and replaces the dead connection.
- **FR-014**: Plugin panics during `validate()` MUST be caught by the hub's existing panic guard and translated to a single error diagnostic with `line: 0, column: 0` and a message indicating internal validator failure. The sidecar continues serving subsequent requests.

#### Coraza-specific line-number capture (P1, in plugin)

- **FR-015**: The coraza plugin's `validate()` implementation MUST construct a captured `debuglog.Logger` that records every structured log entry's `line` field, then call `coraza.NewWAF(WafConfig.WithDebugLogger(captured).WithDirectives(snippet))`. On error from `NewWAF`, the plugin MUST return one diagnostic per captured log entry that has both a `line` field and is marked at WARN/ERROR level, plus one final diagnostic carrying the wrapped error message at the line of the last captured entry. (Lower-level concern, but spec'd here so the integration is testable.)

### Non-Functional Requirements

- **NFR-001**: A `validate()` call for a typical per-Ingress modsecurity snippet (3–10 SecRule lines) MUST complete in under 1 ms on commodity hardware. Measured via existing benchmark harness; budget anchors the chart-level performance expectation.
- **NFR-002**: The validator sidecar MUST be resource-modest. Documented per-pod overhead under a typical chart deployment with otel + coraza + external-auth loaded should be ≤ 50 MB RSS at idle and ≤ 200 MB under concurrent load. This is non-binding (it depends on plugin choice) but is the design budget.
- **NFR-003**: The validator sidecar's failure mode (crash, OOM, slow) MUST NOT leave the haptic-controller pod permanently broken. Kubernetes `restartPolicy: Always` plus a livenessProbe on the unix socket's existence handles transient failures; HAPTIC's own webhook MUST fail-closed (reject admission) on validator unreachability with a clear message.

### Key Entities

- **`Diagnostic`** — value type carrying severity, file, position, message. ABI-stable across plugin-api versions; new severities (e.g., `Info`) require an api-version bump.
- **`ValidateRequest` / `ValidateResponse`** — the wire types for the unix-socket protocol. Versioned via `protocol_version`.
- **Validator-mode hub orchestrator** — boot-time component that loads plugins (no init), opens the socket, dispatches requests, aggregates diagnostics. Single tokio task accepting connections; per-request tasks for processing.

## Success Criteria *(mandatory)*

- **SC-001**: HAPTIC can pre-validate any rendered hub TOML against the validator sidecar before pushing to production. A round-trip including JSON encoding, socket write, validation, and decoding completes in under 5 ms for typical inputs (hub TOML with a handful of `[plugins.params.<name>]` subtrees).
- **SC-002**: Validator sidecar uptime mirrors production hub uptime. Restart-on-failure via Kubernetes deployment-level restart policy. No persistent state on disk; all warm caches are best-effort.
- **SC-003**: Each existing plugin (external-auth, sso-auth, otel, coraza) builds and runs against the new plugin-api version with zero behavioural change for plugins that don't override `validate()`. Coraza and otel ship with concrete `validate()` implementations in the same release window.
- **SC-004**: A protocol-version-2 request sent to a v1-only validator sidecar receives a clean error diagnostic and a closed connection — not a hang, not a panic.
- **SC-005**: A panic inside any plugin's `validate()` is caught and translated to a diagnostic; the sidecar continues serving subsequent requests without restart.

## Out of Scope

- HAPTIC chart wiring (sidecar containers, shared volume, CRD `validators:` field). Lands in a separate HAPTIC MR after this lands.
- **Protocol document.** The wire-protocol contract is owned by HAPTIC (`docs/development/validator-protocol.md`). The hub's `contracts/validate-socket-protocol.md` is now a one-liner pointer to that authoritative spec.
- HAPTIC-side LRU cache (path + content hash → result). Same.
- Plugin-side caches (e.g., Coraza compiled-rules LRU keyed by content hash). Optional optimisation; not required for this spec to be correct. Each plugin author decides.
- Hot-reload of validator sidecar plugin set without restart. Out of scope; restart-the-sidecar is the supported path.
- Request multiplexing on a single connection (i.e., interleaving response frames out of order, or correlation IDs on requests). Within a single connection, this implementation serves requests sequentially in pipelined order. Concurrency is achieved via the controller-side connection pool — each pool slot is a separate connection.
- Validators for non-hub-TOML files (haproxy.cfg, gateway config). The protocol is general enough to support them, but no concrete validator besides the hub's TOML validator ships in this MR.
- Authentication on the unix socket. Pod-boundary access control is the security model; the socket file's permissions (controlled by the chart) restrict access to the controller container.
- Persisting "last known good" config. Different concern (deploy-side, not validate-side); separate spec if/when it lands.
