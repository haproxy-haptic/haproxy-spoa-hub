#![allow(
    non_camel_case_types,
    non_local_definitions,
    clippy::unnecessary_wraps,
    clippy::wildcard_imports
)]

use haproxy_spoa_hub_plugin_api::*;

/// A test plugin that sleeps for a configurable duration (default 5ms)
/// and sets a `done` variable. Used for parallel execution timing tests.
///
/// The sleep duration can be configured via the `delay_ms` message
/// argument (as Int32 or Uint32). If not provided, defaults to 5ms.
#[derive(Debug)]
struct DelayPlugin;

define_plugin!(DelayPlugin, {
    fn new() -> Self {
        DelayPlugin
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn name(&self) -> &str {
        "delay"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        let delay_ms = message
            .args
            .get(&RString::from("delay_ms"))
            .and_then(|v| match v {
                SpoeValue::Int32(ms) => u64::try_from(*ms).ok(),
                SpoeValue::Uint32(ms) => Some(u64::from(*ms)),
                _ => None,
            })
            .unwrap_or(5);

        std::thread::sleep(std::time::Duration::from_millis(delay_ms));

        Ok(ProcessingResult {
            variables: vec![TxnVariable {
                scope: VarScope::Session,
                name: "done".into(),
                value: SpoeValue::Bool(true),
            }]
            .into(),
        })
    }
});
