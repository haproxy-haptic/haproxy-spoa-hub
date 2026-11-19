mod common;

use std::{collections::HashMap, sync::mpsc, time::Duration};

use haproxy_spoa_hub::test_support::plugin_config;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use spop::types::TypedData;
use tokio::sync::oneshot;

#[test]
fn expired_queued_call_is_acked_without_executing_plugin() {
    let recorder = DebuggingRecorder::new();
    let counters = recorder.snapshotter();
    recorder.install().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let config = plugin_config("probe", "libmetrics_noop_plugin.so", &["check"], 50);
        let socket = common::unique_sock("expired");
        let (mut client, server) =
            common::start_server_and_connect(&socket, common::build_registry(&[config])).await;
        let (started, ready) = oneshot::channel();
        let (release, hold) = mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = hold.recv_timeout(Duration::from_secs(5));
        });
        ready.await.unwrap();
        let first = tokio::time::timeout(
            Duration::from_secs(2),
            client.send_notify("check", HashMap::new()),
        )
        .await
        .expect("deadline must not wait for a blocking slot")
        .unwrap();
        assert!(
            matches!(first.find_var("probe.__status"), Some(TypedData::String(s)) if s == "timeout")
        );

        release.send(()).unwrap();
        blocker.await.unwrap();
        // The blocking pool is FIFO. Wait for the expired job to release its
        // permit before submitting another request with the same plugin.
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        let second = client.send_notify("check", HashMap::new()).await.unwrap();
        assert!(
            matches!(second.find_var("probe.__status"), Some(TypedData::String(s)) if s == "ok")
        );
        let dispatches =
            counters
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, value)| {
                    (key.key().name() == "plugin_metrics-noop_dispatches_total").then_some(value)
                });
        assert!(
            matches!(dispatches, Some(DebugValue::Counter(1))),
            "{dispatches:?}"
        );
        common::cleanup(client, server, Some(&socket)).await;
    });
}
