# Feature Specification: HAProxy SPOA Plugin Hub

**Feature Branch**: `001-spoa-plugin-hub`
**Created**: 2026-03-30
**Status**: Draft
**Input**: User description: "Implement a SPOA for HAProxy that acts as a hub for dynamically loadable plugins"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator Deploys Hub with Plugins (Priority: P1)

An operator installs the SPOA hub binary, places one or more plugin
shared libraries (`.so` files) in a configured directory, writes a
configuration file that maps SPOE message names to plugins, and starts
the hub. The hub loads all configured plugins at startup, validates
their ABI compatibility, and begins listening for HAProxy connections.
When HAProxy sends SPOE messages, the hub dispatches them to the
correct plugins and returns the combined results.

**Why this priority**: Without the core hub-to-plugin dispatch loop,
nothing else works. This is the foundational value proposition.

**Independent Test**: Start the hub with a minimal no-op test plugin
that receives a message and sets a single transaction variable. Verify
via HAProxy logs that the variable is set correctly on each request.

**Acceptance Scenarios**:

1. **Given** a hub binary and a no-op plugin `.so` in the plugin
   directory, **When** the operator starts the hub with a valid config,
   **Then** the hub loads the plugin, logs its name and version, and
   begins accepting connections.
2. **Given** a running hub with one plugin, **When** HAProxy sends an
   SPOE NOTIFY frame with a matching message name, **Then** the hub
   invokes the plugin and returns an ACK frame containing the
   transaction variables set by the plugin.
3. **Given** a plugin `.so` built against an incompatible ABI version,
   **When** the hub attempts to load it, **Then** the hub rejects the
   plugin with a clear error message naming the expected and found ABI
   versions, and continues loading remaining plugins.

---

### User Story 2 - Parallel Plugin Execution (Priority: P1)

An operator configures multiple independent plugins (e.g., TLS
fingerprinting and GeoIP lookup) for the same SPOE message. The hub
executes these plugins concurrently to minimize total latency. Each
plugin independently sets its own transaction variables. If one plugin
fails, the others still complete and their results are returned.

**Why this priority**: Parallel execution is a core architectural
requirement — sequential execution of independent plugins would make
the hub a latency bottleneck that defeats the purpose of the design.

**Independent Test**: Configure two test plugins that each sleep for
5 ms and set distinct variables. Verify that the total processing time
is close to 5 ms (parallel) rather than 10 ms (sequential) and that
both variables appear in the ACK response.

**Acceptance Scenarios**:

1. **Given** two independent plugins registered for the same message,
   **When** a NOTIFY frame arrives, **Then** both plugins execute
   concurrently and the response contains variables from both.
2. **Given** two plugins where one fails with an error, **When** a
   NOTIFY frame arrives, **Then** the successful plugin's variables are
   returned and the failure is logged with the plugin name and error
   detail.
3. **Given** three plugins where two are independent and one depends on
   results from the first, **When** a NOTIFY frame arrives, **Then**
   the two independent plugins run in parallel, the dependent plugin
   runs after its dependency completes, and all results are returned.
4. **Given** a plugin whose dependency errors, times out, is overloaded, or is
   skipped, **When** a NOTIFY frame arrives, **Then** the dependent does not
   execute and returns the host-owned status `skipped`.

---

### User Story 3 - Unix Domain Socket and TCP Listening (Priority: P1)

The hub listens for HAProxy connections on a Unix domain socket
(primary, lowest-latency transport) and optionally on a TCP address.
Both transports speak the same SPOP (SPOE Protocol) wire protocol.
The operator configures one or both listeners.

**Why this priority**: Transport support is required for the hub to
communicate with HAProxy at all. Unix socket is the performance-
optimized path; TCP is required for SPOP compliance and remote
deployment scenarios.

**Independent Test**: Start the hub with both a Unix socket and a TCP
listener. Connect HAProxy to each transport independently and verify
that SPOE messages are processed identically on both.

