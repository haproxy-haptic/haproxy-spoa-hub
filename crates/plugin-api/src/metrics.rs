//! FFI-safe metric types and the recorder helper plugins use to emit
//! Prometheus metrics through the hub.
//!
//! # Design
//!
//! Plugins don't talk to a Prometheus exporter directly — the hub owns
//! the `/metrics` endpoint and the `metrics` crate registry. Instead the
//! hub hands each plugin a `RecordMetricFn` callback at init time, and
//! the plugin invokes it whenever it wants to emit a counter increment,
//! gauge set, or histogram observation. The callback routes into the
//! hub's existing `metrics::{counter, gauge, histogram}` macros,
//! prefixing the metric name with `plugin_<plugin>_` so plugin metrics
//! are namespaced (matches the hub's own `spoa_*` and `spoa_hub_*`
//! prefix discipline).
//!
//! ## Why push, not pull
//!
//! A pull-based design (hub asks each plugin "what are your current
//! metric values?" on every Prometheus scrape) would require plugins to
//! re-aggregate histograms into Prometheus bucket counts on each scrape
//! and serialise them across the FFI boundary every time. The push
//! design lets each event go straight into the hub's existing recorder
//! at the moment it happens — same code path the hub's own metrics use,
//! with native histogram observation support and no per-scrape FFI
//! traffic.
//!
//! ## Lifetime contract
//!
//! The recorder callback and its `ctx` pointer remain valid from the
//! moment the hub calls `set_metric_recorder` on a plugin until that
//! plugin's `destroy` returns. Plugins MUST NOT call the recorder
//! after returning from `destroy`. The hub MUST NOT free the ctx until
//! it has called `destroy` on every plugin that received it.

use std::os::raw::c_void;
use std::sync::RwLock;

use abi_stable::std_types::{RSlice, RStr};

/// Kind of metric being recorded. The hub dispatches each call into the
/// matching `metrics` crate primitive (`counter!` / `gauge!` /
/// `histogram!`). `#[repr(u8)]` pins the discriminant so plugins built
/// against any version of this crate agree on the wire encoding.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonically-increasing counter. `value` is the increment
    /// (commonly `1.0`, but plugins recording batch counts may pass
    /// larger values). The hub casts via `value as u64` — fractional
    /// values are truncated.
    Counter = 0,
    /// Point-in-time gauge. `value` replaces the previous value for
    /// the matching `(name, labels)` tuple.
    Gauge = 1,
    /// Histogram observation. `value` is the observed quantity (e.g.
    /// a latency in seconds). The hub's exporter is configured with
    /// default bucket boundaries; operators that need custom buckets
    /// for a specific plugin metric can declare them via the hub's
    /// `PrometheusBuilder::set_buckets_for_metric` config (see the
    /// hub's `init_metrics` for the existing `spoa_message_duration`
    /// bucket override).
    Histogram = 2,
}

/// One label pair `(key, value)` on a metric sample. Borrowed from the
/// caller; the recorder copies into the hub's registry before returning.
pub type MetricLabel<'a> = (RStr<'a>, RStr<'a>);

/// FFI signature the hub provides to each plugin via
/// [`PluginVTable::set_metric_recorder`].
///
/// `ctx` is an opaque hub-owned pointer; the plugin MUST pass it back
/// verbatim on every record call. The hub uses it to look up the
/// per-plugin metric namespace.
///
/// `name` is the metric short name (without the `plugin_<plugin>_`
/// prefix the hub adds). Stable identifiers only — names must not
/// embed dynamic data; put that in labels.
///
/// [`PluginVTable::set_metric_recorder`]: crate::PluginVTable::set_metric_recorder
pub type RecordMetricFn = extern "C" fn(
    ctx: *const c_void,
    name: RStr<'_>,
    labels: RSlice<'_, MetricLabel<'_>>,
    value: f64,
    kind: MetricKind,
);

