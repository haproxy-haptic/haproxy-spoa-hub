# Feature Specification: Nested Plugin Config with Schema Validation

**Feature Branch**: `002-nested-config-schema`
**Created**: 2026-04-03
**Status**: Draft
**Input**: User description: "Support arbitrary nested data structures in plugin parameters and allow plugins to declare a JSON Schema for config validation before initialization"

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Plugin author uses nested configuration (Priority: P1)

A plugin author building a rate-limiting plugin needs to configure multiple rate limit tiers as a list of objects, each with fields like endpoint pattern, max requests per second, and burst size. Currently, plugin parameters only support flat string key-value pairs, forcing authors to invent ad-hoc encoding schemes (e.g., comma-separated values) or load separate config files.

With nested config support, the plugin author writes TOML config using native tables and arrays, and the hub passes the full structure to the plugin's `init()` method as a typed tree of values.

**Why this priority**: This is the core enabler. Without nested config support, complex plugins like rate-limiting require workarounds that are fragile and hard to maintain.

**Independent Test**: Can be fully tested by writing a TOML config with nested params (tables, arrays, integers, booleans) and verifying the plugin receives correctly typed values during `init()`.

**Acceptance Scenarios**:

1. **Given** a TOML config with `[plugins.params]` containing nested tables, **When** the hub loads the plugin, **Then** the plugin receives a structured config tree preserving types and nesting.
2. **Given** a TOML config with `[plugins.params]` containing arrays of objects, **When** the hub loads the plugin, **Then** the plugin can iterate the array and access each object's fields with their native types (string, integer, float, boolean).
3. **Given** a TOML config with `[plugins.params]` containing only flat string key-value pairs (existing format), **When** the hub loads the plugin, **Then** the plugin receives them as string-typed values (backward-compatible behavior).

---

### User Story 2 - Plugin author declares a config schema (Priority: P2)

A plugin author wants to prevent misconfiguration by declaring the expected shape and types of their plugin's parameters. The plugin provides a JSON Schema definition, and the hub validates the user-supplied config against it before calling `init()`. If validation fails, the hub reports clear errors identifying which fields are missing, wrong-typed, or otherwise invalid.

**Why this priority**: Schema validation catches configuration mistakes early with clear error messages, preventing confusing runtime failures inside `init()`. It builds on the nested config support from P1.

**Independent Test**: Can be tested by providing a plugin with a schema, then supplying both valid and invalid configs, and verifying the hub accepts or rejects them with appropriate error messages before `init()` is called.

**Acceptance Scenarios**:

1. **Given** a plugin that declares a JSON Schema requiring a string field `db_path`, **When** the config includes `db_path = "/data/test.mmdb"`, **Then** validation passes and `init()` is called.
2. **Given** a plugin that declares a JSON Schema requiring a string field `db_path`, **When** the config omits `db_path`, **Then** the hub rejects the config with an error message naming the missing field, and `init()` is never called.
3. **Given** a plugin that declares a JSON Schema expecting `max_rps` as an integer, **When** the config provides `max_rps = "not-a-number"`, **Then** the hub rejects the config with a type mismatch error.
4. **Given** a plugin that does not declare any schema, **When** the hub loads it, **Then** no validation is performed and `init()` is called normally.

---

### User Story 3 - Hub operator troubleshoots config errors (Priority: P3)

A hub operator deploying a new plugin writes the TOML config and starts the hub. If the config doesn't match the plugin's schema, the hub logs a clear error message at startup that identifies the plugin name, the invalid field path, and what was expected vs. what was provided.

**Why this priority**: Good error messages reduce operational burden. This is a refinement of P2 focused on the operator experience.

**Independent Test**: Can be tested by providing an invalid config and checking that the hub's error output includes the plugin name, field path, and human-readable description of the violation.

**Acceptance Scenarios**:

1. **Given** a plugin with a schema and an invalid config, **When** the hub starts, **Then** the error log includes the plugin name, the JSON path of the failing field, and a description of the constraint violated.
2. **Given** multiple plugins where one has an invalid config, **When** the hub starts, **Then** the invalid plugin is skipped with an error, and other valid plugins load normally.

---

### Edge Cases

- What happens when a TOML datetime value appears in params? It is converted to a string representation.
- What happens when a plugin's JSON Schema itself is malformed? The hub rejects the plugin with a schema parse error and skips it.
- What happens when params is omitted entirely from a plugin config? It defaults to an empty table (existing behavior preserved).
- What happens when a schema defines constraints not expressible in TOML (e.g., string format "email")? Validation still works -- TOML string values are validated against JSON Schema string constraints.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The plugin parameter system MUST support all native TOML value types: strings, integers, floats, booleans, arrays, and tables (including nesting).
- **FR-002**: The plugin API MUST expose a typed config value representation that preserves the original TOML types across the FFI boundary.
- **FR-003**: The typed config value MUST provide accessor methods for each supported type (string, integer, float, boolean, array, table) that return `None` for type mismatches.
- **FR-004**: Plugins MUST be able to optionally declare a JSON Schema describing their expected configuration structure.
- **FR-005**: The hub MUST validate plugin config against the declared schema before calling the plugin's `init()` method.
- **FR-006**: When schema validation fails, the hub MUST skip the plugin, log the plugin name and validation errors, and continue loading other plugins.
- **FR-007**: When a plugin declares no schema, the hub MUST pass config to `init()` without validation (opt-in behavior).
- **FR-008**: TOML datetime values MUST be converted to string representations when passed to plugins.
- **FR-009**: Existing flat string-only configs MUST continue to work without changes to the TOML files (values are now typed as strings rather than being the only option).

### Key Entities

- **ConfigValue**: A recursive typed value representing a TOML value tree. Variants: String, Integer, Float, Bool, Array (of ConfigValues), Table (map of string keys to ConfigValues).
- **PluginContext**: The initialization context passed to plugins, containing the plugin name and a table of ConfigValue parameters.
- **Config Schema**: A JSON Schema document declared by a plugin, used by the hub to validate the plugin's params section before initialization.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Plugin authors can configure plugins using arbitrarily nested TOML structures without resorting to string encoding workarounds.
- **SC-002**: All six TOML value types (string, integer, float, boolean, array, table) are preserved with their native types through to the plugin.
- **SC-003**: A plugin with a declared schema that receives invalid config is rejected before `init()` runs, with an error message identifying the specific validation failure.
- **SC-004**: Plugins without a schema continue to work identically to current behavior.
- **SC-005**: Existing TOML configs with flat string params load without modification.

## Assumptions

- Plugins are recompiled against the updated plugin API. This is a breaking change to the FFI types, acceptable because the crate has not been published yet.
- JSON Schema is a reasonable standard for plugin authors to learn. It is widely known, well-documented, and supported by tooling in most languages.
- The hub is responsible for schema validation, not the plugin. Plugins receive only validated config.
- TOML is the only config format. There is no need to support JSON or YAML config files.
