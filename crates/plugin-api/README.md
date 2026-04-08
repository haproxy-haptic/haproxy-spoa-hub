# haproxy-spoa-hub-plugin-api

Plugin API for [haproxy-spoa-hub](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub) —
define HAProxy SPOE agent plugins as independently compiled shared libraries.

## Quick start

```rust
#![allow(non_camel_case_types, non_local_definitions)]
use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct MyPlugin;

define_plugin!(MyPlugin, {
    fn new() -> Self { MyPlugin }

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
        Ok(ProcessingResult { variables: vec![].into() })
    }
});
```

Your plugin crate must set `crate-type = ["cdylib", "rlib"]` in `Cargo.toml`.

For the full guide on writing plugins — including configuration, dependencies,
variable scoping, and JSON Schema validation — see
[Writing a Plugin](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub#writing-a-plugin)
in the main repository.

## License

Apache-2.0
