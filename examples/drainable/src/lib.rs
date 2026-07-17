//! Test plugin with fire-and-forget background work.
//!
//! Models the shape of real plugins that need drain semantics (mirror's
//! `runtime.spawn`'d HTTP dispatch, otel's batched exporter queue):
//! `process()` returns the SPOE ACK immediately, but spawns a thread
//! that does additional work after the ACK lands. If the hub tears
//! down this plugin while the background thread is still running, the
//! work is lost — which is the regression `apply_reload`'s drain phase
//! exists to prevent.
//!
//! This plugin's `drain()` spin-waits on its in-flight counter
//! reaching zero with the supplied timeout. The hub integration test
//! (`crates/hub/tests/test_drain.rs`) exercises both the clean-drain
//! path (long timeout, all threads complete, drain returns `true`)
//! and the forced-shutdown path (short timeout, drain returns `false`,
//! the hub's `apply_reload` increments
//! `spoa_hub_config_reload_drain_forced_total{plugin=drainable}`).
//!
//! To also model the *loss* mode of the real plugins — mirror's
//! background HTTP dispatch dies when the plugin instance (and with it
//! the state-owned tokio runtime) is dropped — the plugin cancels its
//! pending background work in `Drop`: a job that wakes up after the
//! plugin instance was dropped skips its completion side effect. The
//! reload-quiesce integration test
//! (`crates/hub/tests/test_reload_quiesce.rs`) uses this to prove the
//! swap → quiesce → drain → shutdown ordering end-to-end via message
//! args:
//!
//! - `dispatch_ms` (int, default 0): sleep *inside* `process()` before
//!   the background work is enqueued — widens the SPOE dispatch window
//!   so a test can deterministically interleave a reload with an
//!   in-flight dispatch.
//! - `sleep_ms` (int, default 100): how long the background job runs
//!   after `process()` returned.
//! - `start_file` (string, optional): written at `process()` entry so
//!   a test can observe "dispatch is in flight" without guessing at
//!   scheduling.
//! - `complete_file` (string, optional): written by the background job
//!   on completion — unless the plugin was dropped first, in which
//!   case the job's side effect is lost (the mirror-model loss).
//!
//! This fixture intentionally keeps process-global atomics because its tests
//! observe them from outside the plugin instance. Production plugins should
//! keep generation-owned state on their struct; `define_plugin!` supports
//! normal `self` access.

#![allow(non_camel_case_types, non_local_definitions)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use haproxy_spoa_hub_plugin_api::{ProcessingResult, SpoeValue, TxnVariable, define_plugin};

/// Background-thread counter. Incremented before each spawn,
/// decremented when the thread's sleep returns. Drain spin-waits on
/// this hitting zero. Process-scoped (since the .so's statics are
/// shared across all loads) — tests using this plugin should ensure
/// no other test is mid-dispatch when they assert on drain semantics.
pub static IN_FLIGHT: AtomicI64 = AtomicI64::new(0);

/// Set when the plugin instance is dropped. Background jobs check it
/// before performing their completion side effect, modelling a
/// fire-and-forget dispatcher whose executor dies with the plugin
/// (the mirror plugin's state-owned tokio runtime aborts its spawned
/// tasks when the runtime is dropped). Reset in `init()` so a fresh
/// generation starts alive.
static CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct DrainablePlugin;

impl Drop for DrainablePlugin {
    fn drop(&mut self) {
        // Model the mirror plugin's teardown: dropping the plugin
        // state kills its executor, and with it every background job
        // that hasn't completed yet. Jobs observe this via CANCELLED
        // and skip their completion side effect.
        CANCELLED.store(true, Ordering::SeqCst);
    }
}

define_plugin!(DrainablePlugin, {
    fn new() -> Self {
        DrainablePlugin
    }

    fn init(
        &mut self,
        _context: &PluginContext,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // No init guard — the .so's statics are process-scoped and
        // we want the plugin to load cleanly even on the second-and-
        // later loads in the same test process. The counter resets
        // naturally between tests because each test sequences its
        // dispatches and drains before asserting. The cancellation
        // flag is reset here so a new generation starts alive even if
        // a previous generation's Drop poisoned the shared static.
        CANCELLED.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn name(&self) -> &str {
        "drainable"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn process(
        &self,
        message: &SpoeMessage,
    ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
        // sleep_ms is configurable via the message's args so individual
        // tests can dial the background-work duration up or down.
        let sleep_ms = message
            .get_int("sleep_ms")
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(100);

        // Observable "dispatch entered process()" marker, written
        // BEFORE the dispatch delay so tests can wait for the file
        // instead of guessing at scheduling.
        if let Some(path) = message.get_string("start_file") {
            std::fs::write(path, b"started")?;
        }

        // In-process dispatch latency: the window between the hub
        // reading the NOTIFY frame and the fire-and-forget work being
        // enqueued. Real plugins spend this on argument parsing and
        // request building; making it configurable lets tests
        // interleave a reload deterministically.
        let dispatch_ms = message
            .get_int("dispatch_ms")
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0);
        if dispatch_ms > 0 {
            thread::sleep(Duration::from_millis(dispatch_ms));
        }

        let complete_file = message.get_string("complete_file").map(ToOwned::to_owned);

        // Bump the counter BEFORE spawning so a same-instant drain
        // call observes the work. Decrement happens at the end of the
        // spawned closure — never before the sleep returns.
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(sleep_ms));
            // Completion side effect is lost if the plugin was dropped
            // while the job was still running — the mirror-model loss.
            if !CANCELLED.load(Ordering::SeqCst) {
                if let Some(path) = complete_file {
                    let _ = std::fs::write(path, b"completed");
                }
            }
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        });

        Ok(ProcessingResult::single(TxnVariable::session(
            "dispatched",
            SpoeValue::Bool(true),
        )))
    }

    fn drain(&self, timeout_ms: u64) -> bool {
        // Spin-wait with a 5ms poll interval. 5ms is short enough that
        // a 100ms drain budget gives ~20 polls (plenty of resolution to
        // observe a 100ms in-flight sleep finishing) and long enough
        // that the loop doesn't pin a core on a quiescent registry.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if IN_FLIGHT.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                // Timeout — but check one more time in case work
                // completed concurrently with the deadline check.
                return IN_FLIGHT.load(Ordering::SeqCst) == 0;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
});
