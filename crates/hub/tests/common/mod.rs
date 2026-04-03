// Not all test binaries use every helper; suppress dead-code warnings.
#![allow(dead_code)]

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::match_same_arms,
    clippy::no_effect_underscore_binding,
    clippy::used_underscore_binding
)]
pub mod spop_client;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use spop_client::SpopClient;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub fn unique_sock(prefix: &str) -> String {
    format!(
        "/tmp/spoa-hub-{}-{}-{}.sock",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    )
}

/// Path to `target/debug` where plugin `.so` files are built.
/// (Duplicated from `test_support::workspace_target_dir` because
/// integration tests cannot access `#[cfg(test)]` library modules.)
pub fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
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
    haproxy_spoa_hub::config::PluginConfig {
        name: "noop".to_string(),
        library: "libnoop_plugin.so".to_string(),
        messages: vec!["check-request".to_string()],
        depends_on: vec![],
        timeout_ms: Some(1000),
        params: toml::Table::default(),
    }
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

/// Disconnect the client, wait for the server task, and clean up the socket file.
pub async fn cleanup(
    mut client: SpopClient<tokio::net::UnixStream>,
    server: JoinHandle<()>,
    socket_path: &str,
) {
    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    let _ = std::fs::remove_file(socket_path);
}

/// Disconnect a TCP client and wait for the server task.
pub async fn cleanup_tcp(
    mut client: SpopClient<tokio::net::TcpStream>,
    server: JoinHandle<()>,
) {
    client.disconnect().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}
