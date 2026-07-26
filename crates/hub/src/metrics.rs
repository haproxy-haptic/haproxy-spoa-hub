use std::{net::SocketAddr, time::Duration};

use metrics::{
    Counter, Gauge, Histogram, counter, describe_counter, describe_gauge, describe_histogram,
    gauge, histogram,
};

pub fn init_metrics(addr: Option<SocketAddr>) -> anyhow::Result<()> {
    // Latency buckets tuned to where the SPOE data path actually lands: fine
    // resolution across the 1ms–10ms band (hub processing and WAF evaluation sit
    // at ~1.5ms mean), so p99 is resolvable instead of collapsing into a coarse
    // 1ms→5ms→10ms gap that reports every quantile as "5ms". Coarser above 10ms
    // where only outliers live, and sub-millisecond points for the cheap plugins.
    const LATENCY_BUCKETS: &[f64] = &[
        0.0001, 0.0005, 0.001, 0.002, 0.003, 0.005, 0.0075, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0,
    ];
    if let Some(addr) = addr {
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_message_duration".into()),
                LATENCY_BUCKETS,
            )?
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_plugin_duration".into()),
                LATENCY_BUCKETS,
            )?
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_backpressure_wait".into()),
                LATENCY_BUCKETS,
            )?
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_ack_flush_duration".into()),
                LATENCY_BUCKETS,
            )?
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full("spoa_ack_batch_size".into()),
                &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0],
            )?
            .add_global_label("service", "haproxy-spoa-hub");

        builder.install()?;
        tracing::info!(%addr, "prometheus metrics endpoint started");
    }

    describe_metrics();
    Ok(())
}

/// Register the description text for every metric the hub emits.
/// Split out of [`init_metrics`] so the exporter wiring and the
/// (long) catalogue of descriptions stay independently readable.
fn describe_metrics() {
    describe_counter!("spoa_messages_total", "Total SPOP messages processed");
    describe_counter!(
        "spoa_request_id_source_total",
        "Where each message's correlation id came from. `adopted` means HAProxy \
         sent a usable `req_id` argument, so the hub logs the same id as \
         HAProxy's access log. `generated` means none arrived, which is expected \
         when the SPOE config predates the argument. `rejected` means one arrived \
         and failed validation — the one worth alerting on, because the hub and \
         the access log then name every request differently and nothing else \
         surfaces it. The id itself is never a label: it is per-request, so it \
         would be unbounded cardinality."
    );
    describe_counter!(
        "spoa_messages_unhandled_total",
        "NOTIFY messages for which no plugin was registered in the \
         current configuration. The hub still ACKs the frame (SPOP has \
         no message-level error mechanism) but the message has no \
         effect — from HAProxy's point of view the work is dropped. \
         Labelled by message name. Steady growth means haproxy's SPOE \
         engine (spoe.conf) and the hub's TOML disagree about the \
         message set — either a misconfiguration, or a config reload \
         shrank the hub's `messages` list while HAProxy's previous \
         configuration generation keeps firing the removed message \
         (e.g. `send-spoe-group` for a message dropped by an upstream \
         controller's transient render)."
    );
    describe_histogram!(
        "spoa_message_duration_seconds",
        "Per-message processing latency in seconds"
    );
    describe_gauge!(
        "spoa_active_connections",
        "Current number of active SPOP connections"
    );
    describe_gauge!(
        "spoa_inflight_frames",
        "NOTIFY frames currently being processed (holding a shared in-flight gate permit)"
    );
    describe_gauge!(
        "spoa_inflight_gate_capacity",
        "Capacity of the shared in-flight gate. When spoa_inflight_frames reaches \
         it, connection read loops stop reading and HAProxy is backpressured \
         (the excess queues in HAProxy's buffers instead of overloading the hub)."
    );
    describe_counter!(
        "spoa_backpressure_waits_total",
        "Times a connection read loop had to wait for an in-flight gate permit \
         before reading the next frame — i.e. HAProxy was backpressured. A rising \
         rate means the hub is at capacity and shedding load back to HAProxy."
    );
    describe_histogram!(
        "spoa_backpressure_wait_seconds",
        "How long a connection read loop blocked waiting for an in-flight gate \
         permit (observed only when the gate was full, so it is the wait tail, not \
         a per-frame latency). This is the one segment of request latency added by \
         the shared backpressure gate; a growing tail means the gate is the \
         bottleneck. Pairs with spoa_backpressure_waits_total (the count)."
    );
    describe_histogram!(
        "spoa_ack_flush_duration_seconds",
        "Time the per-connection ACK writer spent in a single flush (one syscall \
         batch) writing completed replies back to HAProxy. Covers the return-path \
         serialization+write that spoa_message_duration_seconds excludes."
    );
    describe_histogram!(
        "spoa_ack_batch_size",
        "Number of ACK frames coalesced into one flush by the per-connection ACK \
         writer. Higher values mean SPOP pipelining is letting concurrent replies \
         share a syscall; a distribution pinned at 1 means replies are not \
         overlapping (low concurrency or serialized load)."
    );
    describe_plugin_metrics();
    describe_reload_metrics();
}

