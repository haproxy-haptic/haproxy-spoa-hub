//! Hand-rolled `#[repr(C)]` plugin vtable.
//!
//! Design rationale: see ADR-001 / docs/abi-evolution.md (TBD). In short:
//! `abi_stable`'s prefix-type model rejects loads where the host's vtable
//! has more fields than the plugin's, blocking the host-grows-faster
//! direction we need. The hand-rolled C-style vtable with a leading
//! `api_version: u32` field, version-gated host access, and an
//! append-only evolution discipline gives us bidirectional ABI compat
//! without a third-party load-time check that fights us.
//!
//! # Layout invariants (load-bearing)
//!
//! 1. `api_version` MUST be the first field.
//! 2. Existing fields MUST NOT be reordered or removed in any future
//!    crate version. New fields MUST be appended after the existing ones.
//! 3. `PLUGIN_API_VERSION` increments by 1 each time a new field is
//!    appended and the corresponding accessor lands.
//! 4. Hosts MUST read `api_version` before accessing any field beyond
//!    the v1 baseline. Reading newer fields on a plugin that reports
//!    a lower `api_version` is undefined behavior — the plugin's
//!    allocation does not include those bytes.
//!
//! See `crates/hub/tests/abi_matrix.rs` for the regression test that
//! enforces invariants 1–3 by loading every published plugin version
//! against the current hub binary.

use std::os::raw::c_void;

use abi_stable::std_types::{RBoxError, ROption, RResult, RStr, RString, RVec};

use crate::metrics::RecordMetricFn;
use crate::types::{Diagnostic, PluginContext, ProcessingResult, SpoeMessage};

/// API version corresponding to the v1 baseline (initial release of the
/// hand-rolled vtable). Every field declared in `PluginVTable` is part
/// of v1 and is present on every plugin built against this crate.
pub const PLUGIN_API_VERSION_V1: u32 = 1;

/// API version v2: adds `set_metric_recorder` so plugins can emit
/// Prometheus metrics through the hub's existing recorder. Plugins
/// built against v2 keep loading on v1 hubs (the hub ignores the field
/// because it doesn't know it exists); plugins built against v1 keep
/// loading on v2 hubs (the hub gates access on `api_version >= 2` and
/// skips the install for v1 plugins).
pub const PLUGIN_API_VERSION_V2: u32 = 2;

/// API version v3: adds `drain` so the hub can wait for a plugin's
/// fire-and-forget background work (mirror's `runtime.spawn`'d HTTP
/// dispatches, otel's batched exporter queue, etc.) to quiesce before
/// `shutdown` is invoked on a reload. Plugins that complete all work
/// inside `process()` (coraza, external-auth, sso-auth, ...) get the
/// `define_plugin!` macro's default thunk which returns immediately —
/// no in-flight state, nothing to wait for. Plugins with background
/// work override `drain` to block until their state is quiescent or
/// the supplied deadline elapses.
///
/// Compat: v3 plugins keep loading on v2/v1 hubs (those hubs ignore
/// the new field); v2/v1 plugins keep loading on v3 hubs (the hub
/// gates access on `api_version >= 3` and skips the drain wait for
/// older plugins — equivalent to today's behaviour, which is the
/// right default since those plugins are all synchronous-only).
pub const PLUGIN_API_VERSION_V3: u32 = 3;

/// Latest API version this crate's `PluginVTable` exposes. Plugin
/// authors set this as the `api_version` field. When future versions
/// of this crate append a field, this constant bumps by 1.
pub const PLUGIN_API_VERSION: u32 = PLUGIN_API_VERSION_V3;

/// Symbol name the hub looks up via `dlsym` after `dlopen`.
pub const GET_PLUGIN_VTABLE_SYMBOL: &[u8] = b"get_plugin_vtable\0";

/// Type of the exported entry-point symbol.
pub type GetPluginVTableFn = extern "C" fn() -> *const PluginVTable;

/// The function table a plugin shared library exports.
///
/// `#[repr(C)]` is mandatory: it pins field offsets and padding rules
/// so plugins built against an older version of this crate keep
/// working when the hub is built against a newer one (and vice versa).
///
/// # Safety contract for hosts
///
/// Fields below the v1 baseline marker are version-gated. Hosts MUST
/// NOT access them via `&PluginVTable` directly — the borrow would
/// implicitly cover the whole struct, which on an older plugin is
/// past the end of its allocation. Use the `read_*_field` accessors
/// or read individual fields through the raw pointer (e.g.
/// `unsafe { (*ptr).api_version }`) which read only the bytes for that
/// field.
#[repr(C)]
pub struct PluginVTable {
    // ============================================================
    // v1 baseline — always present on every plugin built against
    // this crate. All fields above the marker comment are mandatory
    // and never change order.
    // ============================================================
    /// Plugin's reported API version. The hub uses this to gate
    /// access to fields beyond the v1 baseline.
    pub api_version: u32,

