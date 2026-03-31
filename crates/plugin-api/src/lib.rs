// abi_stable's proc macros generate code that triggers these warnings
#![allow(non_camel_case_types, non_local_definitions)]

pub mod plugin_trait;
pub mod types;

pub use abi_stable;
pub use plugin_trait::{PluginBox, PluginMod, PluginMod_Ref, SpoePlugin, SpoePlugin_TO};
pub use types::{ProcessingResult, SpoeMessage, SpoeValue, TxnVariable, VarScope};

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
///     fn init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
#[macro_export]
macro_rules! define_plugin {
    ($plugin_ty:ty, {
        fn new() -> Self $new_body:block

        fn init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> $init_body:block

        fn name(&self) -> &str $name_body:block

        fn version(&self) -> &str $version_body:block

        fn process(
            &self,
            $msg_param:ident : &SpoeMessage $(,)?
        ) -> Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> $process_body:block
    }) => {
        // Internal wrapper that implements the abi_stable trait
        mod __plugin_impl {
            use super::*;
            use $crate::abi_stable::std_types::{RBoxError, RResult, RStr, RBox};
            use $crate::abi_stable::sabi_trait::prelude::TD_Opaque;

            impl $plugin_ty {
                pub fn __new() -> Self $new_body
                pub fn __init(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> $init_body
                pub fn __name(&self) -> &str $name_body
                pub fn __version(&self) -> &str $version_body
                pub fn __process(
                    &self,
                    $msg_param: &$crate::SpoeMessage,
                ) -> Result<$crate::ProcessingResult, Box<dyn std::error::Error + Send + Sync>> $process_body
            }

            impl $crate::SpoePlugin for $plugin_ty {
                fn init(&mut self) -> RResult<(), RBoxError> {
                    match self.__init() {
                        Ok(()) => RResult::ROk(()),
                        Err(e) => RResult::RErr(RBoxError::from_box(e)),
                    }
                }

                fn process(
                    &self,
                    message: &$crate::SpoeMessage,
                ) -> RResult<$crate::ProcessingResult, RBoxError> {
                    // Wrap in catch_unwind for panic safety
                    let result = std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| self.__process(message)),
                    );
                    match result {
                        Ok(Ok(res)) => RResult::ROk(res),
                        Ok(Err(e)) => RResult::RErr(RBoxError::from_box(e)),
                        Err(_panic) => {
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