**Acceptance Scenarios**:

1. **Given** a hub configured with a Unix socket path, **When**
   HAProxy connects via that socket, **Then** the SPOP handshake
   completes and messages are processed.
2. **Given** a hub configured with a TCP bind address, **When**
   HAProxy connects via TCP, **Then** the SPOP handshake completes
   and messages are processed identically to the Unix socket path.
3. **Given** a hub configured with both transports, **When** HAProxy
   instances connect on both simultaneously, **Then** both connections
   are served concurrently without interference.

---

### User Story 4 - Plugin Developer Creates a New Plugin (Priority: P2)

A developer who has no access to the hub source code creates a new
plugin as a standalone Rust crate. They depend on a published
plugin API crate that defines the plugin interface, implement the
required functions, compile to a `.so`, and distribute the plugin
independently. The hub operator drops the `.so` into the plugin
directory and adds a config entry.

**Why this priority**: The decoupled plugin ecosystem is the key
differentiator of this project. Without a clean, documented, stable
plugin API, third-party contributions require coupling to the hub
repository.

**Independent Test**: A developer creates a minimal plugin crate from
scratch using only the published API crate, compiles it, and the hub
loads and executes it without any changes to the hub itself.

**Acceptance Scenarios**:

1. **Given** a developer with only the plugin API crate documentation,
   **When** they create a new crate, implement the plugin interface,
   and compile with `cargo build --release`, **Then** the output is a
   `.so` file loadable by the hub.
2. **Given** a compiled third-party plugin `.so`, **When** the operator
   adds it to the plugin directory and updates the config, **Then** the
   hub loads it at next startup without recompilation of the hub.
3. **Given** a plugin built against plugin API version 1.x, **When**
   the hub is upgraded to a newer 1.y version, **Then** the plugin
   continues to work without recompilation (minor version backward
   compatibility).

---

### User Story 5 - Hub Provides Pre-Parsed Data to Plugins (Priority: P2)

The hub performs generic SPOE message parsing once and provides plugins
with structured, pre-parsed data (typed message arguments, metadata)
rather than raw SPOP frame bytes. This avoids duplicate parsing work
in every plugin and ensures consistent data interpretation.

**Why this priority**: Duplicate parsing across plugins wastes CPU
cycles and introduces risk of inconsistent interpretations. The hub's
role as a shared processing layer depends on this.

**Independent Test**: Send an SPOE message with multiple typed
arguments (integer, string, binary). Verify via a test plugin that
all arguments arrive as correctly typed, pre-parsed values matching
the original SPOE message content.

**Acceptance Scenarios**:

1. **Given** an SPOE message with integer, string, IPv4, IPv6, and
   binary arguments, **When** the hub dispatches it to a plugin,
   **Then** the plugin receives each argument with its correct type
   and value without performing any wire-format parsing.
2. **Given** an SPOE message, **When** the hub dispatches it, **Then**
   plugins also receive connection metadata (stream ID and frame ID)
   as structured fields in SpoeMessage.

---

### User Story 6 - Graceful Shutdown and Reload (Priority: P3)

The operator sends a signal (SIGTERM/SIGINT) to the hub to initiate
graceful shutdown. In-flight SPOE messages complete processing before
the hub exits. A separate signal (SIGHUP) triggers configuration
reload: newly added plugins are loaded and removed plugins are
unloaded without dropping active connections.

**Why this priority**: Production deployments require zero-downtime
maintenance. Without graceful lifecycle management, plugin updates
require hard restarts that drop in-flight requests.

**Independent Test**: Send a SIGHUP while requests are in flight.
Verify that active requests complete, newly configured plugins are
available for subsequent requests, and removed plugins stop receiving
new work.

**Acceptance Scenarios**:

1. **Given** a running hub processing requests, **When** it receives
   SIGTERM, **Then** it stops accepting new connections, finishes all
   in-flight messages, and exits cleanly.
