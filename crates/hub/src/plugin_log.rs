//! Hub-side sink for plugin `log` records.
//!
//! When the hub loads a plugin built against plugin-API v4+, it installs
//! [`record_log`] plus a [`LogCtx`] pointer via the vtable's
//! `set_log_sink` field. The plugin's `log` macros then arrive here and
//! are re-emitted as `tracing` events under the `plugin` target with
//! `plugin = <name>` and `log_target = <plugin module path>`, so they
//! land in the hub's structured output next to its own lines.
//!
//! `LogCtx` is interned by plugin name and leaked for the process
//! lifetime, for the same reason as [`crate::plugin_metrics::MetricCtx`]:
//! the plugin keeps the pointer in a `.so`-resident static that outlives
//! any one plugin generation.

use std::collections::HashMap;
use std::os::raw::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, PoisonError};

use haproxy_spoa_hub_plugin_api::{LogLevel, RStr};
use tracing::Level;

/// Per-plugin context handed to plugins via `set_log_sink`.
pub struct LogCtx {
    plugin: String,
}

impl LogCtx {
    /// Erase the type for the FFI hand-off.
    #[must_use]
    pub fn as_ptr(&self) -> *const c_void {
        std::ptr::from_ref::<LogCtx>(self).cast()
    }
}

static LOG_CTX_INTERN: Mutex<Option<HashMap<String, &'static LogCtx>>> = Mutex::new(None);

/// Return the process-lifetime [`LogCtx`] for a plugin name, creating
/// (and intentionally leaking) it on first use.
pub fn intern_log_ctx(plugin_name: &str) -> &'static LogCtx {
    let mut guard = LOG_CTX_INTERN
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&ctx) = map.get(plugin_name) {
        return ctx;
    }
    let leaked: &'static LogCtx = Box::leak(Box::new(LogCtx {
        plugin: plugin_name.to_owned(),
    }));
    map.insert(plugin_name.to_owned(), leaked);
    leaked
}

/// The most verbose level any installed `tracing` subscriber accepts,
/// handed to plugins so they skip formatting records the hub would drop.
#[must_use]
pub fn max_level() -> LogLevel {
    match tracing::level_filters::LevelFilter::current().into_level() {
        None => LogLevel::Off,
        Some(Level::ERROR) => LogLevel::Error,
        Some(Level::WARN) => LogLevel::Warn,
        Some(Level::INFO) => LogLevel::Info,
        Some(Level::DEBUG) => LogLevel::Debug,
        Some(Level::TRACE) => LogLevel::Trace,
    }
}

/// FFI entry-point a plugin invokes for every log record. Matches
/// `haproxy_spoa_hub_plugin_api::LogSinkFn`. `ctx` must be the pointer
/// passed to `set_log_sink`; null drops the record. Wrapped in
/// `catch_unwind` because it is called from `extern "C"`.
pub extern "C" fn record_log(
    ctx: *const c_void,
    level: LogLevel,
    target: RStr<'_>,
    message: RStr<'_>,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() {
            return;
        }
        // SAFETY: the plugin passes back the pointer we handed it at
        // install; it is process-lifetime (interned), so always alive.
        let ctx = unsafe { &*ctx.cast::<LogCtx>() };
        let plugin = ctx.plugin.as_str();
        let log_target = target.as_str();
        let message = message.as_str();
        match level {
            LogLevel::Off => {}
            LogLevel::Error => {
                tracing::error!(target: "plugin", plugin, log_target, "{message}");
            }
            LogLevel::Warn => {
                tracing::warn!(target: "plugin", plugin, log_target, "{message}");
            }
            LogLevel::Info => {
                tracing::info!(target: "plugin", plugin, log_target, "{message}");
            }
            LogLevel::Debug => {
                tracing::debug!(target: "plugin", plugin, log_target, "{message}");
            }
            LogLevel::Trace => {
                tracing::trace!(target: "plugin", plugin, log_target, "{message}");
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_stable_per_name() {
        let a = intern_log_ctx("intern-test-a");
        let b = intern_log_ctx("intern-test-b");
        assert!(std::ptr::eq(a, intern_log_ctx("intern-test-a")));
        assert!(!std::ptr::eq(a, b));
        assert_eq!(a.plugin, "intern-test-a");
    }

    #[test]
    fn record_log_tolerates_null_ctx() {
        record_log(
            std::ptr::null(),
            LogLevel::Info,
            RStr::from("t"),
            RStr::from("m"),
        );
    }
}
