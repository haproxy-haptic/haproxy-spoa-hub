//! Integration tests for the v3 drain-on-reload flow.
//!
//! The drainable example plugin (`examples/drainable`) is loaded and
//! exercised via the SPOE NOTIFY path. Each call to `process` spawns
//! a background thread that decrements an in-flight counter after a
//! sleep; the plugin's `drain` spin-waits on that counter.
//!
//! The hub-side surface under test is `PluginRegistry::drain_all` —
//! the same function `apply_reload` calls between the registry swap
//! and `shutdown_all`. Triggering a real reload through `apply_reload`
//! would couple the test to file-watch / SIGHUP plumbing exercised
//! elsewhere; here we go straight at `drain_all` with a known
//! registry state.
//!
//! Single test, not three: the drainable plugin's IN_FLIGHT counter
//! is a module-level static (process-scoped because dlopen on the
//! same .so reuses the handle). Splitting into per-scenario tests
//! that run in parallel via `#[tokio::test]` would race on that
//! counter. Sequencing scenarios inside one test avoids the
//! `serial_test` dep and keeps the failure mode obvious — a single
//! `cargo test test_drain` run is one wall-clock-ordered transcript.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use haproxy_spoa_hub::test_support::{load_test_plugins, plugin_config};

fn drainable_config() -> haproxy_spoa_hub::config::PluginConfig {
    plugin_config(
        "drainable",
        "libdrainable_plugin.so",
        &["check-request"],
        1000,
    )
}

#[tokio::test]
async fn drain_semantics() {
    let socket_path = common::unique_sock("drain");
    let registry =
        haproxy_spoa_hub::registry::shared_registry(load_test_plugins(&[drainable_config()], 100))
            .expect("failed to build drainable registry");
    let (mut client, server) =
        common::start_server_and_connect(&socket_path, registry.clone()).await;

    // --- Scenario 1: quiescent registry drains immediately. -----------
    //
    // No NOTIFY messages sent → in-flight counter is zero → drain_all
    // returns clean for every plugin, no perceptible wait. This pins
    // the no-op-fast-path: drain_all on a registry that has nothing
    // to drain shouldn't add reload latency.
    {
        let started = Instant::now();
        let results = registry.load().drain_all(Duration::from_secs(5));
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 1, "expected one plugin in registry");
        assert_eq!(results[0].0, "drainable");
        assert!(results[0].1, "quiescent registry should drain cleanly");
        assert!(
            elapsed < Duration::from_millis(100),
            "drain_all on quiescent registry should return ~immediately, took {:?}",
            elapsed
        );
    }

    // --- Scenario 2: drain waits for in-flight work to complete. ------
    //
    // Three concurrent dispatches, each holds in_flight for 100ms.
    // drain_all called immediately after with a 500ms budget should
    // block for ~100ms (max in-flight task duration) and return
    // drained=true. The asserts on lower-bound (≥80ms) and upper-
    // bound (<500ms) elapsed time prove the wait actually happened
    // AND that drain didn't burn the whole budget.
    {
        let mut args = HashMap::new();
        args.insert("sleep_ms".to_string(), spop::types::TypedData::Int32(100));
        for _ in 0..3 {
            client
                .send_notify("check-request", args.clone())
                .await
                .expect("send_notify failed");
        }

        let started = Instant::now();
        let results = registry.load().drain_all(Duration::from_millis(500));
        let elapsed = started.elapsed();

        assert!(
            results.iter().all(|(_, drained)| *drained),
            "expected clean drain, got {:?}",
            results
        );
        assert!(
            elapsed >= Duration::from_millis(80),
            "drain should have waited for in-flight work (took {:?})",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "drain should have completed before the 500ms budget elapsed (took {:?})",
            elapsed
        );
    }

    // --- Scenario 3: drain times out on long-running work. ------------
    //
    // 500ms in-flight task vs 50ms drain budget. drain_all must
    // return after ~50ms with drained=false. The hub's apply_reload
    // increments spoa_hub_config_reload_drain_forced_total{plugin=
    // drainable} on this outcome and proceeds to shutdown — the
    // in-flight work is lost.
    {
        let mut args = HashMap::new();
        args.insert("sleep_ms".to_string(), spop::types::TypedData::Int32(500));
        client
            .send_notify("check-request", args)
            .await
            .expect("send_notify failed");

        let started = Instant::now();
        let results = registry.load().drain_all(Duration::from_millis(50));
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 1);
        assert!(
            !results[0].1,
            "expected forced shutdown (drained=false), got {:?}",
            results
        );
        assert!(
            elapsed >= Duration::from_millis(40),
            "drain should have waited at least until the timeout (took {:?})",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "drain should NOT have waited for the full in-flight duration (took {:?})",
            elapsed
        );

        // Let the 500ms background thread finish before the test
        // returns so the IN_FLIGHT counter is back to zero and the
        // process exits cleanly (no lingering JoinHandles).
        tokio::time::sleep(Duration::from_millis(550)).await;
    }

    common::cleanup(client, server, Some(&socket_path)).await;
}