2. **Given** a running hub, **When** it receives SIGHUP with updated
   config that adds a new plugin, **Then** the new plugin is loaded
   and available for subsequent messages without interrupting active
   connections.
3. **Given** a running hub, **When** it receives SIGHUP with a config
   that removes a plugin, **Then** the removed plugin finishes any
   in-flight work, is unloaded, and subsequent messages no longer
   dispatch to it.

---

### Edge Cases

- What happens when a plugin panics or segfaults during message
  processing? The hub MUST isolate the failure, log it, and continue
  serving other plugins and connections.
- What happens when the plugin directory contains a `.so` file that is
  not a valid plugin (e.g., a random shared library)? The hub MUST
  reject it with a descriptive error and continue loading other plugins.
- What happens when two plugins set variables? The hub MUST
  auto-prefix all transaction variables with the plugin name (e.g.,
  `geoip.country_code`, `ja3.hash`), making name conflicts
  structurally impossible. Plugins set variables using unprefixed
  names; the hub adds the prefix transparently before building the
  ACK response.
- What happens when HAProxy disconnects mid-message? The hub MUST
  cancel in-flight plugin work for that connection and free resources.
- What happens when plugin execution exceeds a configured timeout? The
  hub MUST return an explicit timeout status and results from plugins
  that completed within their deadline. Because blocking native plugin
  calls cannot be cancelled safely, the call retains its bounded
  concurrency slot until it actually returns. Excess calls are rejected
  with an explicit overload status instead of accumulating unbounded work.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The hub MUST implement the SPOP (SPOE Protocol) as
  defined by HAProxy, including HAPROXY-HELLO/AGENT-HELLO handshake,
  NOTIFY/ACK message exchange, and HAPROXY-DISCONNECT handling.
- **FR-002**: The hub MUST load plugin shared libraries (`.so` files)
  at startup from a configured directory path.
- **FR-003**: The hub MUST validate each plugin's ABI version at load
  time and reject incompatible plugins with a descriptive error.
- **FR-004**: The hub MUST dispatch SPOE messages to plugins based on
  a configurable mapping of SPOE message names to plugin names.
- **FR-005**: The hub MUST execute independent plugins concurrently
  for the same message to minimize total processing latency.
- **FR-006**: The hub MUST support plugin dependency declarations so
  that dependent plugins execute after their dependencies complete.
  The hub MUST pass dependency output as additional named arguments
  (using the namespace-prefixed variable names) merged into the
  original SPOE message data the dependent plugin receives.
- **FR-007**: The hub MUST aggregate transaction variables from all
  plugins into a single ACK response per SPOE message. Variables MUST
  be auto-prefixed with the plugin name (e.g., `<plugin>.<varname>`)
  to prevent naming conflicts between plugins.
- **FR-008**: The hub MUST listen on Unix domain sockets for local
  HAProxy connections.
- **FR-009**: The hub MUST listen on TCP sockets for remote HAProxy
  connections.
- **FR-010**: The hub MUST parse SPOE message arguments into typed
  values (integer, string, binary, IPv4, IPv6, boolean) and provide
  them to plugins in a structured format.
- **FR-011**: The hub MUST provide a stable, versioned plugin API crate
  that plugin developers use to build plugins independently.
- **FR-012**: The hub MUST enforce per-plugin execution timeouts,
  configurable per plugin. When a plugin exceeds its timeout, the hub
  MUST return partial results from all plugins that completed in time
  rather than failing the entire message. The hub MUST enforce bounded
  per-plugin concurrency and queue capacity, retain capacity for timed-out
  blocking calls until they return, and emit a host-owned status for success,
  error, timeout, and overload.
- **FR-013**: The hub MUST catch plugin panics at the FFI boundary and
  convert them to logged errors without crashing the hub process.
- **FR-014**: The hub MUST support graceful shutdown on SIGTERM/SIGINT,
  completing in-flight messages before exiting.