fn describe_plugin_metrics() {
    describe_counter!("spoa_plugin_errors_total", "Total plugin processing errors");
    describe_counter!("spoa_plugin_timeouts_total", "Total plugin timeout events");
    describe_counter!(
        "spoa_plugin_overloads_total",
        "Plugin calls rejected because the configured concurrency and bounded queue were full"
    );
    describe_counter!(
        "spoa_plugin_queue_timeouts_total",
        "Plugin calls rejected after their bounded queue wait expired"
    );
    describe_histogram!(
        "spoa_plugin_duration_seconds",
        "Per-plugin service time (process() duration, excluding permit wait) in seconds"
    );
    describe_gauge!(
        "spoa_plugin_in_flight",
        "Plugin calls actually executing, including timed-out blocking calls that have not returned"
    );
    describe_gauge!(
        "spoa_plugin_queued",
        "Plugin calls currently waiting for a bounded concurrency permit"
    );
    describe_counter!(
        "spoa_plugin_skips_total",
        "Plugins skipped via hub.skip_plugins or failed dependency"
    );
    describe_gauge!(
        "spoa_plugin_adaptive_limit",
        "Current adaptive concurrency ceiling the Gradient2 controller (ADR-0002) \
         is holding for this plugin. Emitted only for plugins with \
         adaptive_concurrency = true. Read alongside spoa_plugin_in_flight to see \
         how close the plugin is to the moving limit it is pressing against."
    );
    describe_gauge!(
        "spoa_plugin_adaptive_baseline_rtt_seconds",
        "The Gradient2 controller's smoothed no-load service-time baseline \
         (long_rtt) for this plugin, in seconds. The controller shrinks the limit \
         when current service time rises above this baseline by more than its \
         tolerance. Emitted only for adaptive_concurrency = true plugins."
    );
}

fn describe_reload_metrics() {
    describe_counter!(
        "spoa_hub_config_reload_success_total",
        "Successful config reloads (SIGHUP or file-watch trigger)"
    );
    describe_counter!(
        "spoa_hub_config_reload_failure_total",
        "Config reloads that failed parse / plugin re-init; previous \
         config is retained in memory"
    );
    describe_gauge!(
        "spoa_hub_config_reload_age_seconds",
        "Seconds since the last successful config reload. Set to 0 at \
         startup; updated on each successful reload. A growing value \
         while operators expect changes to apply means the file-watch \
         trigger is stalled or every recent reload attempt failed — \
         cross-check against spoa_hub_config_reload_failure_total."
    );
    describe_counter!(
        "spoa_hub_config_reload_skipped_total",
        "Config file changes that were detected but skipped because the \
         file content matched the last successfully-loaded version \
         (byte-identical write — common when an upstream controller \
         rewrites the config every reconcile regardless of content). \
         Skipping these avoids tearing down in-flight plugin work; \
         a growing counter with low (or zero) success/failure activity \
         is a signal that the upstream writer is doing unnecessary work."
    );
    describe_histogram!(
        "spoa_hub_config_reload_drain_duration_seconds",
        "Wall-clock time spent retiring the previous registry on a \
         config reload: waiting for in-flight dispatches holding the \
         pre-swap registry snapshot to complete (quiesce) plus waiting \
         for plugins' background work to drain, before shutdown. One \
         observation per reload that actually reached the swap → \
         quiesce → drain → shutdown path (skipped reloads and failed \
         reloads don't observe). A plot near zero is the common case \
         (synchronous plugins, idle traffic). Approaching the \
         configured drain timeout indicates a plugin (or upstream \
         it depends on) is consistently slow — cross-check against \
         spoa_hub_config_reload_drain_forced_total{plugin} and \
         spoa_hub_config_reload_quiesce_forced_total to find the \
         phase and plugin responsible."
    );
    describe_counter!(
        "spoa_hub_config_reload_quiesce_forced_total",
        "Reloads where in-flight NOTIFY dispatches still held the \
         pre-swap registry snapshot when the drain budget expired. The \
         old plugin set was drained and shut down anyway, so those \
         dispatches' fire-and-forget follow-up work may be lost. \
         Dispatch duration is bounded by the per-plugin timeouts, so a \
         growing counter means the drain budget is smaller than the \
         slowest plugin's dispatch latency — raise \
         [hub] reload_drain_timeout_ms or lower the plugin timeout."
    );
    describe_counter!(
        "spoa_hub_config_reload_drain_forced_total",
        "Plugins that did NOT report a clean drain within the reload \
         drain budget and got force-shutdown anyway (losing whatever \
         in-flight requests they still had outstanding). One \
         increment per (reload, plugin) pair that hit the deadline. \
         Labelled by plugin name. A growing counter on a specific \
         plugin during heavy traffic is the signal that operators \
         need to raise [hub] reload_drain_timeout_ms or that the \
         plugin's upstream is slow."
    );
}

