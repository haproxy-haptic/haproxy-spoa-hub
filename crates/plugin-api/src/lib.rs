//! Plugin API for haproxy-spoa-hub.
//!
//! # ABI model
//!
//! Plugins are loaded from `.so` files at runtime via `dlopen`. The
//! interface between hub and plugin is a hand-rolled `#[repr(C)]`
//! vtable defined in [`vtable::PluginVTable`], reached via the
//! exported `get_plugin_vtable` symbol.
//!
//! See [`vtable`] for the load-bearing layout invariants and version
//! evolution rules. In short: append-only, never reorder, never remove,
//! version-gate every new field on the host. The matrix test in
//! `crates/hub/tests/abi_matrix.rs` enforces these invariants by
//! loading every published plugin version against the current hub.
//!
//! # Layout discipline for data types
//!
//! `PluginContext`, `SpoeMessage`, `ProcessingResult`, `Diagnostic`,
//! `ConfigValue`, `SpoeValue`, `TxnVariable`, and `VarScope` are all
//! `#[derive(StableAbi)]` which gives them `#[repr(C)]` layout.
//! Existing fields MUST NOT be reordered or removed. Adding a field is
//! a layout change that requires a coordinated plugin-api version bump
//! (and is itself a separate concern from vtable evolution — there is
//! no per-field version-gating on data types).
//!
//! `abi_stable` is pinned to `=0.11.3` in `Cargo.toml` so plugins and
//! hub see byte-identical `RString` / `RVec` / etc. across the FFI
//! boundary. Bumping that pin is a coordinated rollout, not a routine
//! patch.

#![allow(non_camel_case_types, non_local_definitions)]

pub mod metrics;
pub mod types;
pub mod vtable;

pub use abi_stable;
pub use abi_stable::std_types::{
    RBoxError, RHashMap, ROption, RResult, RSlice, RStr, RString, RVec, Tuple2,
};
pub use metrics::{MetricKind, MetricLabel, MetricRecorder, RecordMetricFn};
pub use types::{
    ConfigValue, Diagnostic, DiagnosticSeverity, PluginContext, ProcessingResult, SpoeMessage,
    SpoeValue, TxnVariable, VarScope,
};
pub use vtable::{
    GET_PLUGIN_VTABLE_SYMBOL, GetPluginVTableFn, PLUGIN_API_VERSION, PLUGIN_API_VERSION_V1,
    PLUGIN_API_VERSION_V2, PLUGIN_API_VERSION_V3, PluginVTable,
};