- **FR-015**: The hub MUST read its configuration from a TOML file
  (specifying listeners, plugin directory, plugin-to-message mappings,
  timeouts, and logging settings).
- **FR-016**: The hub MUST log plugin load/unload events, message
  dispatch events, and errors as structured JSON to stderr.
- **FR-017**: The hub MUST expose runtime performance metrics
  (connection counts, per-plugin latency, message throughput, timeout
  counts) via a Prometheus-compatible HTTP endpoint on a configurable
  port.

### Key Entities

- **Hub**: The main process that implements SPOP, manages plugin
  lifecycle, and dispatches messages. Owns listener sockets, plugin
  registry, and configuration.
- **Plugin**: A shared library (`.so`) that implements the plugin API.
  Has a name, version, ABI version, and a set of message handlers.
  Maintains its own opaque state.
- **Message**: An SPOE message received from HAProxy via a NOTIFY
  frame. Contains a name, typed arguments, and connection metadata.
- **Action/Variable**: A key-value pair (transaction variable) that a
  plugin sets in response to a message. Aggregated by the hub into the
  ACK response sent back to HAProxy.
- **Plugin Configuration**: A mapping entry that binds an SPOE message
  name to one or more plugins, with optional dependency ordering and
  per-plugin timeout overrides.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A no-op plugin (receives message, sets one variable)
  adds less than 100 microseconds of overhead compared to a baseline
  SPOA that does the same work without the plugin abstraction.
- **SC-002**: Two independent plugins running in parallel complete
  within 110% of the slower plugin's individual execution time (i.e.,
  parallelization overhead is less than 10%).
- **SC-003**: A new plugin can be created from scratch by a developer
  unfamiliar with the hub codebase in under 30 minutes, using only the
  published plugin API crate and its documentation.
- **SC-004**: The hub handles at least 10,000 concurrent SPOE
  connections without degradation in per-message processing latency.
- **SC-005**: A plugin crash (panic) does not terminate the hub
  process; the hub continues serving other plugins and connections.
- **SC-006**: Plugin loading rejects ABI-incompatible plugins 100% of
  the time with an actionable error message.
- **SC-007**: Configuration errors are detected and reported at startup
  with line-level detail, not at first request time.

## Clarifications

### Session 2026-03-30

- Q: How should the hub handle transaction variable name conflicts between plugins? → A: Namespace-prefixed — variables are auto-prefixed with the plugin name (e.g., `geoip.country_code`), making conflicts structurally impossible.
- Q: How should the hub handle overload when plugins can't keep up? → A: Bound per-plugin concurrency and queue capacity, return explicit overload or timeout status, and retain capacity until non-cancellable blocking calls actually return.
- Q: What configuration file format should the hub use? → A: TOML — Rust ecosystem standard, clean syntax, strong serde support.
- Q: What observability output format and destination should the hub use? → A: Structured JSON to stderr for logs, plus a built-in Prometheus metrics endpoint on a configurable port.
- Q: How does a dependent plugin receive output from its dependencies? → A: Dependency output merged into input — upstream plugin's namespace-prefixed variables are added as additional named arguments alongside the original SPOE message data.

## Assumptions

- HAProxy is the only consumer of the SPOP interface; no other SPOP
  clients need to be supported.
- Plugins are trusted code (installed by the operator), so process-
  level sandboxing of plugins is not required. Panic isolation at the
  FFI boundary is sufficient.
- The hub and plugins run on the same machine and are compiled for the
  same target architecture (e.g., x86_64-unknown-linux-gnu).
- Plugin developers have access to a Rust toolchain and can compile
  `cdylib` crates. Support for plugins in other languages is out of
  scope for the initial version.
- The `spop` Rust crate (or equivalent) provides a usable
  implementation of the SPOE wire protocol codec.
- The plugin API crate will be published to crates.io for independent
  consumption by plugin developers.
- Hot-reload of plugins (SIGHUP) is a P3 feature. The initial
  deployment may launch without it if P1/P2 stories are prioritized.