/// Per-plugin metric handles, resolved once at registry-build time.
///
/// The `metrics` crate's `counter!`/`gauge!`/`histogram!` macros hash the
/// metric name plus its labels and look the handle up in a global registry on
/// **every** call, and each call allocates the label `String`. On the dispatch
/// hot path that is several allocations plus registry lookups per plugin call —
/// every coraza evaluation records in-flight ±1 and a duration sample. Resolving
/// the handles once here moves all of that off the per-frame path; recording
/// then costs a single atomic op on the cached handle.
pub struct PluginMetrics {
    pub in_flight: Gauge,
    pub queued: Gauge,
    pub duration: Histogram,
    pub errors: Counter,
    pub timeouts: Counter,
    pub overloads: Counter,
    pub queue_timeouts: Counter,
    pub skips: Counter,
}

impl PluginMetrics {
    /// Resolve every per-plugin metric handle once, tagged with `plugin`.
    #[must_use]
    pub fn new(plugin: &str) -> Self {
        Self {
            in_flight: gauge!("spoa_plugin_in_flight", "plugin" => plugin.to_owned()),
            queued: gauge!("spoa_plugin_queued", "plugin" => plugin.to_owned()),
            duration: histogram!("spoa_plugin_duration_seconds", "plugin" => plugin.to_owned()),
            errors: counter!("spoa_plugin_errors_total", "plugin" => plugin.to_owned()),
            timeouts: counter!("spoa_plugin_timeouts_total", "plugin" => plugin.to_owned()),
            overloads: counter!("spoa_plugin_overloads_total", "plugin" => plugin.to_owned()),
            queue_timeouts: counter!(
                "spoa_plugin_queue_timeouts_total", "plugin" => plugin.to_owned()
            ),
            skips: counter!("spoa_plugin_skips_total", "plugin" => plugin.to_owned()),
        }
    }
}

/// Per-message metric handles, resolved once at registry-build time (one per
/// message the hub is configured to handle). Recorded once per NOTIFY message on
/// the hot path; see [`PluginMetrics`] for the rationale. Messages the hub is
/// *not* configured for use the uncached [`record_message_processed`] fallback,
/// since their names are not known ahead of time.
pub struct MessageMetrics {
    pub duration: Histogram,
    pub total: Counter,
}

impl MessageMetrics {
    /// Resolve the per-message metric handles once, tagged with `message`.
    #[must_use]
    pub fn new(message: &str) -> Self {
        Self {
            duration: histogram!("spoa_message_duration_seconds", "message" => message.to_owned()),
            total: counter!("spoa_messages_total", "message" => message.to_owned()),
        }
    }
}

/// Record a successful config reload. Increments the success counter
/// and resets the age gauge to 0. Called from `main.rs` when the new
/// `arc_swap` registry has been swapped in.
pub fn record_config_reload_success() {
    counter!("spoa_hub_config_reload_success_total").increment(1);
    gauge!("spoa_hub_config_reload_age_seconds").set(0.0);
}

/// Record a config reload that failed parse or plugin re-init. The
/// previous config is still in memory; this counter tracks how often
/// operators are pushing broken configs (or how often the file-watch
/// fires on a half-written file). Does not touch the age gauge — the
/// last successful reload's age keeps growing until the next success.
pub fn record_config_reload_failure() {
    counter!("spoa_hub_config_reload_failure_total").increment(1);
}

