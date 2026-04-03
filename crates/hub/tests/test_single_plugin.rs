mod common;

use std::collections::HashMap;
use std::time::Duration;

use spop::types::TypedData;

fn noop_config() -> haproxy_spoa_hub::config::PluginConfig {
    haproxy_spoa_hub::config::PluginConfig {
        name: "noop".to_string(),
        library: "libnoop_plugin.so".to_string(),
        messages: vec!["check-request".to_string()],
        depends_on: vec![],
        timeout_ms: Some(1000),
        params: toml::Table::default(),
    }
}

#[tokio::test]
async fn test_noop_plugin_sets_variable() {
    let socket_path = common::unique_sock("noop");
    let _ = std::fs::remove_file(&socket_path);

    let registry = common::build_registry(vec![noop_config()]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

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
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn test_unregistered_message_returns_empty_ack() {
    let socket_path = common::unique_sock("unreg");
    let _ = std::fs::remove_file(&socket_path);

    let registry = common::build_registry(vec![noop_config()]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

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