/// Define a plugin and emit the FFI surface (vtable + entry symbol).
///
/// Plugin authors write a regular `impl`-style block; the macro emits
/// `extern "C"` thunks that bridge into it, the static `PluginVTable`,
/// and the `get_plugin_vtable` symbol the hub looks up after `dlopen`.
///
/// `process` is automatically wrapped in `std::panic::catch_unwind` so
/// a panic during request handling does not abort the hub process.
///
/// # Usage
///
/// ```rust,ignore
/// use haproxy_spoa_hub_plugin_api::{ProcessingResult, SpoeMessage, SpoeValue, TxnVariable, define_plugin};
///
/// #[derive(Debug, Default)]
/// struct MyPlugin;
///
/// define_plugin!(MyPlugin, {
///     fn new() -> Self { MyPlugin }
///
///     fn init(&mut self, _: &PluginContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///         Ok(())
///     }
///
///     fn name(&self) -> &str { "my-plugin" }
///     fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
///
///     fn process(&self, _: &SpoeMessage)
///         -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>>
///     {
///         Ok(ProcessingResult::single(TxnVariable::session(
///             "result", SpoeValue::String("ok".into()),
///         )))
///     }
/// });
/// ```
///
/// `config_schema` and `validate` are optional. Add them at the end of
/// the block in that order if you need them. Plugins that omit them
/// get the default no-op behavior (no schema, empty diagnostics).
///
/// # `validate()` contract: do *semantic* checks only
///
/// The hub guarantees that by the time your `validate()` body runs,
/// the [`PluginContext`] passed in has already been validated against
/// your `config_schema()` (in BOTH the load path and the
/// `--validate-socket` path; see `crates/hub/src/plugin_loader.rs`'s
/// `collect_schema_errors` and how it's called from
/// `crates/hub/src/validate/orchestrator.rs`). Schema errors short-
/// circuit before `validate()` is called and surface to the operator
/// as the same diagnostic shape your override would emit.
///
/// **You should therefore never reimplement structural validation
/// inside `validate()`.** No re-checking that a field is a string,
/// no walking arrays-vs-tables defensively, no type-shape
/// assumptions. Use whatever typed accessor your plugin already
/// builds for `init()` (typically `from_context(ctx)` populating a
/// `PluginConfig` struct) — the structure is guaranteed valid.
///
/// `validate()` is for *semantic* checks the JSON Schema can't
/// express. Examples:
/// - Compiling `SecLang` directives via Coraza so a typo'd
///   `SecBogusDirective` becomes a line-numbered diagnostic
///   (haproxy-spoa-hub-plugin-coraza).
/// - Resolving a remote auth-server URL to verify it's reachable
///   (a hypothetical external-auth deep check).
/// - Anything else that requires runtime context the schema can't
///   capture.
///
/// Historical context: pre-hub-v0.5.2, the `--validate-socket` path
/// did NOT schema-check before calling `validate()`, so plugins that
/// added their own structural walking could (and did) drift away
/// from `config_schema()` and the runtime parser. The coraza plugin's
/// `applications`-as-array vs `applications`-as-table mismatch in
/// v0.4.0–v0.4.1 was that bug. Centralising the check in the hub
/// makes it impossible for a plugin author to accidentally repeat.
///
/// # Lifetime constraints
///
/// `name` and `version` MUST return `&'static str`. The macro emits an
/// FFI thunk whose return type is `RStr<'static>`, so a `&str`
/// borrowed from `&self` will not compile. In practice this means
/// returning either a string literal or `env!("CARGO_PKG_VERSION")`.
/// If you need to compute the name from instance fields, store it in
/// a `&'static str` (e.g. via `Box::leak(...)` at construction time)
/// or pre-register the strings as `const`s.
///
/// # Panic safety
///
/// Every author-supplied body is wrapped in `std::panic::catch_unwind`
/// inside its FFI thunk. A panic surfaces as:
/// - `process` / `init` / `create`: `RResult::RErr(PluginPanicError)`.
/// - `validate`: a single `Diagnostic::error` entry returned to the
///   host (not an abort — required for `--validate-socket` mode).
/// - `config_schema`: treated as `RNone`.
/// - `name` / `version`: returned as the literal `"<plugin-panic>"`.
/// - `shutdown` / `destroy`: absorbed; memory may leak but the hub
///   stays up.
#[macro_export]
macro_rules! define_plugin {
    (
        $plugin_ty:ty, {
            fn new() -> Self $new_body:block

            fn init(&mut $init_self:ident, $ctx_param:ident : &PluginContext $(,)?)
                -> Result<(), Box<dyn std::error::Error + Send + Sync>> $init_body:block

            fn name(&$name_self:ident) -> &str $name_body:block

            fn version(&$version_self:ident) -> &str $version_body:block

            fn process(
                &$process_self:ident,
                $msg_param:ident : &SpoeMessage $(,)?
            ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> $process_body:block

            $(fn config_schema(&$schema_self:ident) -> Option<&str> $schema_body:block)?

            $(fn validate(&$validate_self:ident, $vctx_param:ident : &PluginContext $(,)?) -> Vec<Diagnostic> $validate_body:block)?

            $(fn drain(&$drain_self:ident, $drain_timeout_param:ident : u64 $(,)?) -> bool $drain_body:block)?

            $(metrics_static = $metrics_static:path;)?
        }
    ) => {
        // Author-supplied bodies become inherent methods on the
        // plugin type. The thunks below cast the opaque state pointer
        // back to `&Self` (or `&mut Self` for init) and call them.
        impl $plugin_ty {
            #[allow(dead_code)]
            fn __new() -> Self $new_body

            #[allow(clippy::unnecessary_wraps, dead_code)]
            fn __init(
                &mut $init_self,
                $ctx_param: &$crate::PluginContext,
            ) -> ::std::result::Result<(), ::std::boxed::Box<dyn ::std::error::Error + Send + Sync>>
                $init_body

            #[allow(dead_code)]
            fn __name(&$name_self) -> &'static str $name_body

            #[allow(dead_code)]
            fn __version(&$version_self) -> &'static str $version_body

            #[allow(clippy::unnecessary_wraps, dead_code)]
            fn __process(
                &$process_self,
                $msg_param: &$crate::SpoeMessage,
            ) -> ::std::result::Result<
                $crate::ProcessingResult,
                ::std::boxed::Box<dyn ::std::error::Error + Send + Sync>,
            > $process_body

            $(
                #[allow(clippy::unnecessary_wraps, dead_code)]
                fn __config_schema(&$schema_self) -> ::std::option::Option<&'static str> $schema_body
            )?

            $(
                #[allow(clippy::unnecessary_wraps, dead_code)]
                fn __validate(
                    &$validate_self,
                    $vctx_param: &$crate::PluginContext,
                ) -> ::std::vec::Vec<$crate::Diagnostic> $validate_body
            )?

            $(
                #[allow(clippy::unnecessary_wraps, dead_code)]
                fn __drain(&$drain_self, $drain_timeout_param: u64) -> bool $drain_body
            )?
        }

        // FFI thunks. All `unsafe` operations are confined here; the
        // plugin author's bodies above remain in safe Rust.
        const _: () = {
            use ::std::os::raw::c_void;
            use ::std::panic::{AssertUnwindSafe, catch_unwind};

            // Every thunk that calls into user-supplied code is wrapped
            // in `catch_unwind`. Without this, a panic in any plugin
            // method would unwind through `extern "C"` — which Rust
            // defines as abort — taking the whole hub down. Each thunk
            // has a sensible fallback for the catch case so the host
            // observes a structured error rather than UB.

            extern "C" fn create() -> $crate::RResult<*mut c_void, $crate::RBoxError> {
                match catch_unwind(|| {
                    let plugin: ::std::boxed::Box<$plugin_ty> =
                        ::std::boxed::Box::new(<$plugin_ty>::__new());
                    ::std::boxed::Box::into_raw(plugin).cast::<c_void>()
                }) {
                    Ok(raw) => $crate::RResult::ROk(raw),
                    Err(_) => $crate::RResult::RErr($crate::RBoxError::new(
                        $crate::PluginPanicError,
                    )),
                }
            }

            extern "C" fn destroy(state: *mut c_void) {
                if state.is_null() {
                    return;
                }
                // SAFETY: state was produced by `create` via Box::into_raw
                // on `Box<$plugin_ty>`. The hub guarantees one destroy per
                // create. A panic in the plugin's Drop is absorbed; the
                // alternative (abort) would lose every other live plugin.
                let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
                    ::std::mem::drop(::std::boxed::Box::from_raw(state.cast::<$plugin_ty>()));
                }));
            }

            extern "C" fn init(
                state: *mut c_void,
                ctx: &$crate::PluginContext,
            ) -> $crate::RResult<(), $crate::RBoxError> {
                // SAFETY: state was produced by `create` and not yet
                // destroyed. Hub holds an exclusive reference during init.
                let plugin = unsafe { &mut *state.cast::<$plugin_ty>() };
                match catch_unwind(AssertUnwindSafe(|| plugin.__init(ctx))) {
                    Ok(Ok(())) => $crate::RResult::ROk(()),
                    Ok(Err(e)) => $crate::RResult::RErr($crate::RBoxError::from_box(e)),
                    Err(_) => $crate::RResult::RErr($crate::RBoxError::new(
                        $crate::PluginPanicError,
                    )),
                }
            }

            extern "C" fn process(
                state: *const c_void,
                msg: &$crate::SpoeMessage,
            ) -> $crate::RResult<$crate::ProcessingResult, $crate::RBoxError> {
                // SAFETY: state is alive between init and destroy. Process
                // takes &self so concurrent calls share a borrow — the
                // plugin must use interior mutability for any mutable state.
                let plugin = unsafe { &*state.cast::<$plugin_ty>() };
                match catch_unwind(AssertUnwindSafe(|| plugin.__process(msg))) {
                    Ok(Ok(result)) => $crate::RResult::ROk(result),
                    Ok(Err(e)) => $crate::RResult::RErr($crate::RBoxError::from_box(e)),
                    Err(_) => $crate::RResult::RErr($crate::RBoxError::new(
                        $crate::PluginPanicError,
                    )),
                }
            }

            extern "C" fn name(state: *const c_void) -> $crate::RStr<'static> {
                // SAFETY: state alive between init and destroy.
                let plugin = unsafe { &*state.cast::<$plugin_ty>() };
                match catch_unwind(AssertUnwindSafe(|| plugin.__name())) {
                    Ok(s) => $crate::RStr::from(s),
                    Err(_) => $crate::RStr::from("<plugin-panic>"),
                }
            }

            extern "C" fn plugin_version(state: *const c_void) -> $crate::RStr<'static> {
                // SAFETY: state alive between init and destroy.
                let plugin = unsafe { &*state.cast::<$plugin_ty>() };
                match catch_unwind(AssertUnwindSafe(|| plugin.__version())) {
                    Ok(s) => $crate::RStr::from(s),
                    Err(_) => $crate::RStr::from("<plugin-panic>"),
                }
            }

            extern "C" fn shutdown(_state: *const c_void) {
                // The original SpoePlugin trait's shutdown has an empty
                // default body; the macro mirrors that. Plugins that need
                // teardown logic do it in `destroy` (Drop on the boxed
                // state), which is called once after the last use.
                // Wrapped for forward-compat: if the macro grows a
                // user-overridable shutdown body, the wrapper is already
                // here.
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    let _ = _state;
                }));
            }

            extern "C" fn config_schema(_state: *const c_void) -> $crate::ROption<$crate::RString> {
                // catch_unwind around the (potentially user-overridden)
                // body. On panic, return RNone so the hub skips schema
                // validation rather than aborting.
                catch_unwind(AssertUnwindSafe(|| {
                    $crate::__define_plugin_config_schema_thunk!(_state, $plugin_ty $(, $schema_body)?)
                }))
                .unwrap_or($crate::ROption::RNone)
            }

            extern "C" fn validate(
                _state: *const c_void,
                _ctx: &$crate::PluginContext,
            ) -> $crate::RVec<$crate::Diagnostic> {
                // catch_unwind around the (potentially user-overridden)
                // body. On panic, return a single error Diagnostic so
                // the hub surfaces a structured failure instead of
                // aborting (critical for --validate-socket mode).
                catch_unwind(AssertUnwindSafe(|| {
                    $crate::__define_plugin_validate_thunk!(_state, _ctx, $plugin_ty $(, $validate_body)?)
                }))
                .unwrap_or_else(|_| {
                    let mut diags = $crate::RVec::new();
                    diags.push($crate::Diagnostic::error(
                        0,
                        0,
                        "plugin's validate() panicked",
                    ));
                    diags
                })
            }

            extern "C" fn set_metric_recorder(
                _state: *mut c_void,
                _record_fn: $crate::RecordMetricFn,
                _ctx: *const c_void,
            ) {
                // The thunk dispatches to the plugin's `metrics_static`
                // declaration (when present) by calling install() on
                // the named MetricRecorder. When absent, no-op.
                // catch_unwind so a panic in install() doesn't unwind
                // through extern "C".
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    $crate::__define_plugin_metrics_thunk!(_state, _record_fn, _ctx $(, $metrics_static)?)
                }));
            }

            extern "C" fn drain(_state: *const c_void, _timeout_ms: u64) -> bool {
                // Dispatch to user-supplied `fn drain` if present;
                // otherwise the default thunk returns true immediately
                // (correct for synchronous plugins that complete all
                // work inside `process()` — coraza, external-auth,
                // sso-auth, fingerprinting, maxmind, ...).
                //
                // catch_unwind so a panic in a user-defined drain
                // doesn't unwind through extern "C". On panic we
                // return false — the hub treats that as a forced
                // shutdown (counter incremented, in-flight work lost)
                // which is the right outcome for a misbehaving plugin.
                catch_unwind(AssertUnwindSafe(|| {
                    $crate::__define_plugin_drain_thunk!(_state, _timeout_ms, $plugin_ty $(, $drain_body)?)
                }))
                .unwrap_or(false)
            }

            #[allow(non_upper_case_globals)]
            static PLUGIN_VTABLE_INSTANCE: $crate::PluginVTable = $crate::PluginVTable {
                api_version: $crate::PLUGIN_API_VERSION,
                create,
                destroy,
                init,
                process,
                name,
                plugin_version,
                shutdown,
                config_schema,
                validate,
                set_metric_recorder,
                drain,
            };

            #[unsafe(no_mangle)]
            pub extern "C" fn get_plugin_vtable() -> *const $crate::PluginVTable {
                &PLUGIN_VTABLE_INSTANCE
            }
        };
    };
}