    /// Factory: allocate and return a new plugin state pointer.
    /// `RErr` aborts plugin loading.
    pub create: extern "C" fn() -> RResult<*mut c_void, RBoxError>,

    /// Destructor: free the plugin state pointer. Called once at hub
    /// shutdown or when a plugin fails post-create. MUST tolerate
    /// being called with a state pointer that `init()` has not yet
    /// observed.
    pub destroy: extern "C" fn(state: *mut c_void),

    /// Initialize the plugin. Called once after `create` and any
    /// schema/validate pre-checks. `RErr` aborts plugin loading.
    pub init: extern "C" fn(state: *mut c_void, ctx: &PluginContext) -> RResult<(), RBoxError>,

    /// Handle one SPOE message. Called per request. The implementation
    /// is wrapped in `catch_unwind` by the `define_plugin!` macro so a
    /// panic does not abort the hub process.
    pub process: extern "C" fn(
        state: *const c_void,
        msg: &SpoeMessage,
    ) -> RResult<ProcessingResult, RBoxError>,

    /// Plugin's display name. Borrowed for the lifetime of the plugin
    /// instance (until `destroy` is called). Plugins typically return
    /// a `&'static str` literal.
    pub name: extern "C" fn(state: *const c_void) -> RStr<'static>,

    /// Plugin's `SemVer` string. Same lifetime contract as `name`.
    pub plugin_version: extern "C" fn(state: *const c_void) -> RStr<'static>,

    /// Shutdown hook. Called once before `destroy`.
    pub shutdown: extern "C" fn(state: *const c_void),

    /// Optional JSON Schema string for config validation. `RNone`
    /// skips schema validation. The default `define_plugin!` macro
    /// emits a thunk that returns `RNone` when the plugin author has
    /// not declared one.
    pub config_schema: extern "C" fn(state: *const c_void) -> ROption<RString>,

    /// Deep config validation. Returns an empty `RVec` when the
    /// plugin's config is valid. Errors block loading; warnings are
    /// surfaced but do not block.
    pub validate: extern "C" fn(state: *const c_void, ctx: &PluginContext) -> RVec<Diagnostic>,
    // ============================================================
    // End of v1 baseline. Future additions appear below this line.
    // Each new field MUST be guarded on `api_version` in the host
    // and bumps `PLUGIN_API_VERSION` by 1.
    // ============================================================
    //
    // v2 additions — present iff api_version >= PLUGIN_API_VERSION_V2.
    /// Install the hub's metric recorder on the plugin. Called by the
    /// hub once between `create` and `init`, so the plugin can emit
    /// metrics from `init` onward. The plugin stores the `record_fn`
    /// and `ctx` (typically inside a [`MetricRecorder`] field of its
    /// state) and invokes `record_fn(ctx, ...)` for every metric
    /// event. The recorder remains valid until `destroy` returns; the
    /// hub guarantees lifetime, the plugin guarantees not to call it
    /// after `destroy`.
    ///
    /// Plugins that don't want metrics still need to provide a thunk
    /// (the `define_plugin!` macro emits a no-op default) — the field
    /// is unconditionally read on v2 hubs.
    ///
    /// **Host access:** gated on `api_version >= PLUGIN_API_VERSION_V2`.
    /// Reading this field on a v1 plugin's vtable is UB — the v1
    /// allocation does not include these bytes.
    ///
    /// [`MetricRecorder`]: crate::MetricRecorder
    pub set_metric_recorder:
        extern "C" fn(state: *mut c_void, record_fn: RecordMetricFn, ctx: *const c_void),

