mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use spop::types::TypedData;

fn delay_config(
    name: &str,
    message: &str,
    timeout_ms: u64,
) -> haproxy_spoa_hub::config::PluginConfig {
    haproxy_spoa_hub::config::PluginConfig {
        name: name.into(),
        library: "libdelay_plugin.so".into(),
        messages: vec![message.into()],
        depends_on: vec![],
        timeout_ms: Some(timeout_ms),
        params: toml::Table::default(),
    }
}

#[tokio::test]
async fn test_parallel_plugins_execute_concurrently() {
    let socket_path = common::unique_sock("parallel");
    let _ = std::fs::remove_file(&socket_path);

    let registry = common::build_registry(&[
        delay_config("delay-a", "check-parallel", 2000),
        delay_config("delay-b", "check-parallel", 2000),
    ]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    // Both delay plugins sleep for 50ms by default (configured via delay_ms arg).
    // If parallel: total ~50ms. If sequential: total ~100ms.
    let mut args = HashMap::new();
    args.insert("delay_ms".to_string(), TypedData::UInt32(50));

    let start = Instant::now();
    let ack = client.send_notify("check-parallel", args).await.unwrap();
    let elapsed = start.elapsed();

    // Both plugins should have set their "done" variable
    assert!(
        ack.find_var("delay-a.done").is_some(),
        "delay-a.done missing, got: {:?}",
        ack.actions
    );
    assert!(
        ack.find_var("delay-b.done").is_some(),
        "delay-b.done missing, got: {:?}",
        ack.actions
    );

    // Parallel execution: should complete in roughly 50ms, not 100ms.
    // Allow generous margin for CI overhead.
    assert!(
        elapsed < Duration::from_millis(200),
        "expected parallel execution (<200ms), took {elapsed:?}",
    );

    common::cleanup(client, server, &socket_path).await;
}

#[tokio::test]
async fn test_plugin_timeout_returns_partial_results() {
    let socket_path = common::unique_sock("timeout");
    let _ = std::fs::remove_file(&socket_path);

    // fast plugin: 2000ms timeout (will complete)
    // slow plugin: 50ms timeout (will expire with 500ms delay)
    let registry = common::build_registry(&[
        delay_config("fast", "check-timeout", 2000),
        delay_config("slow", "check-timeout", 50),
    ]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    // Both plugins see delay_ms=500.
    // Fast: 500ms delay, 2000ms timeout → completes.
    // Slow: 500ms delay, 50ms timeout → times out.
    let mut args = HashMap::new();
    args.insert("delay_ms".to_string(), TypedData::UInt32(500));

    let ack = client.send_notify("check-timeout", args).await.unwrap();

    assert!(
        ack.find_var("fast.done").is_some(),
        "fast.done missing (should complete within 2000ms timeout), got: {:?}",
        ack.actions
    );
    // Slow should have timed out — its variable should NOT be present
    assert!(
        ack.find_var("slow.done").is_none(),
        "slow.done should be absent (timed out), but got: {:?}",
        ack.actions
    );

    common::cleanup(client, server, &socket_path).await;
}
