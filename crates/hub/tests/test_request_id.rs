//! End-to-end behaviour of the `req_id` correlation argument.
//!
//! The unit tests in `request_id.rs` cover `normalize()` itself. These cover the
//! *wiring*: that the hub actually calls it on the real NOTIFY path, and that what
//! a plugin receives is what the hub decided. Deleting the two lines in
//! `connection.rs` leaves every unit test passing and breaks all three of these.
//!
//! The echo example plugin mirrors every argument back as a transaction variable,
//! so the ACK shows exactly what the plugin saw.

mod common;

use std::collections::HashMap;

use haproxy_spoa_hub::request_id::{MAX_LEN, REQ_ID_ARG, is_valid};
use haproxy_spoa_hub::test_support::plugin_config;
use spop::types::TypedData;

/// Sends one NOTIFY through a hub wired to the echo plugin and returns the
/// `req_id` the plugin saw.
async fn req_id_seen_by_plugin(label: &str, args: HashMap<String, TypedData>) -> String {
    let socket_path = common::unique_sock(label);
    let config = plugin_config("echo", "libecho_plugin.so", &["check-req-id"], 1000);
    let registry = common::build_registry(&[config]);
    let (mut client, _server) = common::start_server_and_connect(&socket_path, registry).await;

    let ack = client.send_notify("check-req-id", args).await.unwrap();

    match ack.find_var(&format!("echo.{REQ_ID_ARG}")) {
        Some(TypedData::String(s)) => s.clone(),
        other => panic!("expected a string {REQ_ID_ARG} in the ACK, got {other:?}"),
    }
}

#[tokio::test]
async fn supplied_req_id_reaches_the_plugin_unchanged() {
    let want = "019f9e64-e9de-7d1b-88c9-76644f0e9b86";
    let args = HashMap::from([(REQ_ID_ARG.to_string(), TypedData::String(want.to_string()))]);

    let seen = req_id_seen_by_plugin("reqid-adopt", args).await;

    assert_eq!(
        seen, want,
        "HAProxy's id must reach the plugin byte-for-byte — otherwise the hub's \
         logs and HAProxy's access log name the same request differently"
    );
}

#[tokio::test]
async fn missing_req_id_is_generated_by_the_hub() {
    let args = HashMap::from([(
        "other_arg".to_string(),
        TypedData::String("value".to_string()),
    )]);

    let seen = req_id_seen_by_plugin("reqid-generate", args).await;

    assert!(
        !seen.is_empty(),
        "a message without req_id must still reach the plugin with one"
    );
    assert!(
        is_valid(&seen),
        "a generated id must satisfy the same rule as an adopted one, so nothing \
         downstream has to branch on the source: got {seen:?}"
    );
}

/// The security-relevant path. An id that arrives malformed must be replaced
/// before any plugin sees it: a plugin that logs the value would otherwise let a
/// newline or a quote break its log record, and an unbounded value would grow it
/// without limit.
#[tokio::test]
async fn unsafe_req_id_is_replaced_before_any_plugin_sees_it() {
    let over_long = "x".repeat(MAX_LEN + 1);
    let cases: [(&str, TypedData); 7] = [
        ("empty", TypedData::String(String::new())),
        ("space", TypedData::String("a b".to_string())),
        ("newline", TypedData::String("x\ny".to_string())),
        ("tab", TypedData::String("tab\there".to_string())),
        ("quote", TypedData::String("quote\"y".to_string())),
        ("over MAX_LEN", TypedData::String(over_long.clone())),
        ("wrong type", TypedData::UInt32(7)),
    ];

    for (what, bad) in cases {
        let original = match &bad {
            TypedData::String(s) => Some(s.clone()),
            _ => None,
        };
        let args = HashMap::from([(REQ_ID_ARG.to_string(), bad)]);

        let seen = req_id_seen_by_plugin("reqid-reject", args).await;

        assert!(
            is_valid(&seen),
            "replacement for the {what} case must be valid, got {seen:?}"
        );
        if let Some(orig) = original {
            assert_ne!(
                seen, orig,
                "the rejected {what} value must never reach a plugin"
            );
        }
    }
}
