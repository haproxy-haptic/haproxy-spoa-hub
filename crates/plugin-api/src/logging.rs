//! Forwarding of a plugin's `log` records to the hub.
//!
//! A plugin `.so` carries its own copy of the `log` crate, so a logger
//! the hub installs in its own process image is invisible to the plugin
//! and its records go nowhere. The hub therefore hands each plugin a
//! [`LogSinkFn`] callback (v4 vtable field `set_log_sink`, called once
//! between `create` and `init`); [`HubLogSink`] is a `log::Log`
//! implementation, installed as the plugin's global logger by the
//! `define_plugin!` macro, that forwards every enabled record through
//! that callback. Plugin authors only use the `log` macros and must not
//! install a logger: the hub's is in place before `init` runs, so a
//! `log::set_logger` there fails (and an unwrapping `env_logger::init()`
//! fails the plugin load).
//!
//! # Lifetime contract
//!
//! Same as the metric recorder: the sink callback and its `ctx` remain
//! valid from `set_log_sink` until the plugin's `destroy` returns; the
//! hub keys `ctx` by plugin name and never frees it, so a record emitted
//! from a background thread after a reload still lands.

use std::os::raw::c_void;
use std::sync::RwLock;

use abi_stable::std_types::RStr;

/// Severity of a forwarded record. Discriminants equal `log::Level`'s
/// (`Error = 1` … `Trace = 5`); `Off` only appears as a maximum level.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl From<log::Level> for LogLevel {
    fn from(level: log::Level) -> Self {
        match level {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug => LogLevel::Debug,
            log::Level::Trace => LogLevel::Trace,
        }
    }
}

impl From<LogLevel> for log::LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => log::LevelFilter::Off,
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Debug => log::LevelFilter::Debug,
            LogLevel::Trace => log::LevelFilter::Trace,
        }
    }
}

/// FFI signature the hub provides to each plugin via
/// [`PluginVTable::set_log_sink`]. `ctx` is opaque and hub-owned;
/// `target` is the record's `log` target (the plugin module path);
/// `message` is the formatted record.
///
/// [`PluginVTable::set_log_sink`]: crate::PluginVTable::set_log_sink
pub type LogSinkFn =
    extern "C" fn(ctx: *const c_void, level: LogLevel, target: RStr<'_>, message: RStr<'_>);

/// The plugin-side `log::Log` implementation that forwards to the hub.
/// One per `.so`; two plugin instances loaded from the same library
/// share it, and records are attributed to whichever instance installed
/// the sink last.
pub struct HubLogSink {
    inner: RwLock<Option<SinkInner>>,
}

struct SinkInner {
    sink_fn: LogSinkFn,
    ctx: *const c_void,
}

// SAFETY: `ctx` points into hub-owned memory that the hub keeps valid and
// addressable from any thread for the plugin's lifetime, and the sink
// function is thread-safe (it dispatches into `tracing`). Both fields are
// read-only after installation.
unsafe impl Send for SinkInner {}
unsafe impl Sync for SinkInner {}

/// The sink `define_plugin!` installs as the plugin's global logger.
pub static HUB_LOG_SINK: HubLogSink = HubLogSink::new();

impl HubLogSink {
    const fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    /// Install (or replace, on a plugin reload) the hub's sink and make
    /// this the plugin's global logger. `max_level` is the most verbose
    /// level the hub will emit, so cheaper records are skipped before
    /// they are formatted.
    #[doc(hidden)]
    pub fn install(&self, sink_fn: LogSinkFn, ctx: *const c_void, max_level: LogLevel) {
        let new = SinkInner { sink_fn, ctx };
        match self.inner.write() {
            Ok(mut guard) => *guard = Some(new),
            Err(poisoned) => *poisoned.into_inner() = Some(new),
        }
        // Err means a logger is already set — ours from an earlier plugin
        // generation of this library; it reads the replaced inner.
        let _ = log::set_logger(&HUB_LOG_SINK);
        log::set_max_level(max_level.into());
    }
}

impl log::Log for HubLogSink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Held across the callback so a racing reload's `install` cannot
        // swap the inner mid-call.
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(ref inner) = *guard else {
            return;
        };
        let message = record.args().to_string();
        (inner.sink_fn)(
            inner.ctx,
            record.level().into(),
            RStr::from(record.target()),
            RStr::from(message.as_str()),
        );
    }

    fn flush(&self) {}
}
