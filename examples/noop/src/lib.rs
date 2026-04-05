#![allow(non_camel_case_types, non_local_definitions)]

use haproxy_spoa_hub_plugin_api::{
    ProcessingResult, SpoeValue, TxnVariable, VarScope, define_plugin,
};

#[derive(Debug)]
struct NoopPlugin;

define_plugin!(NoopPlugin, {
    fn new() -> Self {
        NoopPlugin
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