/// Internal helper used by `define_plugin!` to expand the optional
/// `config_schema` arm. With body → call the impl; without → return None.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_plugin_config_schema_thunk {
    ($state:ident, $plugin_ty:ty) => {{
        let _ = $state;
        $crate::ROption::RNone
    }};
    ($state:ident, $plugin_ty:ty, $body:block) => {{
        // SAFETY: state alive between init and destroy.
        let plugin = unsafe { &*$state.cast::<$plugin_ty>() };
        match plugin.__config_schema() {
            ::std::option::Option::Some(s) => $crate::ROption::RSome($crate::RString::from(s)),
            ::std::option::Option::None => $crate::ROption::RNone,
        }
    }};
}

/// Internal helper used by `define_plugin!` to expand the optional
/// `validate` arm. With body → call the impl; without → return empty.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_plugin_validate_thunk {
    ($state:ident, $ctx:ident, $plugin_ty:ty) => {{
        let _ = $state;
        let _ = $ctx;
        $crate::RVec::new()
    }};
    ($state:ident, $ctx:ident, $plugin_ty:ty, $body:block) => {{
        // SAFETY: state alive between init and destroy.
        let plugin = unsafe { &*$state.cast::<$plugin_ty>() };
        $crate::RVec::from(plugin.__validate($ctx))
    }};
}

