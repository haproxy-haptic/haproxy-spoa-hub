# Contract: Plugin API Changes

## ConfigValue type (new)

Added to the public API of `haproxy-spoa-hub-plugin-api`.

```rust
#[repr(u8)]
#[derive(Debug, Clone, StableAbi)]
pub enum ConfigValue {
    String(RString),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Array(RVec<ConfigValue>),
    Table(RHashMap<RString, ConfigValue>),
}
```

### Accessor methods

```rust
impl ConfigValue {
    pub fn as_str(&self) -> Option<&str>;
    pub fn as_integer(&self) -> Option<i64>;
    pub fn as_float(&self) -> Option<f64>;
    pub fn as_bool(&self) -> Option<bool>;
    pub fn as_array(&self) -> Option<&RVec<ConfigValue>>;
    pub fn as_table(&self) -> Option<&RHashMap<RString, ConfigValue>>;
}
```

## PluginContext change (breaking)

```rust
pub struct PluginContext {
    pub name: RString,
    pub config: RHashMap<RString, ConfigValue>,  // was RHashMap<RString, RString>
}
```

### Migration for plugin authors

Before:
```rust
let db_path = context.config.get(&"db_path".into())
    .ok_or("missing db_path")?;
self.db = open_database(db_path.as_str())?;
```

After:
```rust
let db_path = context.config.get(&"db_path".into())
    .and_then(|v| v.as_str())
    .ok_or("missing db_path")?;
self.db = open_database(db_path)?;
```

## SpoePlugin trait addition (non-breaking)

Added after `#[sabi(last_prefix_field)]` on `shutdown()`:

```rust
fn config_schema(&self) -> ROption<RString> {
    ROption::RNone
}
```

Plugins compiled without this method use the default (no schema, no validation). This is a minor ABI addition.

### define_plugin! macro

Two arms supported:

1. Without schema (existing pattern, backward compatible):
```rust
define_plugin!(MyPlugin, {
    fn new() -> Self { ... }
    fn init(&mut self, context: &PluginContext) -> ... { ... }
    fn name(&self) -> &str { ... }
    fn version(&self) -> &str { ... }
    fn process(&self, message: &SpoeMessage) -> ... { ... }
});
```

2. With schema:
```rust
define_plugin!(MyPlugin, {
    fn new() -> Self { ... }
    fn init(&mut self, context: &PluginContext) -> ... { ... }
    fn name(&self) -> &str { ... }
    fn version(&self) -> &str { ... }
    fn process(&self, message: &SpoeMessage) -> ... { ... }
    fn config_schema(&self) -> Option<&str> { ... }
});
```

## TOML config format (unchanged surface, expanded types)

Before (only strings allowed):
```toml
[plugins.params]
db_path = "/data/GeoLite2.mmdb"
locale = "en"
```

After (all TOML types allowed):
```toml
[plugins.params]
db_path = "/data/GeoLite2.mmdb"
locale = "en"
max_rps = 1000
enabled = true
trusted_ips = ["10.0.0.0/8", "172.16.0.0/12"]

[plugins.params.rate_limits]
default = { max_rps = 100, burst = 50 }
premium = { max_rps = 1000, burst = 200 }
```

## Hub validation behavior

| Plugin has schema? | Config valid? | Result |
|--------------------|---------------|--------|
| No                 | N/A           | `init()` called, no validation |
| Yes                | Yes           | `init()` called |
| Yes                | No            | Plugin skipped, error logged with plugin name + field path + constraint |
| Yes (malformed)    | N/A           | Plugin skipped, schema parse error logged |
