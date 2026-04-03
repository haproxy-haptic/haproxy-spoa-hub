mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::spop_client::SpopClient;
use spop::types::TypedData;
use tokio::net::{TcpListener, TcpStream};

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
async fn test_dual_transport_identical_responses() {
    let registry = common::build_registry(vec![noop_config()]);

    // --- Unix listener ---
    let socket_path = common::unique_sock("transport");
    let _ = std::fs::remove_file(&socket_path);

    let (mut unix_client, unix_server) =
        common::start_server_and_connect(&socket_path, Arc::clone(&registry)).await;

    let unix_ack = unix_client
        .send_notify("check-request", HashMap::new())
        .await
        .expect("unix send_notify failed");

    // --- TCP listener ---
    let tcp_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind tcp");
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let tcp_reg = Arc::clone(&registry);
    let tcp_server = tokio::spawn(async move {
        let (stream, _) = tcp_listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, tcp_reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let tcp_stream = TcpStream::connect(tcp_addr)
        .await
        .expect("failed to connect to tcp");
    let mut tcp_client = SpopClient::handshake(tcp_stream)
        .await
        .expect("tcp handshake failed");

    let tcp_ack = tcp_client
        .send_notify("check-request", HashMap::new())
        .await
        .expect("tcp send_notify failed");

    // --- Verify identical responses ---
    let unix_var = unix_ack.find_var("noop.result");
    let tcp_var = tcp_ack.find_var("noop.result");

    assert!(
        unix_var.is_some(),
        "expected noop.result in Unix ACK, got: {:?}",
        unix_ack.actions
    );
    assert!(
        tcp_var.is_some(),
        "expected noop.result in TCP ACK, got: {:?}",
        tcp_ack.actions
    );

    match (unix_var.unwrap(), tcp_var.unwrap()) {
        (TypedData::String(u), TypedData::String(t)) => {
            assert_eq!(u, t, "Unix and TCP responses differ");
            assert_eq!(u, "ok");
        }
        (u, t) => panic!("expected String variants, got Unix={u:?} TCP={t:?}"),
    }

    assert_eq!(
        unix_ack.actions.len(),
        tcp_ack.actions.len(),
        "action count differs"
    );

    // Cleanup
    unix_client.disconnect().await.ok();
    tcp_client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), unix_server).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), tcp_server).await;
    let _ = std::fs::remove_file(&socket_path);
}

#[tokio::test]
async fn test_tcp_notify_with_args() {
    let registry = common::build_registry(vec![noop_config()]);

    let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let reg = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = tcp_listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = TcpStream::connect(tcp_addr).await.unwrap();
    let mut client = SpopClient::handshake(stream).await.unwrap();

    let mut args = HashMap::new();
    args.insert("key".to_string(), TypedData::String("value".to_string()));

    let ack = client
        .send_notify("check-request", args)
        .await
        .expect("tcp send_notify with args failed");

    // noop plugin ignores args but still sets noop.result
    let var = ack.find_var("noop.result");
    assert!(var.is_some(), "expected noop.result in TCP ACK");

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
}

#[tokio::test]
async fn test_unix_notify_with_args() {
    let registry = common::build_registry(vec![noop_config()]);

    let socket_path = common::unique_sock("unix-args");
    let _ = std::fs::remove_file(&socket_path);

    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    let mut args = HashMap::new();
    args.insert("payload".to_string(), TypedData::Binary(vec![0xDE, 0xAD]));

    let ack = client
        .send_notify("check-request", args)
        .await
        .expect("unix send_notify with args failed");

    let var = ack.find_var("noop.result");
    assert!(var.is_some(), "expected noop.result in Unix ACK");

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
