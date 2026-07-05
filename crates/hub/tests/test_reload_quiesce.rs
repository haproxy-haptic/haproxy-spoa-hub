//! Integration tests for the reload quiesce phase (swap → quiesce →
//! drain → shutdown) and the unhandled-NOTIFY loudness guarantee.
//!
//! Regression coverage for haptic race B (haptic issue #47 /
//! `docs/adr/0009` there): on a config reload, NOTIFY frames that
//! loaded the pre-swap registry snapshot were still dispatching while
//! the hub drained and retired the old plugin set. The drain ran
//! before those dispatches had enqueued their fire-and-forget
//! background work (the mirror plugin's `runtime.spawn`'d HTTP
//! request), observed nothing in flight, and the work died with the
//! old plugin generation — a silent drop of ~50% of in-flight
//! `send-spoe-group` NOTIFYs around each reload, seen as
//! gateway-conformance mirror flakiness.
//!
//! The hub-side surface under test is `registry::swap_and_retire` —
//! the same function `apply_reload` calls (same rationale as
//! `test_drain.rs` testing `drain_all` directly: going through
//! `apply_reload` would couple this to file-watch / SIGHUP plumbing
//! exercised elsewhere).
//!
//! Single test, not three: the drainable plugin's `IN_FLIGHT` /
//! `CANCELLED` statics are process-scoped (dlopen on the same `.so`
//! reuses the handle), and the `DebuggingRecorder` is process-global.
//! Sequencing the scenarios inside one test keeps both under control
//! without a `serial_test` dep — same pattern as `test_drain.rs`.
//!
//! All waits are condition-polls with generous deadlines that only
//! bite on failure; no assertion depends on the scheduler winning a
//! race. The one bounded `timeout` (scenario 1's "retire must still
//! be pending") is safe in the fixed code by construction: the held
//! snapshot keeps the old registry's strong count above 1, so the
//! quiesce loop cannot complete regardless of scheduling.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use haproxy_spoa_hub::registry::{PluginRegistry, shared_registry, swap_and_retire};
use haproxy_spoa_hub::test_support::{load_test_plugins, plugin_config};
use haproxy_spoa_hub_plugin_api::{RString, SpoeMessage, SpoeValue};
use metrics::{Key, Label};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use metrics_util::{CompositeKey, MetricKind};
use spop::types::TypedData;

fn drainable_config() -> haproxy_spoa_hub::config::PluginConfig {
    // Generous plugin timeout: scenario 2 holds process() open for
    // 500ms (dispatch_ms) and nothing here should trip the dispatch
    // timeout.
    plugin_config(
        "drainable",
        "libdrainable_plugin.so",
        &["check-request"],
        5000,
    )
}

/// Build an empty registry standing in for a reload whose config no
/// longer declares the message (the shrunken-`messages`-list case that
/// triggers race B in production).
fn empty_registry() -> PluginRegistry {
    PluginRegistry::build(vec![]).expect("empty registry builds")
}

/// Poll `cond` every 5ms until it holds or `deadline` elapses
/// (assert-failing with `what` in the latter case).
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + deadline;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn reload_quiesce_semantics() {
    // Process-global recorder for the unhandled-NOTIFY counter
    // assertion. Installed first so every metric emitted below is
    // captured.
    let recorder = DebuggingRecorder::new();
    let snap = recorder.snapshotter();
    recorder
        .install()
        .expect("install DebuggingRecorder (no other recorder may be set in this test binary)");

    scenario_retire_waits_for_epoch_holders().await;
    scenario_inflight_notify_survives_reload().await;
    scenario_unhandled_notify_is_loud(&snap).await;
}

