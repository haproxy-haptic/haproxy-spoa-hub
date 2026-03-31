mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::spop_client::SpopClient;
use spop::types::TypedData;
use tokio::net::UnixListener;

#[tokio::test]
async fn test_noop_plugin_sets_variable() {
    let socket_path = format!(
        "/tmp/spoa-hub-test-noop-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );

    // Clean up stale socket
    let _ = std::fs::remove_file(&socket_path);

    // Load the noop plugin
    let plugin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug");

    let config = haproxy_spoa_hub::config::PluginConfig {
        name: "noop".to_string(),
        library: "libnoop_plugin.so".to_string(),
        messages: vec!["check-request".to_string()],
        depends_on: vec![],
        timeout_ms: Some(1000),
    };

    let loaded = haproxy_spoa_hub::plugin_loader::load_plugin(&plugin_dir, &config, 100)
        .expect("failed to load noop plugin");

    let registry: haproxy_spoa_hub::registry::SharedRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            haproxy_spoa_hub::registry::PluginRegistry::build(vec![loaded])
                .expect("failed to build registry"),
        ));

    // Start listener
    let listener = UnixListener::bind(&socket_path).expect("failed to bind socket");
    let registry_clone = Arc::clone(&registry);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, registry_clone).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("failed to connect");
    let mut client = SpopClient::handshake(stream)
        .await
        .expect("handshake failed");

    // Send a NOTIFY with a "check-request" message
    let ack = client
        .send_notify("check-request", HashMap::new())
        .await
        .expect("send_notify failed");

    // Verify the noop plugin set "noop.result" = "ok"
    let result_var = ack.find_var("noop.result");
    assert!(
        result_var.is_some(),
        "expected noop.result variable in ACK, got actions: {:?}",
        ack.actions
    );
    match result_var.unwrap() {
        TypedData::String(s) => assert_eq!(s, "ok"),
        other => panic!("expected String(\"ok\"), got {other:?}"),
    }

    // Disconnect
    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn test_unregistered_message_returns_empty_ack() {
    let socket_path = format!(
        "/tmp/spoa-hub-test-unreg-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let _ = std::fs::remove_file(&socket_path);

    let plugin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug");

    let config = haproxy_spoa_hub::config::PluginConfig {
        name: "noop".to_string(),
        library: "libnoop_plugin.so".to_string(),
        messages: vec!["check-request".to_string()],
        depends_on: vec![],
        timeout_ms: Some(1000),
    };

    let loaded = haproxy_spoa_hub::plugin_loader::load_plugin(&plugin_dir, &config, 100).unwrap();

    let registry: haproxy_spoa_hub::registry::SharedRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            haproxy_spoa_hub::registry::PluginRegistry::build(vec![loaded]).unwrap(),
        ));

    let listener = UnixListener::bind(&socket_path).unwrap();
    let registry_clone = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, registry_clone).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
    let mut client = SpopClient::handshake(stream).await.unwrap();

    // Send message name that no plugin is registered for
    let ack = client
        .send_notify("unknown-message", HashMap::new())
        .await
        .unwrap();

    // ACK should be empty (no actions)
    assert!(
        ack.actions.is_empty(),
        "expected empty ACK for unregistered message, got: {:?}",
        ack.actions
    );

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
