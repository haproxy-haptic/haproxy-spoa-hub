use abi_stable::std_types::{RBoxError, RResult, RStr};

use haproxy_spoa_hub_plugin_api::{
    PluginPanicError, ProcessingResult, SpoeMessage, SpoePlugin, SpoePlugin_TO,
};

/// A plugin whose `process()` always panics.
#[derive(Debug)]
struct PanickingPlugin;

impl SpoePlugin for PanickingPlugin {
    fn init(&mut self) -> RResult<(), RBoxError> {
        RResult::ROk(())
    }

    fn process(&self, _message: &SpoeMessage) -> RResult<ProcessingResult, RBoxError> {
        // Simulate a bug: instead of returning, we wrap in catch_unwind
        // to test the macro's safety net. Since we can't use the macro
        // in an integration test (it generates an export_root_module),
        // we replicate the catch_unwind wrapper directly.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("intentional panic in process()")
        }));
        match result {
            Ok(Ok(res)) => RResult::ROk(res),
            Ok(Err(e)) => RResult::RErr(RBoxError::from_box(e)),
            Err(_panic) => RResult::RErr(RBoxError::new(PluginPanicError)),
        }
    }

    fn name(&self) -> RStr<'_> {
        "panicking".into()
    }

    fn version(&self) -> RStr<'_> {
        "0.0.1".into()
    }
}

#[test]
fn test_panicking_plugin_returns_rerr() {
    use abi_stable::sabi_trait::prelude::TD_Opaque;

    let plugin: haproxy_spoa_hub_plugin_api::PluginBox =
        SpoePlugin_TO::from_value(PanickingPlugin, TD_Opaque);

    let msg = SpoeMessage {
        name: "test".into(),
        args: Default::default(),
        stream_id: 1,
        frame_id: 1,
    };

    let result = plugin.process(&msg);
    match result {
        RResult::RErr(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("plugin panicked"),
                "expected panic error message, got: {err_msg}"
            );
        }
        RResult::ROk(_) => panic!("expected RErr from panicking plugin, got ROk"),
    }
}

/// A plugin whose `process()` returns a normal error (not a panic).
#[derive(Debug)]
struct ErrorPlugin;

impl SpoePlugin for ErrorPlugin {
    fn init(&mut self) -> RResult<(), RBoxError> {
        RResult::ROk(())
    }

    fn process(&self, _message: &SpoeMessage) -> RResult<ProcessingResult, RBoxError> {
        let result: Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> =
            Err("something went wrong".into());
        match result {
            Ok(res) => RResult::ROk(res),
            Err(e) => RResult::RErr(RBoxError::from_box(e)),
        }
    }

    fn name(&self) -> RStr<'_> {
        "error-plugin".into()
    }

    fn version(&self) -> RStr<'_> {
        "0.0.1".into()
    }
}

#[test]
fn test_error_plugin_returns_rerr_without_panic() {
    use abi_stable::sabi_trait::prelude::TD_Opaque;

    let plugin: haproxy_spoa_hub_plugin_api::PluginBox =
        SpoePlugin_TO::from_value(ErrorPlugin, TD_Opaque);

    let msg = SpoeMessage {
        name: "test".into(),
        args: Default::default(),
        stream_id: 1,
        frame_id: 1,
    };

    let result = plugin.process(&msg);
    match result {
        RResult::RErr(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("something went wrong"),
                "expected error message, got: {err_msg}"
            );
        }
        RResult::ROk(_) => panic!("expected RErr from error plugin, got ROk"),
    }
}

#[test]
fn test_plugin_panic_error_display() {
    let err = PluginPanicError;
    assert_eq!(err.to_string(), "plugin panicked during message processing");
}
