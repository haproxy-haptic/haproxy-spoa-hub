# Quickstart: Nested Plugin Config with Schema Validation

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
```

## Using nested config in a plugin

TOML config:
```toml
[[plugins]]
name = "rate-limiter"
library = "librate_limiter_plugin.so"
messages = ["check-request"]

[plugins.params]
default_max_rps = 100
trusted_ips = ["10.0.0.0/8", "172.16.0.0/12"]

[plugins.params.tiers.free]
max_rps = 50
burst = 25

[plugins.params.tiers.premium]
max_rps = 1000
burst = 200
```

Accessing in plugin `init()`:
```rust
fn init(&mut self, context: &PluginContext)
    -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
    // Scalar values
    let max_rps = context.config.get(&"default_max_rps".into())
        .and_then(|v| v.as_integer())
        .unwrap_or(100);

    // Array values
    if let Some(ips) = context.config.get(&"trusted_ips".into())
        .and_then(|v| v.as_array())
    {
        for ip in ips.iter() {
            if let Some(cidr) = ip.as_str() {
                self.trusted_ips.push(cidr.parse()?);
            }
        }
    }

    // Nested table
    if let Some(tiers) = context.config.get(&"tiers".into())
        .and_then(|v| v.as_table())
    {
        for pair in tiers.iter() {
            let (name, tier_config) = (pair.0, pair.1);
            // ... parse each tier
        }
    }

    Ok(())
}
```

## Adding schema validation to a plugin

```rust
define_plugin!(RateLimiter, {
    fn new() -> Self { RateLimiter::default() }
    fn init(&mut self, context: &PluginContext) -> ... { /* ... */ }
    fn name(&self) -> &str { "rate-limiter" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn process(&self, message: &SpoeMessage) -> ... { /* ... */ }
    fn config_schema(&self) -> Option<&str> {
        Some(r#"{
            "type": "object",
            "required": ["default_max_rps"],
            "properties": {
                "default_max_rps": { "type": "integer", "minimum": 1 },
                "trusted_ips": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "tiers": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "required": ["max_rps", "burst"],
                        "properties": {
                            "max_rps": { "type": "integer", "minimum": 1 },
                            "burst": { "type": "integer", "minimum": 0 }
                        }
                    }
                }
            }
        }"#)
    }
});
```

If config is invalid, the hub logs an error like:
```
ERROR plugin="rate-limiter" error="config validation failed: \"default_max_rps\" is a required property"
```
