# haproxy-spoa-hub-plugin-api

Plugin API for [haproxy-spoa-hub](https://github.com/haproxy-haptic/haproxy-spoa-hub) —
define HAProxy SPOE agent plugins as independently compiled shared libraries.

## Quick start

```rust
#![allow(non_camel_case_types, non_local_definitions)]
use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug, Default)]
struct MyPlugin {
    requests: std::sync::atomic::AtomicU64,
}

define_plugin!(MyPlugin, {
    fn new() -> Self { MyPlugin::default() }

    fn init(&mut self, _context: &PluginContext)
        -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        Ok(())
    }

    fn name(&self) -> &str { "my-plugin" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        self.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ProcessingResult { variables: vec![].into() })
    }
});
```

State belongs on the plugin instance. A hub reload constructs and initializes a
new instance before atomically swapping registries, so failed initialization
cannot mutate the generation that is still serving requests. Use interior
mutability for state changed by `process(&self, ...)`; avoid process-global
mutable statics, which break reload isolation and prevent old generations from
being reclaimed safely.

Your plugin crate must set `crate-type = ["cdylib", "rlib"]` in `Cargo.toml`.

For the full guide on writing plugins — including configuration, dependencies,
variable scoping, and JSON Schema validation — see
[Writing a Plugin](https://github.com/haproxy-haptic/haproxy-spoa-hub#writing-a-plugin)
in the main repository.

## License

Apache-2.0
