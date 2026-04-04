use abi_stable::std_types::{RBoxError, RHashMap, RResult, RStr};

use haproxy_spoa_hub_plugin_api::{
    PluginContext, PluginPanicError, ProcessingResult, SpoeMessage, SpoePlugin, SpoePlugin_TO,
};

/// A plugin whose `process()` always panics.
#[derive(Debug)]
struct PanickingPlugin;

impl SpoePlugin for PanickingPlugin {
    fn init(&mut self, _context: &PluginContext) -> RResult<(), RBoxError> {
        RResult::ROk(())
    }

    fn process(&self, _message: &SpoeMessage) -> RResult<ProcessingResult, RBoxError> {
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

    fn shutdown(&self) {}
}

#[test]
fn test_panicking_plugin_returns_rerr() {
    use abi_stable::sabi_trait::prelude::TD_Opaque;

    let plugin: haproxy_spoa_hub_plugin_api::PluginBox =
        SpoePlugin_TO::from_value(PanickingPlugin, TD_Opaque);

    let msg = SpoeMessage {
        name: "test".into(),
        args: RHashMap::default(),
        stream_id: 1,
        frame_id: 1,
    };

    let Err(e) = plugin.process(&msg).into_result() else {
        panic!("expected RErr from panicking plugin, got ROk");
    };
    let err_msg = e.to_string();
    assert!(
        err_msg.contains("plugin panicked"),
        "expected panic error message, got: {err_msg}"
    );
}

/// A plugin whose `process()` returns a normal error (not a panic).
#[derive(Debug)]
struct ErrorPlugin;

impl SpoePlugin for ErrorPlugin {
    fn init(&mut self, _context: &PluginContext) -> RResult<(), RBoxError> {
        RResult::ROk(())
    }

    fn process(&self, _message: &SpoeMessage) -> RResult<ProcessingResult, RBoxError> {
        let result: Result<ProcessingResult, Box<dyn std::error::Error + Send + Sync>> =
            Err("something went wrong".into());
        result.map_err(RBoxError::from_box).into()
    }

    fn name(&self) -> RStr<'_> {
        "error-plugin".into()
    }

    fn version(&self) -> RStr<'_> {
        "0.0.1".into()
    }

    fn shutdown(&self) {}
}

#[test]
fn test_error_plugin_returns_rerr_without_panic() {
    use abi_stable::sabi_trait::prelude::TD_Opaque;

    let plugin: haproxy_spoa_hub_plugin_api::PluginBox =
        SpoePlugin_TO::from_value(ErrorPlugin, TD_Opaque);

    let msg = SpoeMessage {
        name: "test".into(),
        args: RHashMap::default(),
        stream_id: 1,
        frame_id: 1,
    };

    let Err(e) = plugin.process(&msg).into_result() else {
        panic!("expected RErr from error plugin, got ROk");
    };
    let err_msg = e.to_string();
    assert!(
        err_msg.contains("something went wrong"),
        "expected error message, got: {err_msg}"
    );
}

#[test]
fn test_plugin_panic_error_display() {
    let err = PluginPanicError;
    assert_eq!(err.to_string(), "plugin panicked during message processing");
}
