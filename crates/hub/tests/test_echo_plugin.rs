mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::spop_client::SpopClient;
use spop::types::TypedData;
use tokio::net::UnixListener;

#[tokio::test]
async fn test_echo_plugin_returns_all_types() {
    let socket_path = common::unique_sock("echo");
    let _ = std::fs::remove_file(&socket_path);

    let plugin_dir = common::plugin_dir();

    let config = haproxy_spoa_hub::config::PluginConfig {
        name: "echo".to_string(),
        library: "libecho_plugin.so".to_string(),
        messages: vec!["check-types".to_string()],
        depends_on: vec![],
        timeout_ms: Some(1000),
        params: toml::Table::default(),
    };

    let loaded = haproxy_spoa_hub::plugin_loader::load_plugin(&plugin_dir, &config, 100)
        .expect("failed to load echo plugin");

    let registry: haproxy_spoa_hub::registry::SharedRegistry =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            haproxy_spoa_hub::registry::PluginRegistry::build(vec![loaded])
                .expect("failed to build registry"),
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

    let mut args = HashMap::new();
    args.insert(
        "string_val".to_string(),
        TypedData::String("hello".to_string()),
    );
    args.insert("uint32_val".to_string(), TypedData::UInt32(42));
    args.insert("bool_val".to_string(), TypedData::Bool(true));

    let ack = client.send_notify("check-types", args).await.unwrap();

    let string_var = ack.find_var("echo.string_val");
    assert!(
        string_var.is_some(),
        "missing string_val, got: {:?}",
        ack.actions
    );
    match string_var.unwrap() {
        TypedData::String(s) => assert_eq!(s, "hello"),
        other => panic!("expected String, got {other:?}"),
    }
    assert!(
        ack.find_var("echo.uint32_val").is_some(),
        "missing uint32_val"
    );
    assert!(ack.find_var("echo.bool_val").is_some(), "missing bool_val");

    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server).await;
    let _ = std::fs::remove_file(&socket_path);
}
