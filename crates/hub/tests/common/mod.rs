// Not all test binaries use every helper; suppress dead-code warnings.
#![allow(dead_code)]

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::doc_markdown
)]
pub mod spop_client;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use haproxy_spoa_hub::test_support::{plugin_config, workspace_target_dir};
use spop_client::SpopClient;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub fn unique_sock(prefix: &str) -> String {
    let path = format!(
        "/tmp/spoa-hub-{}-{}-{}.sock",
        prefix,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    // Remove stale socket file from previous test runs
    let _ = std::fs::remove_file(&path);
    path
}

/// Build a shared registry from a list of plugin configs.
pub fn build_registry(
    configs: &[haproxy_spoa_hub::config::PluginConfig],
) -> haproxy_spoa_hub::registry::SharedRegistry {
    let dir = workspace_target_dir();
    let loaded: Vec<_> = configs
        .iter()
        .map(|c| {
            haproxy_spoa_hub::plugin_loader::load_plugin(&dir, c, 100)
                .unwrap_or_else(|e| panic!("failed to load plugin '{}': {e}", c.name))
        })
        .collect();
    Arc::new(arc_swap::ArcSwap::from_pointee(
        haproxy_spoa_hub::registry::PluginRegistry::build(loaded)
            .expect("failed to build registry"),
    ))
}

pub fn noop_config() -> haproxy_spoa_hub::config::PluginConfig {
    plugin_config("noop", "libnoop_plugin.so", &["check-request"], 1000)
}

pub fn delay_config(
    name: &str,
    message: &str,
    timeout_ms: u64,
) -> haproxy_spoa_hub::config::PluginConfig {
    plugin_config(name, "libdelay_plugin.so", &[message], timeout_ms)
}

/// Bind a Unix listener, spawn a single-connection server, and return a
/// connected `SpopClient` ready for NOTIFY calls.
pub async fn start_server_and_connect(
    socket_path: &str,
    registry: haproxy_spoa_hub::registry::SharedRegistry,
) -> (SpopClient<tokio::net::UnixStream>, JoinHandle<()>) {
    let listener = UnixListener::bind(socket_path).expect("failed to bind socket");
    let reg = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .expect("failed to connect");
    let client = SpopClient::handshake(stream)
        .await
        .expect("handshake failed");

    (client, server)
}

/// Bind a TCP listener, spawn a single-connection server, and return a
/// connected `SpopClient` ready for NOTIFY calls.
pub async fn start_tcp_server_and_connect(
    registry: haproxy_spoa_hub::registry::SharedRegistry,
) -> (SpopClient<tokio::net::TcpStream>, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind tcp");
    let addr = listener.local_addr().unwrap();
    let reg = Arc::clone(&registry);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        haproxy_spoa_hub::connection::handle_connection(stream, reg).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("failed to connect to tcp");
    let client = SpopClient::handshake(stream)
        .await
        .expect("tcp handshake failed");

    (client, server)
}

/// Disconnect the client, wait for the server task, and optionally clean up a
/// Unix socket file.
pub async fn cleanup<S>(
    mut client: SpopClient<S>,
    server: JoinHandle<()>,
    socket_path: Option<&str>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    if let Some(path) = socket_path {
        let _ = std::fs::remove_file(path);
    }
}
