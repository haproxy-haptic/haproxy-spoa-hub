#![allow(non_camel_case_types, non_local_definitions)]

use haproxy_spoa_hub_plugin_api::{ProcessingResult, Tuple2, TxnVariable, VarScope, define_plugin};

/// A test plugin that echoes all message arguments back as transaction
/// variables. Used to verify that all `SpoeValue` type variants arrive
/// correctly and for plugin developer experience testing.
#[derive(Debug)]
struct EchoPlugin;

define_plugin!(EchoPlugin, {
    fn new() -> Self {
        EchoPlugin
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn name(&self) -> &str {
        "echo"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ProcessingResult {
            variables: message
                .args
                .iter()
                .map(|Tuple2(key, value)| TxnVariable {
                    scope: VarScope::Transaction,
                    name: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        })
    }
});
