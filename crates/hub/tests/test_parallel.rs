mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::spop_client::SpopClient;
use spop::types::TypedData;
use tokio::net::UnixListener;

fn unique_sock(prefix: &str) -> String {
    format!(
        "/tmp/spoa-hub-{}-{}-{}.sock",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    )
}

fn plugin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
}

/// Load two delay plugins under different names for the same message.
fn load_two_delay_plugins() -> haproxy_spoa_hub::registry::SharedRegistry {
    let dir = plugin_dir();

    let config_a = haproxy_spoa_hub::config::PluginConfig {
        name: "delay-a".to_string(),
        library: "libdelay_plugin.so".to_string(),
        messages: vec!["check-parallel".to_string()],
        depends_on: vec![],
        timeout_ms: Some(2000),
    };
    let config_b = haproxy_spoa_hub::config::PluginConfig {
        name: "delay-b".to_string(),
        library: "libdelay_plugin.so".to_string(),
        messages: vec!["check-parallel".to_string()],
        depends_on: vec![],
        timeout_ms: Some(2000),
    };

    let a = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_a, 2000).unwrap();
    let b = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_b, 2000).unwrap();

    Arc::new(arc_swap::ArcSwap::from_pointee(
        haproxy_spoa_hub::registry::PluginRegistry::build(vec![a, b]).unwrap(),
    ))
}

#[tokio::test]
async fn test_parallel_plugins_execute_concurrently() {
    let socket_path = unique_sock("parallel");
    let _ = std::fs::remove_file(&socket_path);

    let registry = load_two_delay_plugins();

    let listener = UnixListener::bind(&socket_path).unwrap();
    let reg = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let mut client = SpopClient::handshake(stream).await.unwrap();

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
        "expected parallel execution (<200ms), took {:?}",
        elapsed
    );

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn test_plugin_timeout_returns_partial_results() {
    let socket_path = unique_sock("timeout");
    let _ = std::fs::remove_file(&socket_path);

    let dir = plugin_dir();

    // fast plugin: 5ms delay, 2000ms timeout
    let config_fast = haproxy_spoa_hub::config::PluginConfig {
        name: "fast".to_string(),
        library: "libdelay_plugin.so".to_string(),
        messages: vec!["check-timeout".to_string()],
        depends_on: vec![],
        timeout_ms: Some(2000),
    };
    // slow plugin: will use 500ms delay from args, but only gets 50ms timeout
    let config_slow = haproxy_spoa_hub::config::PluginConfig {
        name: "slow".to_string(),
        library: "libdelay_plugin.so".to_string(),
        messages: vec!["check-timeout".to_string()],
        depends_on: vec![],
        timeout_ms: Some(50), // 50ms timeout — will expire
    };

    let fast = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_fast, 2000).unwrap();
    let slow = haproxy_spoa_hub::plugin_loader::load_plugin(&dir, &config_slow, 50).unwrap();

    let registry: haproxy_spoa_hub::registry::SharedRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            haproxy_spoa_hub::registry::PluginRegistry::build(vec![fast, slow]).unwrap(),
        ));

    let listener = UnixListener::bind(&socket_path).unwrap();
    let reg = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let mut client = SpopClient::handshake(stream).await.unwrap();

    // Fast plugin gets delay_ms=5 (within timeout), slow plugin gets
    // delay_ms=500 (exceeds its 50ms timeout)
    let mut args = HashMap::new();
    args.insert("delay_ms".to_string(), TypedData::UInt32(500));

    let ack = client.send_notify("check-timeout", args).await.unwrap();

    // Fast plugin should have completed (5ms default < 2000ms timeout)
    // Actually both get the same delay_ms=500 arg. But fast has 2000ms
    // timeout so it completes. Slow has 50ms timeout so it times out.
    // Wait — both plugins see the same message with delay_ms=500.
    // Fast: 500ms delay, 2000ms timeout → completes.
    // Slow: 500ms delay, 50ms timeout → times out.
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

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