// --- Scenario 1: retire waits for in-flight epoch holders. ------------
//
// A held `load_full()` snapshot stands in for what
// `handle_notify_frame` holds for the duration of a NOTIFY frame.
// swap_and_retire must (a) make the swap visible to new frames
// immediately, (b) NOT finish retiring while the snapshot is held,
// (c) let the old epoch keep serving (process() still works), and
// (d) complete promptly once the last snapshot is released.
async fn scenario_retire_waits_for_epoch_holders() {
    let registry = shared_registry(load_test_plugins(&[drainable_config()], 100))
        .expect("failed to build drainable registry");

    // The in-flight dispatch's snapshot.
    let snapshot = registry.load_full();

    let mut retire = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { swap_and_retire(&registry, empty_registry(), Duration::from_secs(5)).await }
    });

    // (a) New frames see the new (empty) registry as soon as the
    // swap happened — poll until it is visible.
    wait_until(
        Duration::from_secs(2),
        || registry.load().plan_for_message("check-request").is_none(),
        "swap to become visible to new frames",
    )
    .await;

    // (b) While the snapshot is held, retire must not complete.
    // Safe direction: in the fixed code the quiesce loop cannot
    // exit while we hold a strong ref, so this timeout always
    // elapses; in the pre-fix ordering (no quiesce) retire
    // finishes within microseconds and this assertion trips.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut retire)
            .await
            .is_err(),
        "swap_and_retire completed while an in-flight dispatch still held the \
             old registry snapshot — old plugins would be drained/shut down under it"
    );

    // (c) The old epoch still serves: the snapshot's plugin
    // processes a message successfully mid-retire.
    let entry = snapshot
        .get_plugin("drainable")
        .expect("old snapshot must still know its plugin");
    let msg = SpoeMessage {
        name: RString::from("check-request"),
        args: [("sleep_ms", SpoeValue::Int32(0))]
            .into_iter()
            .map(|(k, v)| (RString::from(k), v))
            .collect(),
        stream_id: 1,
        frame_id: 1,
    };
    let result = entry
        .instance
        .process(&msg)
        .expect("old epoch must keep serving in-flight dispatches until quiesced");
    assert!(
        !result.variables.is_empty(),
        "expected the dispatched variable from the old epoch's plugin"
    );

    // (d) Releasing the last snapshot lets retire complete.
    drop(snapshot);
    let report = tokio::time::timeout(Duration::from_secs(5), retire)
        .await
        .expect("retire must complete once the last snapshot is released")
        .expect("retire task must not panic");
    assert!(
        report.quiesced_cleanly,
        "quiesce should be clean once the snapshot was dropped, got {report:?}"
    );
    assert!(
        report.drain_results.iter().all(|(_, ok)| *ok),
        "expected clean drain, got {report:?}"
    );
}

// --- Scenario 2: end-to-end — in-flight NOTIFY around a reload. -------
//
// The production shape of race B, over a real SPOP connection:
// a NOTIFY is mid-dispatch (inside process(), before its
// fire-and-forget work is enqueued) when a reload removes the
// message from the config. The dispatch must be served by the old
// epoch and its background work must complete before the old
// plugin set is torn down — pre-fix, the drain ran early, saw
// nothing in flight, and the background job died with the old
// plugin generation (completed-file never written).
async fn scenario_inflight_notify_survives_reload() {
    let registry = shared_registry(load_test_plugins(&[drainable_config()], 100))
        .expect("failed to build drainable registry");
    let socket_path = common::unique_sock("reload-quiesce");
    let (client, server) = common::start_server_and_connect(&socket_path, registry.clone()).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let started = dir.path().join("started");
    let completed = dir.path().join("completed");

    let mut args = HashMap::new();
    // 500ms in-process dispatch window so the reload below is
    // guaranteed to land while the NOTIFY is in flight.
    args.insert("dispatch_ms".to_string(), TypedData::Int32(500));
    args.insert("sleep_ms".to_string(), TypedData::Int32(200));
    args.insert(
        "start_file".to_string(),
        TypedData::String(started.display().to_string()),
    );
    args.insert(
        "complete_file".to_string(),
        TypedData::String(completed.display().to_string()),
    );

    let client_task = tokio::spawn(async move {
        let mut client = client;
        let ack = client.send_notify("check-request", args).await;
        (client, ack)
    });

    // Observable "dispatch entered process()" marker — no
    // scheduling guesswork.
    wait_until(
        Duration::from_secs(5),
        || started.exists(),
        "the NOTIFY dispatch to enter process()",
    )
    .await;

    // Reload: the new config no longer declares "check-request".
    let report = swap_and_retire(&registry, empty_registry(), Duration::from_secs(5)).await;

    assert!(
        report.quiesced_cleanly,
        "the in-flight dispatch should quiesce within the budget, got {report:?}"
    );
    assert!(
        report.drain_results.iter().all(|(_, ok)| *ok),
        "expected clean drain, got {report:?}"
    );
    // THE race-B assertion: the fire-and-forget work of the
    // in-flight dispatch survived the reload. Pre-fix, the drain
    // ran before the work was enqueued and the job died with the
    // old plugin generation — this file never appeared.
    assert!(
        completed.exists(),
        "fire-and-forget work of the in-flight dispatch was lost across the reload \
             (race B): drain ran before the dispatch enqueued its background job"
    );

    // The in-flight NOTIFY was served by the old epoch: its ACK
    // carries the plugin's variable.
    let (client, ack) = client_task.await.expect("client task must not panic");
    let ack = ack.expect("in-flight NOTIFY must be ACKed");
    assert!(
        matches!(
            ack.find_var("drainable.dispatched"),
            Some(TypedData::Bool(true))
        ),
        "in-flight NOTIFY must be served by the old epoch, got actions: {:?}",
        ack.actions
    );

    // New frames see the new registry (message gone).
    assert!(
        registry.load().plan_for_message("check-request").is_none(),
        "post-swap frames must dispatch against the new registry"
    );

    common::cleanup(client, server, Some(&socket_path)).await;
}

