# Feature Specification: Hub Control Args for Per-Request Plugin Skip

**Feature Branch**: `003-hub-control-args`
**Created**: 2026-04-03
**Status**: Draft
**Input**: User description: "Per-request conditional plugin execution via hub-prefixed control args in SPOE messages, allowing HAProxy to skip specific plugins using its native ACL engine"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Operator skips a plugin for allowlisted traffic (Priority: P1)

A hub operator runs a rate-limiting plugin alongside other plugins for the same SPOE message. Certain client IPs are on an allowlist and should not be rate-limited. The operator wants HAProxy to signal the hub to skip the rate-limiting plugin for those requests, without incurring any processing overhead for the skipped plugin.

The operator adds a `hub.skip_plugins` argument to the SPOE message definition and uses HAProxy ACLs to set it conditionally. The hub reads this argument, skips the named plugins, and strips all `hub.*` arguments from the message before passing it to the remaining plugins.

**Why this priority**: This is the primary use case driving the feature. Without it, the only option is an early return inside the plugin, which still incurs thread pool and FFI overhead per request.

**Independent Test**: Can be tested by sending a message with `hub.skip_plugins` naming a plugin, and verifying that plugin's output variables do not appear in the response while other plugins' variables do.

**Acceptance Scenarios**:

1. **Given** a message with `hub.skip_plugins = "rate-limiter"` and two registered plugins (rate-limiter and geoip), **When** the hub dispatches the message, **Then** only geoip executes and only geoip's output variables appear in the response.
2. **Given** a message without `hub.skip_plugins`, **When** the hub dispatches the message, **Then** all registered plugins execute as normal (backward compatible).
3. **Given** a message with `hub.skip_plugins = "rate-limiter,geoip"` (multiple plugins), **When** the hub dispatches the message, **Then** both named plugins are skipped.

---

### User Story 2 - Hub strips control args from plugin messages (Priority: P2)

A plugin author writes a plugin that iterates over all message arguments (e.g., an echo or logging plugin). The plugin should never see hub-internal control arguments like `hub.skip_plugins` in its message args, because these are infrastructure concerns, not application data.

The hub strips all arguments with the `hub.` prefix before passing the message to any plugin, keeping the plugin's view of the message clean.

**Why this priority**: Without stripping, plugins would need to filter out `hub.*` args themselves, violating separation of concerns and creating a maintenance burden for every plugin author.

**Independent Test**: Can be tested by sending a message with both `hub.*` args and regular args, then verifying the plugin only sees the regular args.

**Acceptance Scenarios**:

1. **Given** a message with args `src`, `method`, and `hub.skip_plugins`, **When** a plugin receives the message, **Then** its `message.args` contains `src` and `method` but not `hub.skip_plugins`.
2. **Given** a message with only `hub.*` args and no regular args, **When** a plugin receives the message, **Then** its `message.args` is empty.

---

### User Story 3 - Skipped plugin cascades to its dependents (Priority: P3)

A hub operator has a dependency chain: plugin B depends on plugin A. When plugin A is skipped via `hub.skip_plugins`, plugin B should also be automatically skipped because it cannot function without A's output. The operator should not need to list every transitive dependent in the skip list.

**Why this priority**: Without cascade, the operator must manually track and list all dependent plugins in the skip list, which is error-prone and creates tight coupling between the HAProxy config and the hub's plugin dependency graph.

**Independent Test**: Can be tested by skipping an upstream plugin and verifying that its downstream dependent is also skipped without being explicitly named.

**Acceptance Scenarios**:

1. **Given** plugin B depends on plugin A and a message with `hub.skip_plugins = "a"`, **When** the hub dispatches the message, **Then** both A and B are skipped.
2. **Given** a diamond dependency (A -> B, A -> C, B -> D, C -> D) and `hub.skip_plugins = "a"`, **When** the hub dispatches the message, **Then** all four plugins are skipped.
3. **Given** plugins A, B, C where B depends on A but C is independent, and `hub.skip_plugins = "a"`, **When** the hub dispatches the message, **Then** A and B are skipped but C executes normally.

---

### Edge Cases

- What happens when `hub.skip_plugins` names a plugin that doesn't exist? The unknown name is silently ignored; existing plugins execute normally.
- What happens when `hub.skip_plugins` names all plugins for the message? All plugins are skipped; the hub returns an empty ACK response (zero variables).
- What happens when `hub.skip_plugins` is not a string value (e.g., an integer)? The argument is consumed as a hub arg (stripped from the message) but the skip set is empty; all plugins execute.
- What happens when `hub.skip_plugins` contains extra whitespace or trailing commas? Whitespace is trimmed and empty entries are filtered out. `"a , b , "` is equivalent to `"a,b"`.
- What happens when a future `hub.*` arg is sent that the hub doesn't recognize? It is consumed (stripped from the message) but otherwise ignored. This is forward-compatible with future hub control args.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The hub MUST recognize message arguments with the `hub.` prefix as hub control args.
- **FR-002**: The hub MUST strip all `hub.`-prefixed arguments from the message before passing it to any plugin.
- **FR-003**: The hub MUST support a `hub.skip_plugins` control arg containing a comma-separated list of plugin names to skip for the current request.
- **FR-004**: When a plugin is listed in `hub.skip_plugins`, the hub MUST NOT execute that plugin (no thread pool work, no FFI call).
- **FR-005**: When a skipped plugin has downstream dependents (plugins that declared a dependency on it), the hub MUST automatically skip those dependents as well (transitive cascade).
- **FR-006**: When `hub.skip_plugins` is absent or empty, the hub MUST execute all registered plugins as before (full backward compatibility).
- **FR-007**: The hub MUST log each skipped plugin at debug level with the plugin name.
- **FR-008**: The hub MUST record a metric for each skipped plugin to enable operational monitoring.
- **FR-009**: Unrecognized `hub.*` args MUST be silently consumed (stripped but not acted upon), ensuring forward compatibility with future control args.

### Key Entities

- **Hub control arg**: A message argument with the `hub.` prefix, consumed by the hub and invisible to plugins. The prefix serves as a namespace separating hub infrastructure concerns from plugin application data.
- **Skip set**: The set of plugin names to skip for a given request, derived from `hub.skip_plugins` and expanded via transitive dependency cascade.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Skipped plugins incur zero processing overhead -- no thread pool scheduling, no FFI call, no timeout waiting.
- **SC-002**: Plugins never observe `hub.*` arguments in their message args, regardless of how many hub control args HAProxy sends.
- **SC-003**: An operator can skip a plugin for a subset of requests using only HAProxy ACL configuration, with no changes to hub config or plugin code.
- **SC-004**: Skipping an upstream plugin automatically cascades to all its transitive dependents without the operator listing each one.
- **SC-005**: Existing deployments with no `hub.*` args in their SPOE messages continue to work identically (zero behavioral change).

## Assumptions

- HAProxy's SPOE protocol allows dots in argument names. The SPOE spec explicitly permits `a-z`, `A-Z`, `0-9`, `.` and `_` in names.
- The `hub.` prefix is reserved exclusively for hub control args. Plugin authors should not use `hub.` as a prefix for their own argument names.
- The execution plan's topological group ordering is sufficient for cascade computation -- no additional dependency graph traversal is needed beyond walking groups in order.
- The hub's existing dependency graph (declared via `depends_on` in plugin config) is the source of truth for cascade decisions.
