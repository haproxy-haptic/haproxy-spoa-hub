use std::{net::SocketAddr, time::Duration};

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

pub fn init_metrics(addr: Option<SocketAddr>) -> anyhow::Result<()> {
    if let Some(addr) = addr {
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_message_duration".into()),
                &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0],
            )?
            .add_global_label("service", "haproxy-spoa-hub");

        builder.install()?;
        tracing::info!(%addr, "prometheus metrics endpoint started");
    }

    describe_counter!("spoa_messages_total", "Total SPOP messages processed");
    describe_histogram!(
        "spoa_message_duration_seconds",
        "Per-message processing latency in seconds"
    );
    describe_gauge!(
        "spoa_active_connections",
        "Current number of active SPOP connections"
    );
    describe_counter!("spoa_plugin_errors_total", "Total plugin processing errors");
    describe_counter!("spoa_plugin_timeouts_total", "Total plugin timeout events");
    describe_counter!(
        "spoa_plugin_skips_total",
        "Plugins skipped via hub.skip_plugins arg"
    );
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

    Ok(())
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

pub fn record_plugin_error(plugin: &str) {
    counter!("spoa_plugin_errors_total", "plugin" => plugin.to_owned()).increment(1);
}

pub fn record_plugin_timeout(plugin: &str) {
    counter!("spoa_plugin_timeouts_total", "plugin" => plugin.to_owned()).increment(1);
}

pub fn record_plugin_skip(plugin: &str) {
    counter!("spoa_plugin_skips_total", "plugin" => plugin.to_owned()).increment(1);
}

pub fn update_active_connections(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!("spoa_active_connections").set(count as f64);
}
