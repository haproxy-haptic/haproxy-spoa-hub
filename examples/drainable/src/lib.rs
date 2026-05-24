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
//! Macro-hygiene note: the `define_plugin!` macro's body doesn't have
//! a `self` token bound to the plugin instance, so state lives in a
//! module-level `AtomicI64`. Same shape as the production mirror
//! plugin's `STATE.client.in_flight` — but kept primitive here so the
//! plugin can be loaded multiple times in one test process without an
//! init guard interfering (dlopen on the same .so returns the same
//! handle; the static is effectively process-scoped).

#![allow(non_camel_case_types, non_local_definitions)]

use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use haproxy_spoa_hub_plugin_api::{ProcessingResult, SpoeValue, TxnVariable, define_plugin};

/// Background-thread counter. Incremented before each spawn,
/// decremented when the thread's sleep returns. Drain spin-waits on
/// this hitting zero. Process-scoped (since the .so's statics are
/// shared across all loads) — tests using this plugin should ensure
/// no other test is mid-dispatch when they assert on drain semantics.
pub static IN_FLIGHT: AtomicI64 = AtomicI64::new(0);

#[derive(Debug)]
struct DrainablePlugin;

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
        // dispatches and drains before asserting.
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

        // Bump the counter BEFORE spawning so a same-instant drain
        // call observes the work. Decrement happens at the end of the
        // spawned closure — never before the sleep returns.
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(sleep_ms));
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
