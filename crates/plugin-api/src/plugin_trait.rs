// Suppressed warnings originate from abi_stable's #[sabi_trait] and
// #[derive(StableAbi)] macro expansions, not from hand-written code.
#![allow(
    clippy::used_underscore_binding,
    clippy::needless_lifetimes,
    clippy::expl_impl_clone_on_copy,
    clippy::must_use_candidate,
    clippy::cast_ptr_alignment
)]

use abi_stable::{
    StableAbi, declare_root_module_statics,
    library::RootModule,
    package_version_strings, sabi_trait,
    sabi_types::VersionStrings,
    std_types::{RBox, RBoxError, ROption, RResult, RStr, RString},
};

use crate::types::{PluginContext, ProcessingResult, SpoeMessage};

/// FFI-safe plugin trait object type.
pub type PluginBox = SpoePlugin_TO<'static, RBox<()>>;

/// The trait that all SPOA hub plugins must implement.
///
/// Plugins are loaded as shared libraries at runtime. The hub calls
/// `init` once after loading, then `process` for each SPOE message
/// that matches the plugin's configured message names.
///
/// # Thread Safety
///
/// `Send + Sync` is required because plugin instances are shared
/// across connections via `Arc`. The `process` method takes `&self`,
/// so plugins must use interior mutability for any mutable state.
///
/// # Panic Safety
///
/// Use the `define_plugin!` macro to implement plugins — it wraps
/// `process` in `catch_unwind` to prevent panics from aborting the
/// hub process.
#[sabi_trait]
pub trait SpoePlugin: Send + Sync + core::fmt::Debug {
    /// Initialize the plugin. Called once after loading.
    ///
    /// The `context` provides the plugin name and any configuration
    /// parameters from the `[plugins.params]` TOML table. Use this
    /// to set up resources (database connections, lookup tables, etc.).
    /// Return `RErr` to abort plugin registration — the hub will log
    /// the error and skip this plugin.
    fn init(&mut self, context: &PluginContext) -> RResult<(), RBoxError>;

    /// Process an SPOE message and return transaction variables.
    ///
    /// Called for each SPOE message that matches this plugin's
    /// configured message names. The `message` contains pre-parsed
    /// typed arguments and connection metadata (stream/frame IDs).
    ///
    /// When this plugin depends on another, the upstream plugin's
    /// output variables are merged into `message.args` with their
    /// namespace-prefixed names (e.g., `"ja3.hash"`).
    ///
    /// Variable names in the result should be unprefixed — the hub
    /// adds the plugin namespace automatically.
    fn process(&self, message: &SpoeMessage) -> RResult<ProcessingResult, RBoxError>;

    /// Human-readable plugin name used for logging and variable
    /// namespace prefixing.
    fn name(&self) -> RStr<'_>;

    /// Semantic version string (e.g., `"0.1.0"`). Logged at plugin
    /// load time.
    fn version(&self) -> RStr<'_>;

    /// Called during hub shutdown. Use this to clean up resources
    /// (close file handles, flush buffers, disconnect from databases).
    ///
    /// This is the last field in the current ABI version. Methods
    /// added in future minor versions will appear below this line
    /// with default implementations.
    #[sabi(last_prefix_field)]
    fn shutdown(&self);

    /// Return a JSON Schema string to validate plugin configuration.
    ///
    /// The hub calls this after `new()` and before `init()`. If
    /// `RSome(schema)` is returned, the hub parses the string as
    /// JSON Schema and validates the plugin's `[plugins.params]`
    /// against it. Validation failure prevents `init()` from being
    /// called.
    ///
    /// Return `RNone` (the default) to skip validation.
    fn config_schema(&self) -> ROption<RString> {
        ROption::RNone
    }
}

/// Root module exported by every plugin shared library.
///
/// Contains a factory function to create new plugin instances.
#[repr(C)]
#[derive(StableAbi)]
#[sabi(kind(Prefix(prefix_ref = PluginMod_Ref)))]
#[sabi(missing_field(panic))]
pub struct PluginMod {
    /// Factory function: create a new plugin instance.
    #[sabi(last_prefix_field)]
    pub new: extern "C" fn() -> RResult<PluginBox, RBoxError>,
}

impl RootModule for PluginMod_Ref {
    declare_root_module_statics! {PluginMod_Ref}
    const BASE_NAME: &'static str = "spoa_plugin";
    const NAME: &'static str = "spoa_plugin";
    const VERSION_STRINGS: VersionStrings = package_version_strings!();
}
