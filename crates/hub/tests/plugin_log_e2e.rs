//! End-to-end test for the plugin-API v4 log path: a record emitted with
//! the `log` macros inside a plugin `.so` must come out of the hub's
//! `tracing` subscriber, attributed to the plugin.
//!
//! The noop example plugin logs once at `info` in `init` and once at
//! `debug` per `process`. A JSON subscriber writing into a shared buffer
//! is installed as this thread's default before the plugin is loaded, so
//! the max level handed to the plugin at install time reflects it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use abi_stable::std_types::RHashMap;
use haproxy_spoa_hub::plugin::Plugin;
use haproxy_spoa_hub::test_support::workspace_target_dir;
use haproxy_spoa_hub_plugin_api::{
    ConfigValue, PLUGIN_API_VERSION, PluginContext, RString, SpoeMessage,
};

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[test]
fn plugin_log_records_reach_the_hub_subscriber() {
    let buf = SharedBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let so = workspace_target_dir().join("libnoop_plugin.so");
    // SAFETY: the noop example is a well-formed plugin built by this workspace.
    let mut plugin = unsafe { Plugin::load(&so) }.expect("load noop plugin");
    assert_eq!(plugin.api_version(), PLUGIN_API_VERSION);
    plugin.install_log_sink("noop-under-test");

    let ctx = PluginContext {
        name: RString::from("noop-under-test"),
        config: RHashMap::<RString, ConfigValue>::new(),
    };
    plugin.init(&ctx).expect("init noop plugin");
    let msg = SpoeMessage {
        name: RString::from("check-request"),
        args: RHashMap::new(),
        stream_id: 1,
        frame_id: 1,
    };
    plugin.process(&msg).expect("process");

    let out = buf.contents();
    let init_line = out
        .lines()
        .find(|l| l.contains("noop plugin initialised as noop-under-test"))
        .unwrap_or_else(|| panic!("no forwarded init record in:\n{out}"));
    assert!(init_line.contains(r#""level":"INFO""#), "{init_line}");
    assert!(init_line.contains(r#""target":"plugin""#), "{init_line}");
    assert!(
        init_line.contains(r#""plugin":"noop-under-test""#),
        "records carry the [[plugins]] name, not the library's: {init_line}"
    );
    assert!(
        init_line.contains(r#""log_target":"noop_plugin""#),
        "{init_line}"
    );

    let process_line = out
        .lines()
        .find(|l| l.contains("noop processing check-request"))
        .unwrap_or_else(|| panic!("no forwarded process record in:\n{out}"));
    assert!(
        process_line.contains(r#""level":"DEBUG""#),
        "{process_line}"
    );
}
