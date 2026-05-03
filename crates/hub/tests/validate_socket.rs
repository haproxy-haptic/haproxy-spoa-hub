//! End-to-end integration tests for the validator socket server.
//!
//! Brings up the unix-socket server with the workspace's
//! `validating-noop-plugin` loaded (which exercises both
//! `config_schema` and `validate` overrides), connects a client over
//! the socket, exchanges length-prefixed JSON frames, and asserts on
//! the response.

use std::{path::PathBuf, sync::Arc, time::Duration};

use haproxy_spoa_hub::{
    test_support::{plugin_config, workspace_target_dir},
    validate::{
        ServerConfig, ValidationResult,
        framing::write_frame,
        load_all_plugins_for_validation,
        protocol::{PROTOCOL_VERSION, RequestFile, ValidateRequest, ValidateResponse},
        serve,
    },
};
use tempfile::tempdir;
use tokio::{io::AsyncReadExt, net::UnixStream};
use tokio_util::sync::CancellationToken;

fn socket_dir() -> tempfile::TempDir {
    tempdir().expect("tempdir")
}

fn validator_plugin_config() -> Vec<haproxy_spoa_hub::config::PluginConfig> {
    vec![plugin_config(
        "validating-noop",
        "libvalidating_noop_plugin.so",
        &["test-msg"],
        1000,
    )]
}

async fn round_trip(socket_path: &std::path::Path, request: &ValidateRequest) -> ValidateResponse {
    // Connect
    let mut stream = UnixStream::connect(socket_path).await.expect("connect");
    // Write request frame
    let body = serde_json::to_vec(request).unwrap();
    write_frame(&mut stream, &body, 1_048_576)
        .await
        .expect("write");
    // Read response frame (length-prefixed)
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    serde_json::from_slice(&body).expect("decode response")
}

async fn run_against_server<F, Fut>(test: F)
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let dir = socket_dir();
    let socket_path = dir.path().join("validator.sock");

    let plugin_dir = workspace_target_dir();
    let configs = validator_plugin_config();
    let plugins = Arc::new(load_all_plugins_for_validation(&plugin_dir, &configs));
    assert!(
        plugins.contains_key("validating-noop"),
        "validating-noop plugin must be built (run `cargo build` first); workspace target dir = {plugin_dir:?}"
    );

    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server_socket = socket_path.clone();
    let handle = tokio::spawn(async move {
        let _ = serve(
            &server_socket,
            plugins,
            ServerConfig::default(),
            server_shutdown,
        )
        .await;
    });

    // Wait for the socket file to exist.
    for _ in 0..200 {
        if std::fs::metadata(&socket_path).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        std::fs::metadata(&socket_path).is_ok(),
        "validator socket never bound at {socket_path:?}"
    );

    test(socket_path).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_config_returns_valid() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: r#"
                    [hub]
                    listen = "0.0.0.0:9000"

                    [plugins.params.validating-noop]
                    "#
                .to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Valid);
        assert!(response.warnings.is_empty());
        assert!(response.errors.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warn_flag_yields_warning() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: r#"
                    [plugins.params.validating-noop]
                    make_me_warn = true
                    "#
                .to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Warning);
        assert_eq!(response.warnings.len(), 1);
        assert_eq!(response.warnings[0].path, "config.toml");
        assert_eq!(response.warnings[0].message, "warn flag was set");
        assert!(response.errors.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fail_flag_yields_error() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: r#"
                    [plugins.params.validating-noop]
                    make_me_fail = true
                    "#
                .to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert_eq!(response.errors[0].path, "config.toml");
        assert_eq!(response.errors[0].message, "fail flag was set");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_plugin_yields_error() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: r#"
                    [plugins.params.absent-plugin]
                    x = 1
                    "#
                .to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert!(response.errors[0].message.contains("absent-plugin"));
        assert!(response.errors[0].message.contains("not loaded"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_toml_yields_parse_error_with_line() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: "[unclosed-table\nfoo = 1\n".to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert_eq!(response.errors[0].path, "config.toml");
        assert!(
            response.errors[0].line >= 1,
            "expected non-zero line number; got {}",
            response.errors[0].line
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_files_array_yields_protocol_error() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: Vec::new(),
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert!(
            response.errors[0]
                .message
                .contains("'files' array must be non-empty")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_protocol_version_yields_protocol_error() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: 99,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: String::new(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert!(response.errors[0].message.contains("protocol version 99"));
    })
    .await;
}

/// Regression test: the socket file must be removed when the server
/// shuts down gracefully. Without this, a restart reuses the same
/// path and the stale-file removal at startup is the only safety net
/// — fine for crashes but ugly on clean exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_file_removed_on_graceful_shutdown() {
    let dir = socket_dir();
    let socket_path = dir.path().join("validator.sock");

    let plugin_dir = workspace_target_dir();
    let configs = validator_plugin_config();
    let plugins = Arc::new(load_all_plugins_for_validation(&plugin_dir, &configs));

    let shutdown = CancellationToken::new();
    let server_shutdown = shutdown.clone();
    let server_socket = socket_path.clone();
    let handle = tokio::spawn(async move {
        let _ = serve(
            &server_socket,
            plugins,
            ServerConfig::default(),
            server_shutdown,
        )
        .await;
    });

    // Wait for the socket to bind.
    for _ in 0..200 {
        if std::fs::metadata(&socket_path).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        std::fs::metadata(&socket_path).is_ok(),
        "socket never bound"
    );

    // Trigger graceful shutdown and wait for serve() to return.
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("serve task didn't exit within 2s")
        .expect("serve task panicked");

    // The socket file must be gone.
    assert!(
        std::fs::metadata(&socket_path).is_err(),
        "socket file at {socket_path:?} still exists after graceful shutdown — \
         file leak between restarts will eventually need a manual cleanup"
    );
}
