#![allow(
    non_camel_case_types,
    non_local_definitions,
    clippy::unnecessary_wraps,
    clippy::wildcard_imports
)]

use haproxy_spoa_hub_plugin_api::*;

#[derive(Debug)]
struct NoopPlugin;

define_plugin!(NoopPlugin, {
    fn new() -> Self {
        NoopPlugin
    }

    fn init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn name(&self) -> &str {
        "noop"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        _message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessingResult {
            variables: vec![TxnVariable {
                scope: VarScope::Session,
                name: "result".into(),
                value: SpoeValue::String("ok".into()),
            }]
            .into(),
        })
    }
});
