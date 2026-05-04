//! Loaded plugin handle.
//!
//! Owns the dlopened library (so the `.so` stays mapped), the
//! `*const PluginVTable` pointer the plugin handed back via
//! `get_plugin_vtable`, and the opaque `*mut c_void` state pointer
//! produced by the plugin's `create` function.
//!
//! All FFI calls into the plugin go through this struct's methods.
//! New v2+ method accessors MUST version-gate their access with
//! `PLUGIN_API_VERSION_V*` constants — see [`Plugin::validate`] for
//! the canonical pattern. `Plugin` is Send + Sync because the plugin's
//! state is only mutated during init (single-threaded) and `process`
//! takes `&self` (concurrent calls share an immutable borrow).
//!
//! # Drop order
//!
//! `Drop` for `Plugin` runs `shutdown` then `destroy` on the plugin's
//! state, then drops the `Library` (which `dlclose`s the `.so`). The
//! field declaration order in this struct enforces this — Rust drops
//! fields in declaration order, so put `_library` LAST.

use std::os::raw::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use abi_stable::std_types::{ROption, RVec};
use haproxy_spoa_hub_plugin_api::{
    Diagnostic, GET_PLUGIN_VTABLE_SYMBOL, GetPluginVTableFn, PLUGIN_API_VERSION_V1, PluginContext,
    PluginVTable, ProcessingResult, RBoxError, RResult, SpoeMessage,
};

/// Errors a plugin load can produce. Distinct error kinds let callers
/// log structured diagnostics rather than a single string blob.
#[derive(Debug, thiserror::Error)]
pub enum PluginLoadError {
    #[error("dlopen of '{path}' failed: {source}")]
    DlOpen {
        path: String,
        #[source]
        source: libloading::Error,
    },
    #[error("plugin '{path}' missing required symbol 'get_plugin_vtable': {source}")]
    MissingEntrySymbol {
        path: String,
        #[source]
        source: libloading::Error,
    },
    #[error("plugin '{path}' returned a NULL vtable pointer from get_plugin_vtable")]
    NullVTable { path: String },
    #[error(
        "plugin '{path}' reports unsupported api_version {found}; \
         hub requires >= {minimum}"
    )]
    UnsupportedVersion {
        path: String,
        found: u32,
        minimum: u32,
    },
    #[error("plugin '{path}' factory failed: {reason}")]
    CreateFailed { path: String, reason: String },
}

/// Loaded plugin handle. Cloning this is intentionally not provided —
/// only one `Plugin` may own a given state pointer. Wrap in `Arc` for
/// shared ownership across worker threads.
pub struct Plugin {
    state: *mut c_void,
    vtable: *const PluginVTable,
    api_version: u32,
    /// Set to true on the first `shutdown()` call so subsequent calls
    /// (and the Drop impl's safety net) are no-ops at the FFI layer.
    shutdown_called: AtomicBool,
    /// Held purely to keep the `.so` mapped. Dropped LAST after
    /// state is destroyed (Rust drops fields in declaration order).
    _library: libloading::Library,
}

// SAFETY: The plugin's `process` method takes `&self` and the macro
// emits panic-safe thunks. `init` (which takes `&mut self` internally)
// runs once during load before the `Plugin` is shared. After that the
// state is read-only from the host's perspective, so concurrent calls
// from multiple threads are sound.
unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("api_version", &self.api_version)
            .field("name", &self.name())
            .field("plugin_version", &self.plugin_version())
            .finish_non_exhaustive()
    }
}

impl Plugin {
    /// Load a plugin from a `.so` file and create its state.
    ///
    /// On success, the plugin's `init` has NOT yet been called — the
    /// caller is expected to run schema and `validate()` checks first,
    /// then call [`Plugin::init`].
    ///
    /// # Safety
    ///
    /// Loading FFI shared libraries is inherently unsafe. The caller
    /// must trust the `.so` to be a well-formed haproxy-spoa-hub
    /// plugin compatible with at least `PLUGIN_API_VERSION_V1`.
    pub unsafe fn load(lib_path: &Path) -> Result<Self, PluginLoadError> {
        let path_str = lib_path.display().to_string();

        // SAFETY: dlopen is inherently unsafe; we trust the operator's
        // configured plugin path. libloading's Library::new wraps dlopen.
        let library = unsafe { libloading::Library::new(lib_path) }.map_err(|source| {
            PluginLoadError::DlOpen {
                path: path_str.clone(),
                source,
            }
        })?;

        // dlsym for the entry symbol. SAFETY: signature must match
        // GetPluginVTableFn — that's the contract every plugin honors.
        let get_vtable: libloading::Symbol<GetPluginVTableFn> = unsafe {
            library.get(GET_PLUGIN_VTABLE_SYMBOL).map_err(|source| {
                PluginLoadError::MissingEntrySymbol {
                    path: path_str.clone(),
                    source,
                }
            })?
        };

        let vtable: *const PluginVTable = (*get_vtable)();
        if vtable.is_null() {
            return Err(PluginLoadError::NullVTable {
                path: path_str.clone(),
            });
        }

        // SAFETY: plugin honored the entry-symbol contract; vtable is
        // at least 4 bytes long (the api_version field).
        let api_version = unsafe { PluginVTable::read_api_version(vtable) };
        if api_version < PLUGIN_API_VERSION_V1 {
            return Err(PluginLoadError::UnsupportedVersion {
                path: path_str.clone(),
                found: api_version,
                minimum: PLUGIN_API_VERSION_V1,
            });
        }

        // Create the plugin's state. SAFETY: `create` is at v1 baseline
        // offset, always present. Result conversion happens through the
        // ABI-stable RResult type.
        let create_fn = unsafe { (*vtable).create };
        let state = match create_fn() {
            RResult::ROk(ptr) => ptr,
            RResult::RErr(e) => {
                return Err(PluginLoadError::CreateFailed {
                    path: path_str,
                    reason: e.to_string(),
                });
            }
        };

        Ok(Plugin {
            state,
            vtable,
            api_version,
            shutdown_called: AtomicBool::new(false),
            _library: library,
        })
    }