/// Record a config reload that was skipped because the file content
/// matched the last successfully-loaded version. Skipping these is
/// load-bearing: a reload tears down the in-flight plugin state, and
/// fire-and-forget dispatchers (mirror, etc.) that haven't completed
/// their background tokio task lose the request. Upstream writers
/// that rewrite the file every reconcile (atomic-rename without
/// content compare) trigger the file-watch unconditionally — without
/// this dedup, every such rewrite would drop traffic mid-flight.
pub fn record_config_reload_skipped() {
    counter!("spoa_hub_config_reload_skipped_total").increment(1);
}

/// Record the wall-clock duration of retiring the previous registry
/// on a config reload (quiesce wait + plugin drain). Called once per
/// reload that reaches swap → quiesce → drain → shutdown.
/// Skipped/failed reloads don't observe.
pub fn record_config_reload_drain_duration(duration: Duration) {
    histogram!("spoa_hub_config_reload_drain_duration_seconds").record(duration.as_secs_f64());
}

/// Record a reload whose quiesce phase hit the deadline: at least one
/// in-flight dispatch still held the pre-swap registry snapshot when
/// the budget expired, and the old plugin set was drained and shut
/// down anyway.
pub fn record_config_reload_quiesce_forced() {
    counter!("spoa_hub_config_reload_quiesce_forced_total").increment(1);
}

/// Record a plugin that did not drain cleanly within the reload
/// budget. One increment per (reload, plugin) pair that timed out.
/// Operators reading this on a specific plugin should either raise
/// `[hub] reload_drain_timeout_ms` or investigate why the plugin's
/// upstream is slow.
pub fn record_config_reload_drain_forced(plugin: &str) {
    counter!("spoa_hub_config_reload_drain_forced_total", "plugin" => plugin.to_owned())
        .increment(1);
}

/// Update the age gauge from a "seconds since last success" value
/// computed by the caller. Called periodically by main.rs from a
/// 30-second tick task.
pub fn update_config_reload_age(seconds: f64) {
    gauge!("spoa_hub_config_reload_age_seconds").set(seconds);
}

pub fn record_message_processed(message: &str, duration: Duration) {
    histogram!("spoa_message_duration_seconds", "message" => message.to_owned())
        .record(duration.as_secs_f64());
    counter!("spoa_messages_total", "message" => message.to_owned()).increment(1);
}

/// Record a NOTIFY message that no plugin in the current configuration
/// is registered for. The frame is still acknowledged (SPOP has no
/// message-level error mechanism) but the message had no effect —
/// the work `HAProxy` asked for was dropped.
/// Records where a message's correlation id came from.
///
/// The label is a `&'static str` from a three-variant enum, so this adds exactly
/// three series and allocates nothing per request.
pub fn record_request_id_source(source: crate::request_id::Source) {
    counter!("spoa_request_id_source_total", "source" => source.as_str()).increment(1);
}

pub fn record_message_unhandled(message: &str) {
    counter!("spoa_messages_unhandled_total", "message" => message.to_owned()).increment(1);
}

pub fn update_active_connections(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!("spoa_active_connections").set(count as f64);
}

/// Change the count of NOTIFY frames currently in flight (holding a shared
/// in-flight gate permit). +1 when a frame starts processing, -1 when it ends.
pub fn record_inflight_delta(delta: f64) {
    gauge!("spoa_inflight_frames").increment(delta);
}

/// Publish the in-flight gate's capacity once at startup so operators can read
/// saturation as `spoa_inflight_frames / spoa_inflight_gate_capacity`.
pub fn set_inflight_gate_capacity(capacity: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!("spoa_inflight_gate_capacity").set(capacity as f64);
}

/// Count one backpressure event: a read loop blocked waiting for an in-flight
/// gate permit (the gate was full), so the peer was held off reading.
pub fn record_backpressure_wait() {
    counter!("spoa_backpressure_waits_total").increment(1);
}

/// Record how long a read loop blocked before it obtained an in-flight gate
/// permit. Observed only on the backpressured path (the gate was full), so this
/// is the wait-time tail — the one latency segment the shared gate adds.
pub fn record_backpressure_wait_seconds(duration: Duration) {
    histogram!("spoa_backpressure_wait_seconds").record(duration.as_secs_f64());
}

/// Record one ACK-writer flush: `batch_size` frames coalesced into the flush and
/// the wall-clock time the flush (serialize + write) took. Called once per flush
/// (per batch), not per frame, so the macro lookup cost stays off the per-frame
/// path.
pub fn record_ack_flush(batch_size: usize, duration: Duration) {
    #[allow(clippy::cast_precision_loss)]
    histogram!("spoa_ack_batch_size").record(batch_size as f64);
    histogram!("spoa_ack_flush_duration_seconds").record(duration.as_secs_f64());
}
