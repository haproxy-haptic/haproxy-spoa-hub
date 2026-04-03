use std::net::SocketAddr;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

pub fn init_metrics(addr: Option<SocketAddr>) -> anyhow::Result<()> {
    if let Some(addr) = addr {
        let builder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Prefix("spoa_message_duration".to_string()),
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

    Ok(())
}

pub fn record_message_processed(message: &str, duration: std::time::Duration) {
    let message = message.to_owned();
    counter!("spoa_messages_total", "message" => message.clone()).increment(1);
    histogram!("spoa_message_duration_seconds", "message" => message)
        .record(duration.as_secs_f64());
}

pub fn record_plugin_error(plugin: &str) {
    counter!("spoa_plugin_errors_total", "plugin" => plugin.to_string()).increment(1);
}

pub fn record_plugin_timeout(plugin: &str) {
    counter!("spoa_plugin_timeouts_total", "plugin" => plugin.to_string()).increment(1);
}

pub fn record_plugin_skip(plugin: &str) {
    counter!("spoa_plugin_skips_total", "plugin" => plugin.to_string()).increment(1);
}

pub fn update_active_connections(count: f64) {
    gauge!("spoa_active_connections").set(count);
}
