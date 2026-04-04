mod common;

use std::collections::HashMap;

use haproxy_spoa_hub::test_support::plugin_config;
use spop::types::TypedData;

#[tokio::test]
async fn test_echo_plugin_returns_all_types() {
    let socket_path = common::unique_sock("echo");

    let config = plugin_config("echo", "libecho_plugin.so", &["check-types"], 1000);

    let registry = common::build_registry(&[config]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    let args = HashMap::from([
        (
            "string_val".to_string(),
            TypedData::String("hello".to_string()),
        ),
        ("uint32_val".to_string(), TypedData::UInt32(42)),
        ("bool_val".to_string(), TypedData::Bool(true)),
    ]);

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

    common::cleanup(client, server, Some(&socket_path)).await;
}
