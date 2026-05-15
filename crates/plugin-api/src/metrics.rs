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
use std::sync::OnceLock;

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
/// single relaxed pointer load that observes `None`.
pub struct MetricRecorder {
    inner: OnceLock<RecorderInner>,
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
            inner: OnceLock::new(),
        }
    }

    /// Install the hub-provided recorder. Idempotent; subsequent calls
    /// are no-ops (the OnceLock retains the first installation). The
    /// `define_plugin!` macro's `set_metric_recorder` thunk calls this.
    #[doc(hidden)]
    pub fn install(&self, record_fn: RecordMetricFn, ctx: *const c_void) {
        let _ = self.inner.set(RecorderInner { record_fn, ctx });
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
        let Some(inner) = self.inner.get() else {
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
        f.debug_struct("MetricRecorder")
            .field("installed", &self.inner.get().is_some())
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
    fn install_is_idempotent() {
        let r = MetricRecorder::new();
        r.install(test_recorder, ptr::null());
        // Second install should be a no-op — OnceLock retains the
        // first value. The plugin's `process()` would otherwise see
        // mid-flight churn if the hub re-installs on config reload.
        r.install(test_recorder, 0xdead_beef as *const c_void);
        // No assertion needed beyond "no panic"; this would have
        // failed if the OnceLock's set() panicked on the second call.
    }
}
