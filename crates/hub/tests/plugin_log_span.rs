//! A record a plugin logs inside `process()` must carry the dispatch
//! span's `req_id`, so an operator can join it to the hub's own lines
//! for the same request. `process()` runs on the blocking pool, which
//! does not inherit the `spoe_message` span by itself.
//!
//! Own test binary: it installs a global subscriber (the dispatch runs on
//! other tokio threads, so a thread-local default would not see it).

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use spop::types::TypedData;

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

#[tokio::test]
async fn plugin_record_logged_during_process_carries_req_id() {
    let buf = SharedBuf::default();
    let writer = buf.clone();
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .init();

    let socket_path = common::unique_sock("plugin-log-span");
    let registry = common::build_registry(&[common::noop_config()]);
    let (mut client, _server) = common::start_server_and_connect(&socket_path, registry).await;

    let req_id = "019f9e64-e9de-7d1b-88c9-76644f0e9b86";
    let args = HashMap::from([("req_id".to_string(), TypedData::String(req_id.to_string()))]);
    client.send_notify("check-request", args).await.unwrap();

    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let line = out
        .lines()
        .find(|l| l.contains("noop processing check-request"))
        .unwrap_or_else(|| panic!("no forwarded plugin record in:\n{out}"));
    assert!(line.contains(r#""plugin":"noop""#), "{line}");
    assert!(
        line.contains(&format!(r#""req_id":"{req_id}""#)),
        "plugin record must carry the dispatch span's req_id: {line}"
    );
}