/// Ergonomic plugin-side helper that wraps the FFI recorder.
///
/// Plugins embed a `MetricRecorder` in their state, the
/// `define_plugin!` macro wires it via the `set_metric_recorder`
/// vtable entry, and plugin code calls the typed `counter` / `gauge` /
/// `histogram` methods from anywhere with `&self`.
///
/// ```rust,ignore
/// pub struct MyPlugin {
///     metrics: MetricRecorder,
///     // ...other state
/// }
///
/// impl MyPlugin {
///     fn process(&self, msg: &SpoeMessage) -> Result<ProcessingResult, _> {
///         self.metrics.counter("dispatches_total", &[("kind", "ok")], 1);
///         self.metrics.histogram("latency_seconds", &[], 0.012);
///         // ...
///     }
/// }
/// ```
///
/// When the recorder is not yet installed (between `create` and the
/// hub's `set_metric_recorder` call) or the host is running a v1 hub
/// that never installs one, the methods are inexpensive no-ops — a
/// single uncontended read-lock acquire that observes `None`.
///
/// # Why `RwLock<Option<…>>` and not `OnceLock<…>`
///
/// Plugins are typically declared as `pub(crate) static METRICS:
/// MetricRecorder = MetricRecorder::new();` — a process-wide singleton
/// living in the `.so`'s BSS. The hub keeps the `.so` mapped across
/// plugin reloads via `libloading::Library` refcounting, so the static
/// **survives** every reload and accumulates state across plugin
/// instances. With an internal `OnceLock`, `install()` was silently
/// no-op on the second-and-later plugin loads: the NEW plugin's hub
/// `ctx` pointer never replaced the OLD one. When the hub then dropped
/// the OLD plugin's `Box<MetricCtx>` (after the OLD plugin's last
/// in-flight `process()` call retired), the stale OLD pointer in
/// `MetricRecorder` became a use-after-free; the next `counter()` /
/// `gauge()` / `histogram()` from any plugin instance — old or new —
/// dereferenced freed memory and segfaulted the hub process. The
/// haptic conformance suite reproduces this on every conformance shard
/// run because its `reconciliationDebounceInterval=10ms` triggers
/// rapid back-to-back `HAProxy` general-storage writes, each of which
/// the hub's file-watcher turns into a plugin reload.
///
/// `RwLock` lets `install()` actually REPLACE the inner on every call,
/// which is what plugin reload requires. Read paths (`emit`) take the
/// read lock for the duration of the recorder callback, holding the
/// pointer valid against a racing `install()` from a concurrent
/// reload. The hub's lifetime contract — "ctx is valid until the
/// plugin instance that received it is destroyed" — combined with the
/// hub's drop ordering (`Plugin::drop` calls `destroy()` BEFORE
/// dropping `Box<MetricCtx>`) means a NEW install always lands BEFORE
/// the corresponding OLD `Box` is freed, so once `emit` reads NEW
/// inner under its read lock, the ctx it observes is the NEW one and
/// stays valid for the call.
pub struct MetricRecorder {
    inner: RwLock<Option<RecorderInner>>,
}

struct RecorderInner {
    record_fn: RecordMetricFn,
    ctx: *const c_void,
}

// SAFETY: `RecorderInner` holds a `*const c_void` pointing into hub-
// owned memory. The hub guarantees that pointer remains valid (and
// addressable from any thread) for the entire lifetime of the plugin
// instance, and that the recorder function itself is reentrant /
// thread-safe (it routes into the `metrics` crate, which is). The
// plugin treats both fields as read-only after installation. With
// those guarantees, sending and sharing the inner across threads is
// safe.
unsafe impl Send for RecorderInner {}
unsafe impl Sync for RecorderInner {}

