//! Hub-side dispatcher for plugin-emitted metrics.
//!
//! When the hub loads a plugin built against plugin-API v2+, it
//! allocates a [`MetricCtx`] (carrying the plugin's name) and installs
//! the [`record_metric`] function pointer plus the ctx pointer via the
//! vtable's `set_metric_recorder` field. The plugin then invokes
//! `record_metric(ctx, ...)` for every counter / gauge / histogram
//! event it wants to publish.
//!
//! The dispatcher routes each event into the `metrics` crate's runtime
//! API, namespacing the metric name as `plugin_<plugin>_<name>` so all
//! plugin metrics appear under a consistent prefix in Prometheus
//! alongside the hub's own `spoa_*` and `spoa_hub_*` metrics.
//!
//! ## Lifetime contract
//!
//! `MetricCtx` is owned by the `Plugin` struct and lives until the
//! plugin's `destroy` returns. The plugin holds a `*const c_void` to
//! it via the `MetricRecorder` helper; that pointer is valid for the
//! plugin's full lifetime. The dispatcher derefs through that pointer
//! on every event — UB if the box is freed while a plugin call is in
//! flight, which the `Plugin` Drop ordering prevents (`destroy` runs
//! before the box drops).

use std::os::raw::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use haproxy_spoa_hub_plugin_api::{MetricKind, MetricLabel, RSlice, RStr};
use metrics::Label;

/// Per-plugin context handed to plugins via `set_metric_recorder`.
/// Holds the plugin's name prefix in pre-formatted form (e.g.
/// `"plugin_mirror_"`) so the per-event dispatcher concatenates
/// instead of running `format!` from scratch on every emission.
pub struct MetricCtx {
    /// Pre-formatted `"plugin_<plugin_name>_"` prefix. Computed once
    /// at plugin load; the hot path appends the metric short name
    /// (one heap alloc per event instead of the `format!` macro's
    /// argument-formatting overhead).
    name_prefix: String,
}

impl MetricCtx {
    /// Construct a heap-allocated context for one plugin. The
    /// `Plugin` struct retains ownership; the raw pointer handed to
    /// the plugin remains valid until the box drops, which happens
    /// after `destroy` returns.
    #[must_use]
    pub fn boxed(plugin_name: impl AsRef<str>) -> Box<Self> {
        let mut name_prefix = String::with_capacity(plugin_name.as_ref().len() + 8);
        name_prefix.push_str("plugin_");
        name_prefix.push_str(plugin_name.as_ref());
        name_prefix.push('_');
        Box::new(MetricCtx { name_prefix })
    }

    /// Erase the type for the FFI hand-off. The hub passes this raw
    /// pointer to `set_metric_recorder` as the `ctx` arg.
    #[must_use]
    pub fn as_ptr(&self) -> *const c_void {
        std::ptr::from_ref::<MetricCtx>(self).cast()
    }

    /// Borrow the `"plugin_<plugin>_"` prefix. Used by `record_metric`
    /// to compose the full metric name with a single `String::push_str`
    /// (still one heap alloc per emission for the full name, but no
    /// per-event format-argument overhead).
    #[must_use]
    pub fn name_prefix(&self) -> &str {
        &self.name_prefix
    }
}

