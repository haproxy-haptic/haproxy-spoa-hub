//! End-to-end test for the plugin-API v2 metrics path.
//!
//! Closes the testing gap that the unit tests left open: each unit
//! test verifies a single seam (the recorder helper, the dispatcher
//! function, the load path) but none verifies that
//!
//!   plugin emits → recorder fn → metrics crate registry
//!
//! actually produces samples with the right name prefix, label set,
//! and *primitive* (histogram vs counter vs gauge) in the registry.
//! A typo routing Histogram into the counter macro would compile and
//! pass every other test in the suite, only showing up when a real
//! plugin's Prometheus output looked wrong.
//!
//! Approach: install a `metrics-util` `DebuggingRecorder`, load the
//! metrics-noop example plugin, drive a known number of `process()`
//! calls (each emits one counter / one gauge / one histogram), then
//! snapshot the recorder. Assert the expected name prefix, labels,
//! cumulative counter value, latest gauge, and observation count for
//! each.
//!
//! The `DebuggingRecorder` is process-global and cumulative; we keep
//! the whole exercise inside a single `#[test]` to avoid race-y
//! cross-test interleaving (parallel cargo tests would otherwise mix
//! each other's emissions into the same snapshot).

use abi_stable::std_types::RHashMap;
use haproxy_spoa_hub::plugin::Plugin;
use haproxy_spoa_hub::test_support::workspace_target_dir;
use haproxy_spoa_hub_plugin_api::{PLUGIN_API_VERSION_V2, PluginContext, RString, SpoeMessage};
use metrics::{Key, Label};
use metrics_util::CompositeKey;
use metrics_util::MetricKind;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

fn make_empty_message() -> SpoeMessage {
    SpoeMessage {
        name: RString::from("test"),
        args: RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    }
}

/// Find one metric in the snapshot by exact name + label + kind.
/// Returns the recorded `&DebugValue` for assertion; panics with the
/// full snapshot listing when not found, so a routing-bug regression
/// surfaces the actual recorded shape next to the expected one.
fn find_metric<'a>(
    metrics: &'a [(
        CompositeKey,
        Option<metrics::Unit>,
        Option<metrics::SharedString>,
        DebugValue,
    )],
    name: &str,
    labels: &[(&str, &str)],
    kind: MetricKind,
) -> &'a DebugValue {
    let label_vec: Vec<Label> = labels
        .iter()
        .map(|(k, v)| Label::new(k.to_string(), v.to_string()))
        .collect();
    let want = CompositeKey::new(kind, Key::from_parts(name.to_string(), label_vec));
    metrics
        .iter()
        .find_map(|(k, _unit, _desc, v)| if k == &want { Some(v) } else { None })
        .unwrap_or_else(|| {
            panic!(
                "expected metric not found in snapshot:\n  kind={:?}\n  name={}\n  labels={:?}\n\n\
                 snapshot contents:\n{}",
                kind,
                name,
                labels,
                metrics
                    .iter()
                    .map(|(k, _, _, _)| format!("  - {k:?}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
}

/// Number of `process()` calls. Counter must accumulate to exactly
/// this value, histogram records exactly this many observations.
const DISPATCHES: u64 = 5;

#[test]
fn plugin_v2_metrics_emit_with_namespace_labels_and_correct_kind() {
    // Install the DebuggingRecorder process-globally. Only one test
    // in this file emits metrics, so cumulative state is fully under
    // our control and exact values are deterministic.
    let recorder = DebuggingRecorder::new();
    let snap = recorder.snapshotter();
    recorder
        .install()
        .expect("install DebuggingRecorder (no other recorder may be set in this test binary)");

    let so_path = workspace_target_dir().join("libmetrics_noop_plugin.so");
    assert!(
        so_path.exists(),
        "metrics-noop example plugin must be built — run `cargo test --workspace`"
    );

    // SAFETY: we trust our own build output.
    let mut plugin = unsafe { Plugin::load(&so_path) }.expect("load metrics-noop");
    assert_eq!(
        plugin.api_version(),
        PLUGIN_API_VERSION_V2,
        "metrics-noop is built against the current plugin-api so it must report v2"
    );

    plugin
        .init(&PluginContext {
            name: RString::from("metrics-noop"),
            config: RHashMap::new(),
        })
        .expect("init metrics-noop");

    // Drive a known number of process() calls. See DISPATCHES above
    // for the cumulative-value contract.
    let msg = make_empty_message();
    for _ in 0..DISPATCHES {
        plugin.process(&msg).expect("process metrics-noop");
    }

    let metrics = snap.snapshot().into_vec();

    // (1) Counter — namespaced, label-carrying, accumulated exactly.
    //     A routing-bug regression that sent Counter into gauge!/
    //     histogram! would fail at find_metric because the wrong
    //     primitive would be in the registry under this key.
    let counter = find_metric(
        &metrics,
        "plugin_metrics-noop_dispatches_total",
        &[("kind", "ok")],
        MetricKind::Counter,
    );
    match counter {
        DebugValue::Counter(v) => assert_eq!(
            *v, DISPATCHES,
            "counter should have accumulated to {DISPATCHES} after {DISPATCHES} dispatches"
        ),
        other => panic!("expected Counter primitive, got {other:?}"),
    }

    // (2) Gauge — set on every dispatch to the same value; the
    //     latest set is what we observe. The kind-check is the
    //     anti-routing-bug guard: a wrong dispatcher would have
    //     stored this under MetricKind::Counter and find_metric
    //     would fail.
    let gauge = find_metric(
        &metrics,
        "plugin_metrics-noop_in_flight",
        &[("scope", "demo")],
        MetricKind::Gauge,
    );
    match gauge {
        DebugValue::Gauge(v) => assert!(
            (v.into_inner() - 7.0).abs() < f64::EPSILON,
            "gauge value should be 7.0, got {}",
            v.into_inner()
        ),
        other => panic!("expected Gauge primitive, got {other:?}"),
    }

    // (3) Histogram — exactly DISPATCHES observations recorded as a
    //     histogram (not a counter that happened to accept the f64).
    let hist = find_metric(
        &metrics,
        "plugin_metrics-noop_latency_seconds",
        &[("path", "/x")],
        MetricKind::Histogram,
    );
    match hist {
        DebugValue::Histogram(observations) => {
            assert_eq!(
                observations.len() as u64,
                DISPATCHES,
                "histogram should have {} observations recorded, got {}",
                DISPATCHES,
                observations.len()
            );
            // All observations have the same value (the plugin emits
            // a constant 0.012). Verify every entry to catch a
            // hypothetical dispatcher bug that mangled the value.
            for o in observations {
                let v = o.into_inner();
                assert!(
                    (v - 0.012).abs() < 1e-9,
                    "histogram observation should be 0.012, got {v}"
                );
            }
        }
        other => panic!("expected Histogram primitive, got {other:?}"),
    }

    // Drop the plugin to verify shutdown → destroy → metric_ctx box
    // drop → library drop sequence completes without panic. The unit
    // test already covers the noop case; this adds the v2-with-
    // metrics variant explicitly.
    drop(plugin);
}