// --- Scenario 3: unhandled NOTIFY is loud, not silent. ----------------
//
// A NOTIFY for a message no plugin handles (the post-swap side of
// race B: haproxy's previous config generation keeps firing a
// message the hub's new config dropped). SPOP has no message-level
// error frame, so the hub must still ACK — but the drop must be
// observable: WARN log + spoa_messages_unhandled_total{message}.
async fn scenario_unhandled_notify_is_loud(snap: &metrics_util::debugging::Snapshotter) {
    let registry = shared_registry(load_test_plugins(&[drainable_config()], 100))
        .expect("failed to build drainable registry");
    let socket_path = common::unique_sock("unhandled");
    let (mut client, server) =
        common::start_server_and_connect(&socket_path, registry.clone()).await;

    let ack = client
        .send_notify("ghost-msg", HashMap::new())
        .await
        .expect("hub must still ACK a NOTIFY for an unhandled message");
    assert!(
        ack.actions.is_empty(),
        "unhandled message must produce an empty ACK, got: {:?}",
        ack.actions
    );

    let metrics = snap.snapshot().into_vec();
    let want = CompositeKey::new(
        MetricKind::Counter,
        Key::from_parts(
            "spoa_messages_unhandled_total".to_string(),
            vec![Label::new("message", "ghost-msg")],
        ),
    );
    let value = metrics
        .iter()
        .find_map(|(k, _unit, _desc, v)| (k == &want).then_some(v))
        .unwrap_or_else(|| {
            panic!(
                "spoa_messages_unhandled_total{{message=\"ghost-msg\"}} not recorded — \
                     the unhandled-NOTIFY drop is silent again.\nsnapshot contents:\n{}",
                metrics
                    .iter()
                    .map(|(k, _, _, _)| format!("  - {k:?}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
    assert!(
        matches!(value, DebugValue::Counter(1)),
        "expected exactly one unhandled increment, got {value:?}"
    );

    // The connection stays healthy: a handled message on the same
    // connection is still served.
    let mut args = HashMap::new();
    args.insert("sleep_ms".to_string(), TypedData::Int32(0));
    let ack = client
        .send_notify("check-request", args)
        .await
        .expect("handled message on the same connection must still be served");
    assert!(
        matches!(
            ack.find_var("drainable.dispatched"),
            Some(TypedData::Bool(true))
        ),
        "handled message must still dispatch normally, got: {:?}",
        ack.actions
    );

    common::cleanup(client, server, Some(&socket_path)).await;
}