impl MetricRecorder {
    /// Construct an uninitialised recorder. Plugins embed this in their
    /// state; the `define_plugin!` macro fills it in when the hub calls
    /// `set_metric_recorder`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// Install the hub-provided recorder. REPLACES any previously
    /// installed recorder — this is intentional and load-bearing: when
    /// the hub reloads plugins, the NEW plugin's `set_metric_recorder`
    /// thunk calls this with a fresh `ctx` pointer, and the OLD `ctx`
    /// (which the hub is about to free) must be evicted from this
    /// recorder. See the struct-level docs for the use-after-free that
    /// the previous OnceLock-based no-op-on-second-set behaviour
    /// produced. The `define_plugin!` macro's `set_metric_recorder`
    /// thunk calls this.
    ///
    /// On lock poisoning we fall back to recovering the inner via
    /// `into_inner()` — install is rare and a poisoned lock would
    /// otherwise leak a dangling pointer; the hub's panic-catching
    /// thunks make poisoning unlikely in the first place.
    #[doc(hidden)]
    pub fn install(&self, record_fn: RecordMetricFn, ctx: *const c_void) {
        let new = RecorderInner { record_fn, ctx };
        match self.inner.write() {
            Ok(mut guard) => *guard = Some(new),
            Err(poisoned) => *poisoned.into_inner() = Some(new),
        }
    }

    /// Increment a counter. `increment` is typically `1`. Names are
    /// hub-namespaced — emitting `"dispatches_total"` from the mirror
    /// plugin lands as `plugin_mirror_dispatches_total` in Prometheus.
    pub fn counter(&self, name: &str, labels: &[(&str, &str)], increment: u64) {
        #[allow(clippy::cast_precision_loss)]
        self.emit(name, labels, increment as f64, MetricKind::Counter);
    }

    /// Set a gauge. Replaces the previous value for this `(name, labels)`
    /// tuple in the registry.
    pub fn gauge(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.emit(name, labels, value, MetricKind::Gauge);
    }

    /// Record a histogram observation. Bucket boundaries are governed
    /// by the hub's Prometheus exporter config; plugins that need
    /// custom buckets coordinate with the hub at deploy time.
    pub fn histogram(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        self.emit(name, labels, value, MetricKind::Histogram);
    }

