//! Example plugin that opts into the plugin-API v2 metrics path. Every
//! call to `process()` emits one counter increment, one gauge set, and
//! one histogram observation, exercising all three
//! [`haproxy_spoa_hub_plugin_api::MetricKind`] variants. The hub's
//! integration tests load this plugin to verify
//! the end-to-end dispatch (plugin -> recorder fn -> metrics registry)
//! actually lands each kind in the right primitive with the expected
//! `plugin_<plugin>_` name prefix and labels.
//!
//! Plugin authors writing their first metric-emitting plugin can read
//! this as the canonical small example. Note the `static METRICS`
//! pattern: `define_plugin!` body blocks can't reference `self` (Rust
//! macro hygiene puts `self` out of scope), so plugins reach state
//! through a static, exactly as every other plugin in the repo does
//! for its own state.
#![allow(non_camel_case_types, non_local_definitions)]

use haproxy_spoa_hub_plugin_api::{
    MetricRecorder, ProcessingResult, SpoeValue, TxnVariable, define_plugin,
};

/// Plugin-scoped metric recorder. The macro's `metrics_static` arm
/// below points the v2 `set_metric_recorder` install at this static, so
/// the hub's dispatcher binds here once between `create` and `init`.
static METRICS: MetricRecorder = MetricRecorder::new();

#[derive(Debug)]
struct MetricsNoopPlugin;

define_plugin!(MetricsNoopPlugin, {
    fn new() -> Self {
        MetricsNoopPlugin
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn name(&self) -> &str {
        "metrics-noop"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        _message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        // One of each kind. The integration test verifies all three
        // land in the registry with the expected name prefix +
        // labels, and that the histogram observation is recorded as
        // a histogram (not a counter/gauge that happens to compile).
        METRICS.counter("dispatches_total", &[("kind", "ok")], 1);
        METRICS.gauge("in_flight", &[("scope", "demo")], 7.0);
        METRICS.histogram("latency_seconds", &[("path", "/x")], 0.012);
        Ok(ProcessingResult::single(TxnVariable::session(
            "result",
            SpoeValue::String("ok".into()),
        )))
    }

    metrics_static = METRICS;
});
