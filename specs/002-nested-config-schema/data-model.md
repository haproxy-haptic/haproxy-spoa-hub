# Data Model: Nested Plugin Config with Schema Validation

## Entities

### ConfigValue

A recursive, FFI-safe tagged union representing a TOML value tree.

| Variant   | Payload                          | Maps from TOML type |
|-----------|----------------------------------|---------------------|
| String    | `RString`                        | String, Datetime    |
| Integer   | `i64`                            | Integer             |
| Float     | `f64`                            | Float               |
| Bool      | `bool`                           | Boolean             |
| Array     | `RVec<ConfigValue>`              | Array               |
| Table     | `RHashMap<RString, ConfigValue>` | Table               |

**Accessor methods** (all return `Option`):
- `as_str() -> Option<&str>`
- `as_integer() -> Option<i64>`
- `as_float() -> Option<f64>`
- `as_bool() -> Option<bool>`
- `as_array() -> Option<&RVec<ConfigValue>>`
- `as_table() -> Option<&RHashMap<RString, ConfigValue>>`

**Traits**: `Debug`, `Clone`, `StableAbi`, `PartialEq` (manual impl needed since `RHashMap` doesn't derive it).

**Location**: `crates/plugin-api/src/types.rs`

### PluginContext (modified)

| Field    | Old Type                       | New Type                              |
|----------|--------------------------------|---------------------------------------|
| `name`   | `RString`                      | `RString` (unchanged)                 |
| `config` | `RHashMap<RString, RString>`   | `RHashMap<RString, ConfigValue>`      |

**Location**: `crates/plugin-api/src/types.rs`

### PluginConfig (modified)

| Field    | Old Type                     | New Type      |
|----------|------------------------------|---------------|
| `params` | `HashMap<String, String>`    | `toml::Table` |

All other fields unchanged. `toml::Table` implements `Default` (empty map).

**Location**: `crates/hub/src/config.rs`

## Relationships

```text
TOML config file
  └── PluginConfig.params: toml::Table
        ├── [schema validation] → serde_json::Value → jsonschema::validate()
        └── [FFI conversion]   → RHashMap<RString, ConfigValue>
                                    └── PluginContext.config
                                          └── passed to plugin.init()
```

## State Transitions

Plugin loading with schema validation:

```text
new() → config_schema() → [validate config if schema present] → init() → ready
                              │                                     │
                              ├── schema malformed → LoadError       │
                              └── config invalid   → LoadError       │
                                                                     └── init error → LoadError
```