    // v3 additions — present iff api_version >= PLUGIN_API_VERSION_V3.
    /// Block until the plugin's in-flight background work has completed
    /// or `timeout_ms` milliseconds have elapsed. Called by the hub on
    /// a config reload between the registry swap and the call to
    /// `shutdown`, so plugins with fire-and-forget tasks (mirror's
    /// `runtime.spawn`'d HTTP dispatches; otel's batched exporter
    /// queue) can finish ACK'd requests before their runtime is torn
    /// down.
    ///
    /// Returns `true` if the plugin reports it has nothing more in
    /// flight (clean drain). Returns `false` if the timeout elapsed
    /// while in-flight work was still pending (forced shutdown — the
    /// hub will still proceed with `shutdown`, the open requests are
    /// lost). Plugins MUST treat the timeout as advisory — going over
    /// is acceptable in narrow margins, but the hub's overall reload
    /// SLO depends on drain respecting the deadline.
    ///
    /// `process()` calls landing on this plugin instance AFTER drain
    /// is invoked are still serviced; the hub's swap happens before
    /// drain, so post-swap traffic goes to the NEW registry. Drain
    /// concerns only work already in flight against this (now-old)
    /// instance.
    ///
    /// Plugins with no background state (synchronous handlers that
    /// complete inside `process()`) get the `define_plugin!` macro's
    /// default thunk which returns `true` immediately — nothing to
    /// wait for. Plugins that spawn background tasks SHOULD override
    /// `drain` to wait on their in-flight counter.
    ///
    /// **Host access:** gated on `api_version >= PLUGIN_API_VERSION_V3`.
    /// Reading this field on a v1 or v2 plugin's vtable is UB — those
    /// allocations do not include these bytes. The hub treats older
    /// plugins as "drained immediately" because those plugins have no
    /// in-flight background state in practice (coraza, external-auth,
    /// fingerprinting, maxmind, otel pre-v0.4, sso-auth all complete
    /// their work inside `process()`).
    pub drain: extern "C" fn(state: *const c_void, timeout_ms: u64) -> bool,
}

impl PluginVTable {
    /// Read just the `api_version` field of a plugin's vtable.
    ///
    /// # Safety
    ///
    /// `vtable_ptr` must be a non-null pointer to a `PluginVTable`
    /// allocation that is at least 4 bytes long (every plugin has at
    /// least the `api_version` field).
    #[must_use]
    pub unsafe fn read_api_version(vtable_ptr: *const PluginVTable) -> u32 {
        // SAFETY: caller guarantees vtable_ptr is at least 4 bytes long.
        // Reads only the first field; does not borrow the full struct.
        unsafe { (*vtable_ptr).api_version }
    }

    /// Read the v2 `set_metric_recorder` field, returning `None` for
    /// plugins built against v1 (whose allocation doesn't include it).
    ///
    /// # Safety
    ///
    /// `vtable_ptr` must be a non-null pointer to a `PluginVTable`
    /// allocation produced by a published plugin (i.e. consistent with
    /// the `api_version` field's claim about which fields are present).
    /// On v1 plugins this function reads only `api_version` and returns
    /// `None` without touching past the v1 baseline.
    #[must_use]
    pub unsafe fn read_set_metric_recorder(
        vtable_ptr: *const PluginVTable,
    ) -> Option<extern "C" fn(state: *mut c_void, record_fn: RecordMetricFn, ctx: *const c_void)>
    {
        // SAFETY: caller guarantees vtable_ptr is at least 4 bytes long
        // and that api_version accurately describes the layout.
        let api_version = unsafe { Self::read_api_version(vtable_ptr) };
        if api_version < PLUGIN_API_VERSION_V2 {
            return None;
        }
        // SAFETY: api_version >= V2 implies the allocation includes the
        // set_metric_recorder field. Reading the field directly (not
        // via `&PluginVTable`) avoids borrowing the whole struct.
        Some(unsafe { (*vtable_ptr).set_metric_recorder })
    }

    /// Read the v3 `drain` field, returning `None` for plugins built
    /// against v1 or v2 (whose allocation doesn't include it). The
    /// hub treats `None` as "drained immediately" — see the field's
    /// doc comment for the rationale (older plugins are synchronous).
    ///
    /// # Safety
    ///
    /// `vtable_ptr` must be a non-null pointer to a `PluginVTable`
    /// allocation produced by a published plugin (i.e. consistent with
    /// the `api_version` field's claim about which fields are present).
    /// On v1/v2 plugins this function reads only `api_version` and
    /// returns `None` without touching past the relevant baseline.
    #[must_use]
    pub unsafe fn read_drain(
        vtable_ptr: *const PluginVTable,
    ) -> Option<extern "C" fn(state: *const c_void, timeout_ms: u64) -> bool> {
        // SAFETY: caller guarantees vtable_ptr is at least 4 bytes long
        // and that api_version accurately describes the layout.
        let api_version = unsafe { Self::read_api_version(vtable_ptr) };
        if api_version < PLUGIN_API_VERSION_V3 {
            return None;
        }
        // SAFETY: api_version >= V3 implies the allocation includes the
        // drain field. Reading the field directly (not via
        // `&PluginVTable`) avoids borrowing the whole struct.
        Some(unsafe { (*vtable_ptr).drain })
    }
}