/// Internal helper used by `define_plugin!` to expand the optional
/// `drain` arm. With body → call the impl; without → return true
/// immediately (synchronous plugin, nothing in flight to wait for).
#[doc(hidden)]
#[macro_export]
macro_rules! __define_plugin_drain_thunk {
    ($state:ident, $timeout_ms:ident, $plugin_ty:ty) => {{
        let _ = $state;
        let _ = $timeout_ms;
        true
    }};
    ($state:ident, $timeout_ms:ident, $plugin_ty:ty, $body:block) => {{
        // SAFETY: state alive between init and destroy. Drain is
        // called between the registry swap and shutdown — both are
        // operations on still-live state, so &Self is fine.
        let plugin = unsafe { &*$state.cast::<$plugin_ty>() };
        plugin.__drain($timeout_ms)
    }};
}

/// Internal helper used by `define_plugin!` to expand the optional
/// `metrics_static` arm. With a path → install the recorder on the
/// named static `MetricRecorder`; without → ignore (the plugin doesn't
/// emit metrics).
///
/// The recorder remains a static because callbacks may run from any plugin
/// thread and need a process-stable address. Other plugin state should live on
/// the plugin struct: `define_plugin!` captures each receiver identifier, so
/// author bodies can use `self` normally.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_plugin_metrics_thunk {
    ($state:ident, $record_fn:ident, $ctx:ident) => {{
        // Plugin didn't declare `metrics_static` — no-op install.
        let _ = $state;
        let _ = $record_fn;
        let _ = $ctx;
    }};
    ($state:ident, $record_fn:ident, $ctx:ident, $metrics_static:path) => {{
        let _ = $state;
        // Install on the static MetricRecorder the plugin pointed to.
        // The static is global so it's reachable from every thread
        // for the plugin's lifetime, matching the recorder's lifetime
        // contract (valid until destroy returns; static lives forever
        // → trivially satisfies the bound).
        $metrics_static.install($record_fn, $ctx);
    }};
}

