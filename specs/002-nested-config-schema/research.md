# Research: Nested Plugin Config with Schema Validation

## R1: Recursive StableAbi enum feasibility

**Decision**: Define `ConfigValue` as `#[repr(u8)] #[derive(StableAbi)]` enum with `RVec<ConfigValue>` and `RHashMap<RString, ConfigValue>` variants.

**Rationale**: `RVec<T>` and `RHashMap<K, V>` are heap-allocated containers with fixed inline layout (pointer + length + capacity). The `StableAbi` derive macro resolves type layouts lazily via `TypeLayoutCtor` function pointers, which breaks the recursive cycle. This is the same mechanism that allows `RVec<u8>` inside `SpoeValue::Binary` today.

**Alternatives considered**:
- Serialize config to JSON string across FFI: Loses type safety, adds serde_json dependency to every plugin, worse DX.
- Use `#[sabi(unsafe_opaque_fields)]` to wrap `toml::Value`: Fragile across toml crate version changes, defeats ABI checking.
- Flatten nested TOML into dot-notation keys (`server.host`): Keeps `RHashMap<RString, RString>` but requires parsing conventions and loses type information.

## R2: TOML deserialization target for nested params

**Decision**: Use `toml::Table` (alias for `toml::map::Map<String, toml::Value>`) as the deserialization type for `PluginConfig.params`.

**Rationale**: `toml::Table` directly maps to the `[plugins.params]` TOML section with full type fidelity. It implements `Default` (empty map), so `#[serde(default)]` continues to work. All existing test sites using `params: Default::default()` compile without changes.

**Alternatives considered**:
- `toml::Value`: Would allow params to be any TOML type (not just a table), but params should always be a key-value mapping at the top level.
- `HashMap<String, toml::Value>`: Similar to `toml::Table` but loses TOML-specific ordering. Functionally equivalent; `toml::Table` is more idiomatic.

## R3: JSON Schema validation crate

**Decision**: Use `jsonschema` 0.45 with `default-features = false`.

**Rationale**: `jsonschema` is the most mature Rust JSON Schema validation library. Default features include HTTP `$ref` resolution via reqwest + TLS, which are unnecessary for validating inline schemas. Disabling defaults avoids pulling in reqwest, rustls, and their transitive dependency trees.

**Alternatives considered**:
- `valico`: Less maintained, fewer JSON Schema draft features.
- Custom validation: Significant effort to implement even basic JSON Schema; not justified when a mature crate exists.
- `boon`: Newer, smaller, but less battle-tested. `jsonschema` has wider adoption.

## R4: TOML-to-JSON value mapping for schema validation

**Decision**: Convert `toml::Table` to `serde_json::Value` for JSON Schema validation, then separately convert `toml::Table` to `ConfigValue` for FFI.

**Rationale**: The `jsonschema` crate operates on `serde_json::Value`. TOML and JSON share the same scalar types (string, integer, float, boolean) and structural types (array, object/table). The mapping is straightforward:
- `toml::Value::String` → `json!(string)`
- `toml::Value::Integer(i64)` → `json!(i64)`
- `toml::Value::Float(f64)` → `json!(f64)`
- `toml::Value::Boolean` → `json!(bool)`
- `toml::Value::Array` → `json!([...])`
- `toml::Value::Table` → `json!({...})`
- `toml::Value::Datetime` → `json!(dt.to_string())` (stringified, since JSON has no datetime)

Both conversions (TOML→JSON for validation, TOML→ConfigValue for FFI) are done in `plugin_loader.rs`. The `toml` crate provides `toml::Value` → `serde_json::Value` conversion via serde serialization.

**Alternatives considered**:
- Convert `ConfigValue` to `serde_json::Value` instead: Requires building ConfigValue first, then converting again. Two-step is more work and ConfigValue doesn't implement Serialize.
- Use `toml::to_string` then `serde_json::from_str`: Lossy roundtrip for datetimes and ordering. Direct serde conversion is cleaner.

## R5: Plugin trait extensibility for config_schema()

**Decision**: Add `config_schema(&self) -> ROption<RString>` after the `#[sabi(last_prefix_field)]` marker on `shutdown()` in the `SpoePlugin` trait.

**Rationale**: Per abi_stable's sabi_trait rules, methods after `last_prefix_field` are optional with defaults. Old plugins compiled without `config_schema` will use the default (`RNone` = no schema = no validation). This is a minor version addition, not a breaking change to the trait ABI.

The method returns an `ROption<RString>` containing a JSON Schema string. Returning a string (not structured JSON) keeps the plugin-api crate free of serde_json dependency — plugin authors who want schema validation add serde_json to their own crate and use `json!()` for ergonomic schema construction.

**Alternatives considered**:
- Add schema to `PluginMod` (module-level, not instance-level): Requires prefix type changes to the root module, which is a more invasive ABI change.
- Return structured `serde_json::Value` across FFI: Not `StableAbi`-compatible without opaque wrappers.
- Custom StableAbi schema DSL: More types to maintain, limited validation expressiveness, more boilerplate for plugin authors. Discussed with user — JSON Schema chosen.