    /// Call the plugin's shutdown hook. Idempotent — repeat calls are
    /// no-ops. The Drop impl also calls this as a safety net.
    pub fn shutdown(&self) {
        // swap returns the previous value; if it was already true,
        // shutdown has run and we skip.
        if self.shutdown_called.swap(true, Ordering::AcqRel) {
            return;
        }
        // SAFETY: shutdown is at v1 baseline, always present. State is
        // alive between create and destroy.
        let shutdown_fn = unsafe { (*self.vtable).shutdown };
        shutdown_fn(self.state);
    }

    /// API version this plugin reports. Matches `PLUGIN_API_VERSION_V*`
    /// constants for known versions; future plugins may report higher
    /// values, in which case the hub uses only the fields it knows about.
    #[must_use]
    pub fn api_version(&self) -> u32 {
        self.api_version
    }

    /// Plugin's display name. Allocates a `String` because the FFI
    /// returns `RStr<'static>` borrowed from the plugin's data section
    /// and we want the host code to not depend on `abi_stable` types.
    #[must_use]
    pub fn name(&self) -> String {
        // SAFETY: name is at v1 baseline, always present.
        let name_fn = unsafe { (*self.vtable).name };
        name_fn(self.state).as_str().to_owned()
    }

    /// Plugin's `SemVer` string.
    #[must_use]
    pub fn plugin_version(&self) -> String {
        // SAFETY: plugin_version is at v1 baseline, always present.
        let version_fn = unsafe { (*self.vtable).plugin_version };
        version_fn(self.state).as_str().to_owned()
    }

    /// Initialize the plugin with its config context. Call once after
    /// `load`, after pre-init checks (`config_schema`, validate) succeed.
    ///
    /// # Errors
    ///
    /// Returns `RBoxError` if the plugin's `init` function returns an
    /// error. The plugin is then in a partially-initialized state and
    /// should be dropped.
    pub fn init(&mut self, ctx: &PluginContext) -> Result<(), RBoxError> {
        // SAFETY: init is at v1 baseline, always present.
        let init_fn = unsafe { (*self.vtable).init };
        match init_fn(self.state, ctx) {
            RResult::ROk(()) => Ok(()),
            RResult::RErr(e) => Err(e),
        }
    }

    /// Process one SPOE message. Plugin's macro wraps this in
    /// `catch_unwind` so a plugin panic surfaces as `RBoxError(...)`
    /// instead of aborting the hub.
    ///
    /// # Errors
    ///
    /// Returns `RBoxError` if the plugin's `process` returns an error
    /// or panics.
    pub fn process(&self, msg: &SpoeMessage) -> Result<ProcessingResult, RBoxError> {
        // SAFETY: process is at v1 baseline, always present.
        let process_fn = unsafe { (*self.vtable).process };
        match process_fn(self.state, msg) {
            RResult::ROk(r) => Ok(r),
            RResult::RErr(e) => Err(e),
        }
    }

    /// Plugin's optional JSON Schema for its `[plugins.params]` table.
    /// `None` means the plugin opted out of schema validation.
    #[must_use]
    pub fn config_schema(&self) -> Option<String> {
        // SAFETY: config_schema is at v1 baseline. Plugins that don't
        // declare a schema get a no-op thunk emitted by define_plugin!
        // that returns RNone.
        let schema_fn = unsafe { (*self.vtable).config_schema };
        match schema_fn(self.state) {
            ROption::RSome(s) => Some(s.as_str().to_owned()),
            ROption::RNone => None,
        }
    }

    /// Run the plugin's deep-validation hook. Returns the diagnostics
    /// (warnings + errors) the plugin produced.
    ///
    /// # Note on version-gating
    ///
    /// `validate` is at the v1 baseline so every plugin built against
    /// `plugin-api >= 0.4.0` exposes it. Future methods (e.g. a
    /// `metrics()` method appended in v2) will need a version check
    /// here — copy the pattern from the example in the module docs.
    #[must_use]
    pub fn validate(&self, ctx: &PluginContext) -> RVec<Diagnostic> {
        // SAFETY: validate is at v1 baseline, always present.
        let validate_fn = unsafe { (*self.vtable).validate };
        validate_fn(self.state, ctx)
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        if self.state.is_null() {
            return;
        }
        // Run shutdown if the host didn't call it explicitly. shutdown()
        // is idempotent, so this is safe to invoke unconditionally.
        self.shutdown();
        // SAFETY: destroy is at v1 baseline, always present. It frees
        // the state pointer the plugin produced via `create`. Called
        // exactly once because we null-out state below.
        unsafe {
            let destroy_fn = (*self.vtable).destroy;
            destroy_fn(self.state);
        }
        self.state = std::ptr::null_mut();
        // _library drops here, dlclose-ing the .so AFTER state is gone.
    }
}
