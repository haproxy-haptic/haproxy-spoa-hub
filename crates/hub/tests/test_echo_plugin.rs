mod common;

use std::collections::HashMap;

use spop::types::TypedData;

#[tokio::test]
async fn test_echo_plugin_returns_all_types() {
    let socket_path = common::unique_sock("echo");
    let _ = std::fs::remove_file(&socket_path);

    let config = haproxy_spoa_hub::config::PluginConfig {
        name: "echo".into(),
        library: "libecho_plugin.so".into(),
        messages: vec!["check-types".into()],
        depends_on: vec![],
        timeout_ms: Some(1000),
        params: toml::Table::default(),
    };

    let registry = common::build_registry(&[config]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

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

    common::cleanup(client, server, &socket_path).await;
}