/// Error raised when a plugin's `process` panics. Wrapped in `RBoxError`
/// by the `define_plugin!` macro's panic-safety net.
#[derive(Debug)]
pub struct PluginPanicError;

impl std::fmt::Display for PluginPanicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plugin panicked during message processing")
    }
}

impl std::error::Error for PluginPanicError {}

#[cfg(test)]
mod stateful_macro_tests {
    use super::*;

    #[derive(Debug)]
    struct StatefulPlugin {
        value: i32,
    }

    define_plugin!(StatefulPlugin, {
        fn new() -> Self {
            StatefulPlugin { value: 1 }
        }

        fn init(
            &mut self,
            context: &PluginContext,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.value = context
                .get_config("value")
                .and_then(ConfigValue::as_integer)
                .unwrap_or(1) as i32;
            Ok(())
        }

        fn name(&self) -> &str {
            let _ = self.value;
            "stateful-test"
        }

        fn version(&self) -> &str {
            let _ = self.value;
            "0.0.0"
        }

        fn process(
            &self,
            _message: &SpoeMessage,
        ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(ProcessingResult::single(TxnVariable::transaction(
                "value",
                SpoeValue::Int32(self.value),
            )))
        }

        fn config_schema(&self) -> Option<&str> {
            let _ = self.value;
            None
        }

        fn validate(&self, _context: &PluginContext) -> Vec<Diagnostic> {
            let _ = self.value;
            Vec::new()
        }

        fn drain(&self, _timeout_ms: u64) -> bool {
            self.value > 0
        }
    });

    #[test]
    fn author_bodies_can_read_and_write_instance_state() {
        let mut config = RHashMap::new();
        config.insert("value".into(), ConfigValue::Integer(42));
        let context = PluginContext {
            name: "stateful-test".into(),
            config,
        };
        let message = SpoeMessage {
            name: "test".into(),
            args: RHashMap::new(),
            stream_id: 1,
            frame_id: 1,
        };

        let mut plugin = StatefulPlugin::__new();
        plugin.__init(&context).expect("init should succeed");
        let result = plugin.__process(&message).expect("process should succeed");

        assert!(matches!(result.variables[0].value, SpoeValue::Int32(42)));
        assert!(plugin.__drain(0));
    }
}