    fn emit(&self, name: &str, labels: &[(&str, &str)], value: f64, kind: MetricKind) {
        // Read-lock guard kept alive across the callback invocation:
        // a racing reload's `install()` blocks on the write lock until
        // we release, so the `ctx` we hand to `record_fn` cannot be
        // freed mid-call by the hub dropping its `Box<MetricCtx>`
        // (which only happens AFTER the OLD plugin's `process` calls
        // have all retired — see `Plugin::drop` in the hub).
        let guard = match self.inner.read() {
            Ok(g) => g,
            // Poisoned read recovers by reading the inner anyway; we
            // can't propagate the panic from a method that's called
            // from arbitrary metric-emission sites in plugin code.
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(ref inner) = *guard else {
            // No recorder installed — running against a v1 hub, or
            // emit fired before install (between create and
            // set_metric_recorder). Silently drop; no panic, no log
            // spam in the hot path.
            return;
        };
        // Translate the borrowed Rust slices into the FFI shape. The
        // RStr / RSlice constructors are zero-copy borrows.
        // Allocating a small Vec for labels is unavoidable since the
        // hub callback wants RSlice<MetricLabel> — but the hot path
        // (the recorder callback itself) avoids any extra copies.
        let ffi_labels: Vec<MetricLabel<'_>> = labels
            .iter()
            .map(|(k, v)| (RStr::from(*k), RStr::from(*v)))
            .collect();
        (inner.record_fn)(
            inner.ctx,
            RStr::from(name),
            RSlice::from(ffi_labels.as_slice()),
            value,
            kind,
        );
    }
}

impl Default for MetricRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MetricRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `try_read` so a Debug print never deadlocks on a writer
        // (install is rare; treat the contended case as "unknown").
        let installed = match self.inner.try_read() {
            Ok(g) => Some(g.is_some()),
            Err(_) => None,
        };
        f.debug_struct("MetricRecorder")
            .field("installed", &installed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    static EMITTED_COUNT: AtomicU64 = AtomicU64::new(0);

    extern "C" fn test_recorder(
        _ctx: *const c_void,
        _name: RStr<'_>,
        _labels: RSlice<'_, MetricLabel<'_>>,
        _value: f64,
        _kind: MetricKind,
    ) {
        EMITTED_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn emit_without_install_is_silent_noop() {
        let r = MetricRecorder::new();
        // Should not panic, should not invoke any recorder.
        r.counter("test", &[], 1);
        r.gauge("test", &[], 1.0);
        r.histogram("test", &[], 1.0);
        // (No external observer here — the actual test is "no panic".)
    }

    #[test]
    fn install_then_emit_routes_to_recorder() {
        let before = EMITTED_COUNT.load(Ordering::Relaxed);
        let r = MetricRecorder::new();
        r.install(test_recorder, ptr::null());
        r.counter("dispatches_total", &[("kind", "ok")], 1);
        r.gauge("in_flight", &[], 7.0);
        r.histogram("latency_seconds", &[("path", "/x")], 0.012);
        assert_eq!(EMITTED_COUNT.load(Ordering::Relaxed) - before, 3);
    }

    #[test]
    fn install_replaces_previous_recorder() {
        // Was previously asserting OnceLock-style "second install is
        // no-op". That contract caused a use-after-free on plugin
        // reload: the hub creates a fresh `Box<MetricCtx>` for the
        // NEW plugin, but the static `MetricRecorder` in the plugin
        // .so kept the OLD ctx pointer; when the hub later dropped
        // the OLD Box (after the OLD plugin's last in-flight call
        // retired), the NEXT metric emission dereferenced freed
        // memory. The contract is now: install REPLACES.
        static EMITTED_VIA_CTX: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

        extern "C" fn recorder_route_by_ctx(
            ctx: *const c_void,
            _name: RStr<'_>,
            _labels: RSlice<'_, MetricLabel<'_>>,
            _value: f64,
            _kind: MetricKind,
        ) {
            let idx = ctx as usize;
            if idx < EMITTED_VIA_CTX.len() {
                EMITTED_VIA_CTX[idx].fetch_add(1, Ordering::Relaxed);
            }
        }

        let r = MetricRecorder::new();
        r.install(recorder_route_by_ctx, std::ptr::null::<c_void>());
        r.counter("c1", &[], 1);
        assert_eq!(EMITTED_VIA_CTX[0].load(Ordering::Relaxed), 1);
        assert_eq!(EMITTED_VIA_CTX[1].load(Ordering::Relaxed), 0);

        // Second install MUST replace — this is the reload case.
        r.install(recorder_route_by_ctx, std::ptr::dangling::<c_void>());
        r.counter("c2", &[], 1);
        // The NEW ctx received the new emission, NOT the old ctx.
        assert_eq!(EMITTED_VIA_CTX[0].load(Ordering::Relaxed), 1);
        assert_eq!(EMITTED_VIA_CTX[1].load(Ordering::Relaxed), 1);
    }

    /// Concurrent emit while a racing install replaces the recorder.
    /// Models the hub's "NEW plugin's `set_metric_recorder` thunk runs
    /// while OLD plugin's `process()` emissions are in flight" race.
    /// The read-lock in `emit()` must hold the OLD inner valid for
    /// the duration of the in-flight callback; the install must not
    /// be observed as a torn write.
    #[test]
    fn install_during_concurrent_emit_does_not_tear() {
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        static OK_CALLS: AtomicU64 = AtomicU64::new(0);

        extern "C" fn ok_recorder(
            _ctx: *const c_void,
            _name: RStr<'_>,
            _labels: RSlice<'_, MetricLabel<'_>>,
            _value: f64,
            _kind: MetricKind,
        ) {
            OK_CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let r = Arc::new(MetricRecorder::new());
        r.install(ok_recorder, ptr::null());

        let emitter = {
            let r = Arc::clone(&r);
            thread::spawn(move || {
                for _ in 0..10_000 {
                    r.counter("dispatches_total", &[], 1);
                }
            })
        };
        let installer = {
            let r = Arc::clone(&r);
            thread::spawn(move || {
                for i in 0..1_000 {
                    r.install(ok_recorder, i as *const c_void);
                    thread::sleep(Duration::from_micros(10));
                }
            })
        };

        emitter.join().unwrap();
        installer.join().unwrap();
        // All 10_000 emissions must have landed (no panic, no torn
        // write that the read side rejected). The exact count is
        // observable via OK_CALLS, but the load-bearing assertion is
        // "no segfault / no panic during the race".
        assert_eq!(OK_CALLS.load(Ordering::Relaxed), 10_000);
    }
}
