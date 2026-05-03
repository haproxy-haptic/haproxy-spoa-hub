// abi_stable's proc macros generate code that triggers these warnings
#![allow(non_camel_case_types, non_local_definitions)]

pub mod plugin_trait;
pub mod types;

pub use abi_stable;
pub use abi_stable::std_types::{RHashMap, RString, RVec, Tuple2};
pub use plugin_trait::{PluginBox, PluginMod, PluginMod_Ref, SpoePlugin, SpoePlugin_TO};
pub use types::{
    ConfigValue, Diagnostic, DiagnosticSeverity, PluginContext, ProcessingResult, SpoeMessage,
    SpoeValue, TxnVariable, VarScope,
};

/// Define a plugin with automatic panic safety and module export.
///
/// This macro generates the `#[export_root_module]` entry point and
/// wraps the `process` implementation in `std::panic::catch_unwind`
/// to prevent plugin panics from aborting the hub process.
///
/// # Usage
///
/// ```rust,ignore
/// use haproxy_spoa_hub_plugin_api::*;
///
/// #[derive(Debug)]
/// struct MyPlugin;
///
/// define_plugin!(MyPlugin, {
///     fn new() -> Self {
///         MyPlugin
///     }
///
///     fn init(&mut self, _context: &PluginContext)
///         -> Result<(), Box<dyn std::error::Error + Send + Sync>>
///     {
///         Ok(())
///     }
///
///     fn name(&self) -> &str {
///         "my-plugin"
///     }
///
///     fn version(&self) -> &str {
///         "0.1.0"
///     }
///
///     fn process(
///         &self,
///         message: &SpoeMessage,
///     ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> {
///         Ok(ProcessingResult {
///             variables: vec![].into(),
///         })
///     }
/// });
/// ```
/// Internal-only token-burner used by `define_plugin!` to reference a
/// captured `$()?` metavariable inside the expansion (so macro_rules!
/// can resolve which conditional branch to emit) without producing
/// any Rust code itself. Accepts arbitrary token streams. Hidden from
/// rustdoc.
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_present {
    ($($tt:tt)*) => {};
}

#[macro_export]
macro_rules! define_plugin {
    // Single arm — `config_schema` and `validate` are both optional and
    // independently provided. Plugins that only define the required
    // methods (new, init, name, version, process) work as today; plugins
    // that override `config_schema`, `validate`, or both add them in
    // that order at the end of the block. Backward-compatible.
    ($plugin_ty:ty, {
        fn new() -> Self $new_body:block

        fn init(&mut self, $ctx_param:ident : &PluginContext $(,)?) -> Result<(), Box<dyn std::error::Error + Send + Sync>> $init_body:block

        fn name(&self) -> &str $name_body:block

        fn version(&self) -> &str $version_body:block

        fn process(
            &self,
            $msg_param:ident : &SpoeMessage $(,)?
        ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> $process_body:block

        $(fn config_schema(&self) -> Option<&str> $schema_body:block)?

        $(fn validate(&self, $vctx_param:ident : &PluginContext $(,)?) -> Vec<Diagnostic> $validate_body:block)?
    }) => {
        mod __plugin_impl {
            use super::*;
            use $crate::abi_stable::std_types::{
                RBoxError, ROption, RResult, RStr, RString, RVec,
            };
            use $crate::abi_stable::sabi_trait::prelude::TD_Opaque;

            impl $plugin_ty {
                pub fn __new() -> Self $new_body
                #[allow(clippy::unnecessary_wraps)]
                pub fn __init(
                    &mut self,
                    $ctx_param: &$crate::PluginContext,
                ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> $init_body
                pub fn __name(&self) -> &str $name_body
                pub fn __version(&self) -> &str $version_body
                #[allow(clippy::unnecessary_wraps)]
                pub fn __process(
                    &self,
                    $msg_param: &$crate::SpoeMessage,
                ) -> Result<$crate::ProcessingResult, Box<dyn std::error::Error + Send + Sync>> $process_body

                $(
                    #[allow(clippy::unnecessary_wraps)]
                    pub fn __config_schema(&self) -> Option<&str> $schema_body
                )?

                $(
                    #[allow(clippy::unnecessary_wraps)]
                    pub fn __validate(
                        &self,
                        $vctx_param: &$crate::PluginContext,
                    ) -> Vec<$crate::Diagnostic> $validate_body
                )?
            }

            impl $crate::SpoePlugin for $plugin_ty {
                fn init(
                    &mut self,
                    context: &$crate::PluginContext,
                ) -> RResult<(), RBoxError> {
                    self.__init(context).map_err(RBoxError::from_box).into()
                }

                fn process(
                    &self,
                    message: &$crate::SpoeMessage,
                ) -> RResult<$crate::ProcessingResult, RBoxError> {
                    match std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| self.__process(message)),
                    ) {
                        Ok(res) => res.map_err(RBoxError::from_box).into(),
                        Err(_) => {
                            RResult::RErr(RBoxError::new(
                                $crate::PluginPanicError,
                            ))
                        }
                    }
                }

                fn name(&self) -> RStr<'_> {
                    self.__name().into()
                }

                fn version(&self) -> RStr<'_> {
                    self.__version().into()
                }

                fn shutdown(&self) {}

                $(
                    fn config_schema(&self) -> ROption<RString> {
                        $crate::__plugin_present!($schema_body);
                        self.__config_schema().map(RString::from).into()
                    }
                )?

                $(
                    fn validate(
                        &self,
                        context: &$crate::PluginContext,
                    ) -> RVec<$crate::Diagnostic> {
                        $crate::__plugin_present!($validate_body);
                        self.__validate(context).into()
                    }
                )?
            }

            #[$crate::abi_stable::export_root_module]
            pub fn get_root_module() -> $crate::PluginMod_Ref {
                use $crate::abi_stable::prefix_type::PrefixTypeTrait;
                $crate::PluginMod {
                    new: __create_plugin,
                }
                .leak_into_prefix()
            }

            extern "C" fn __create_plugin() -> RResult<$crate::PluginBox, RBoxError> {
                let plugin = <$plugin_ty>::__new();
                RResult::ROk($crate::SpoePlugin_TO::from_value(plugin, TD_Opaque))
            }
        }
    };
}

/// Error type for plugin panics caught by the `define_plugin!` macro.
#[derive(Debug)]
pub struct PluginPanicError;

impl std::fmt::Display for PluginPanicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plugin panicked during message processing")
    }
}

impl std::error::Error for PluginPanicError {}
