mod common;

use std::collections::HashMap;
use std::sync::Arc;

use spop::types::TypedData;

#[tokio::test]
async fn test_dual_transport_identical_responses() {
    let registry = common::build_registry(&[common::noop_config()]);

    // --- Unix listener ---
    let socket_path = common::unique_sock("transport");

    let (mut unix_client, unix_server) =
        common::start_server_and_connect(&socket_path, Arc::clone(&registry)).await;

    let unix_ack = unix_client
        .send_notify("check-request", HashMap::new())
        .await
        .expect("unix send_notify failed");

    // --- TCP listener ---
    let (mut tcp_client, tcp_server) =
        common::start_tcp_server_and_connect(Arc::clone(&registry)).await;

    let tcp_ack = tcp_client
        .send_notify("check-request", HashMap::new())
        .await
        .expect("tcp send_notify failed");

    // --- Verify identical responses ---
    let Some(TypedData::String(unix_val)) = unix_ack.find_var("noop.result") else {
        panic!(
            "expected noop.result = String in Unix ACK, got: {:?}",
            unix_ack.actions
        );
    };
    let Some(TypedData::String(tcp_val)) = tcp_ack.find_var("noop.result") else {
        panic!(
            "expected noop.result = String in TCP ACK, got: {:?}",
            tcp_ack.actions
        );
    };
    assert_eq!(unix_val, tcp_val, "Unix and TCP responses differ");
    assert_eq!(unix_val, "ok");
    assert_eq!(
        unix_ack.actions.len(),
        tcp_ack.actions.len(),
        "action count differs"
    );

    // Cleanup
    common::cleanup(unix_client, unix_server, Some(&socket_path)).await;
    common::cleanup(tcp_client, tcp_server, None).await;
}

#[tokio::test]
async fn test_tcp_notify_with_args() {
    let registry = common::build_registry(&[common::noop_config()]);

    let (mut client, server) = common::start_tcp_server_and_connect(registry).await;

    let args = HashMap::from([("key".to_string(), TypedData::String("value".to_string()))]);

    let ack = client
        .send_notify("check-request", args)
        .await
        .expect("tcp send_notify with args failed");

    // noop plugin ignores args but still sets noop.result
    let var = ack.find_var("noop.result");
    assert!(var.is_some(), "expected noop.result in TCP ACK");

    common::cleanup(client, server, None).await;
}

#[tokio::test]
async fn test_unix_notify_with_args() {
    let registry = common::build_registry(&[common::noop_config()]);

    let socket_path = common::unique_sock("unix-args");

    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    let args = HashMap::from([("payload".to_string(), TypedData::Binary(vec![0xDE, 0xAD]))]);

    let ack = client
        .send_notify("check-request", args)
        .await
        .expect("unix send_notify with args failed");

    let var = ack.find_var("noop.result");
    assert!(var.is_some(), "expected noop.result in Unix ACK");

    common::cleanup(client, server, Some(&socket_path)).await;
}
