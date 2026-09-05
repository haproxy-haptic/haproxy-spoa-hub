mod common;

use std::collections::HashMap;

use spop::types::TypedData;

/// The mock client announces `max-frame-size: 16384` in HAPROXY-HELLO, so the
/// handshake narrows the codec to that. A NOTIFY above it must be refused on
/// the length prefix instead of buffered and parsed.
#[tokio::test]
async fn notify_above_the_negotiated_max_frame_size_drops_the_connection() {
    let socket_path = common::unique_sock("frame-size");

    let registry = common::build_registry(&[common::noop_config()]);
    let (mut client, server) = common::start_server_and_connect(&socket_path, registry).await;

    let mut args = HashMap::new();
    args.insert(
        "oversized".to_string(),
        TypedData::String("x".repeat(20_000)),
    );

    let result = client.send_notify("check-request", args).await;

    assert!(
        result.is_err(),
        "expected the hub to drop the connection, got an ACK: {result:?}"
    );

    common::cleanup(client, server, Some(&socket_path)).await;
}