/// FFI entry-point a plugin invokes for every metric event. Matches
/// `haproxy_spoa_hub_plugin_api::RecordMetricFn`.
///
/// # Safety contract for plugins
///
/// `ctx` must be the exact pointer passed to `set_metric_recorder`.
/// The hub guarantees it is valid for the plugin's lifetime. Passing
/// a different pointer (or a null one outside the plugin's lifetime)
/// is UB.
///
/// # Panic safety
///
/// The entire body is wrapped in `catch_unwind`. This is **load-bearing**:
/// the function is `extern "C"`, and unwinding through a `Rust → C → Rust`
/// boundary is UB. Possible panic sources include allocator OOM from the
/// `String::with_capacity` / `Vec::collect` paths and any internal panic
/// inside the `metrics` crate macros. Matching the existing
/// `define_plugin!` convention (every plugin-side thunk wraps user code
/// in `catch_unwind`), so a panic here drops the event silently and the
/// hub stays up.
pub extern "C" fn record_metric(
    ctx: *const c_void,
    name: RStr<'_>,
    labels: RSlice<'_, MetricLabel<'_>>,
    value: f64,
    kind: MetricKind,
) {
    // AssertUnwindSafe: the function is called fresh by the plugin for
    // every metric event, holds no mutable state across calls, and the
    // metrics crate's macros (`counter!` / `gauge!` / `histogram!`) are
    // designed to be invoked from arbitrary thread contexts. Nothing
    // here is broken by an in-progress panic.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() {
            // Defensive: a buggy plugin shouldn't be able to crash the
            // hub by passing null. Silently drop the event.
            return;
        }
        // SAFETY: caller (plugin) honored the ctx contract — same
        // pointer we provided at install, still alive (plugin hasn't
        // received destroy yet).
        let ctx = unsafe { &*ctx.cast::<MetricCtx>() };

        // Prefix the metric name so all plugin metrics appear under a
        // consistent namespace. The prefix is pre-formatted at plugin
        // load (see MetricCtx::boxed) so the hot path is a single
        // String::with_capacity + two push_str calls — cheaper than
        // format!'s argument-formatting machinery for the same output.
        // Empty / oversize names fall through to the registry which
        // already filters bad names; we don't second-line validate
        // here.
        let prefix = ctx.name_prefix();
        let short = name.as_str();
        let mut metric_name = String::with_capacity(prefix.len() + short.len());
        metric_name.push_str(prefix);
        metric_name.push_str(short);

        // Translate the FFI label slice into the `metrics` crate's
        // owned Label vec. The macros take a Vec/IntoIterator of Label
        // and construct registry keys internally. Per-call heap alloc
        // here is intentional: SmallVec<[Label; 4]> would avoid it for
        // ≤4 labels but adds a dependency the rest of the crate
        // doesn't pull in. The `metrics` crate's internal interning
        // amortises most of the cost; revisit if a profile shows it
        // dominates.
        let metric_labels: Vec<Label> = labels
            .iter()
            .map(|(k, v)| Label::new(k.as_str().to_owned(), v.as_str().to_owned()))
            .collect();

        match kind {
            MetricKind::Counter => {
                // Counters in the `metrics` crate take u64 increments.
                // Truncate fractional values rather than refuse them;
                // the plugin-API doc spells out this rounding.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let inc = if value < 0.0 { 0 } else { value as u64 };
                metrics::counter!(metric_name, metric_labels).increment(inc);
            }
            MetricKind::Gauge => {
                metrics::gauge!(metric_name, metric_labels).set(value);
            }
            MetricKind::Histogram => {
                metrics::histogram!(metric_name, metric_labels).record(value);
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi_stable::std_types::RVec;

    #[test]
    fn ctx_ptr_roundtrips_to_plugin_name() {
        let ctx = MetricCtx::boxed("test_plugin");
        let ptr = ctx.as_ptr();
        // SAFETY: ctx is still alive (held in local), ptr points into
        // its allocation.
        let recovered = unsafe { &*ptr.cast::<MetricCtx>() };
        assert_eq!(recovered.name_prefix(), "plugin_test_plugin_");
    }

    #[test]
    fn record_metric_with_null_ctx_is_silent_noop() {
        // Should not panic, should not unwind through extern "C".
        let labels: RVec<MetricLabel<'_>> = RVec::new();
        record_metric(
            std::ptr::null(),
            RStr::from("any"),
            labels.as_rslice(),
            1.0,
            MetricKind::Counter,
        );
    }

    #[test]
    fn record_metric_dispatches_each_kind_without_panic() {
        // We can't easily assert on the metrics registry from a unit
        // test (the global recorder is process-wide and might not be
        // installed in the test env). The minimum bar: each kind
        // dispatches without panicking. End-to-end emission is
        // covered by hub integration tests under `tests/`.
        let ctx = MetricCtx::boxed("test_plugin");
        let ptr = ctx.as_ptr();
        let pairs = vec![
            (RStr::from("key1"), RStr::from("value1")),
            (RStr::from("key2"), RStr::from("value2")),
        ];
        let labels: RVec<MetricLabel<'_>> = RVec::from(pairs);

        record_metric(
            ptr,
            RStr::from("counter_x"),
            labels.as_rslice(),
            3.0,
            MetricKind::Counter,
        );
        record_metric(
            ptr,
            RStr::from("gauge_x"),
            labels.as_rslice(),
            4.0,
            MetricKind::Gauge,
        );
        record_metric(
            ptr,
            RStr::from("hist_x"),
            labels.as_rslice(),
            0.012,
            MetricKind::Histogram,
        );
    }

    #[test]
    fn counter_with_negative_value_increments_by_zero() {
        // A buggy plugin passing a negative counter increment must
        // not panic and must not underflow the registry.
        let ctx = MetricCtx::boxed("test_plugin");
        let labels: RVec<MetricLabel<'_>> = RVec::new();
        record_metric(
            ctx.as_ptr(),
            RStr::from("c_neg"),
            labels.as_rslice(),
            -1.0,
            MetricKind::Counter,
        );
    }

    /// Verifies the panic safety net by installing a metrics recorder
    /// whose dispatch panics, then calling `record_metric`. Without
    /// the `catch_unwind` wrapper, the panic crosses extern "C" → UB
    /// (typically abort). With the wrapper, the test returns normally.
    ///
    /// `metrics::set_global_recorder` is one-shot per process. We use
    /// `with_local_recorder` instead so the panicking recorder is
    /// scoped to this test only and doesn't interfere with the e2e
    /// integration test in `tests/plugin_metrics_e2e.rs`.
    #[test]
    fn record_metric_absorbs_panics_from_inner_dispatch() {
        use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString};

        struct PanickingRecorder;
        impl Recorder for PanickingRecorder {
            fn describe_counter(&self, _: KeyName, _: Option<metrics::Unit>, _: SharedString) {}
            fn describe_gauge(&self, _: KeyName, _: Option<metrics::Unit>, _: SharedString) {}
            fn describe_histogram(&self, _: KeyName, _: Option<metrics::Unit>, _: SharedString) {}
            fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
                panic!("intentional panic from PanickingRecorder")
            }
            fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
                panic!("intentional panic from PanickingRecorder")
            }
            fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
                panic!("intentional panic from PanickingRecorder")
            }
        }

        let ctx = MetricCtx::boxed("test_plugin");
        let labels: RVec<MetricLabel<'_>> = RVec::new();
        // Scoped recorder: only this closure sees PanickingRecorder;
        // other tests in the same process keep whatever recorder
        // (or no recorder) they had.
        metrics::with_local_recorder(&PanickingRecorder, || {
            record_metric(
                ctx.as_ptr(),
                RStr::from("any"),
                labels.as_rslice(),
                1.0,
                MetricKind::Counter,
            );
            record_metric(
                ctx.as_ptr(),
                RStr::from("any"),
                labels.as_rslice(),
                1.0,
                MetricKind::Gauge,
            );
            record_metric(
                ctx.as_ptr(),
                RStr::from("any"),
                labels.as_rslice(),
                1.0,
                MetricKind::Histogram,
            );
        });
        // Reaching this assert proves catch_unwind absorbed the
        // three panics (one per kind). Without the wrapper the
        // function would have aborted the test binary before we got
        // here.
    }
}
