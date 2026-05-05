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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};
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
        "validating-noop plugin must be built (run `cargo build` first); workspace target dir = {}",
        plugin_dir.display()
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
        "validator socket never bound at {}",
        socket_path.display()
    );

    test(socket_path).await;

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

// All validator-mode integration tests use the canonical `[[plugins]]`
// array-of-tables schema — the exact shape `HubConfig` deserialises at
// runtime startup, and the shape downstream HAPTIC's chart-rendered
// `general/spoa-hub-config.toml` emits. Earlier drafts of the
// validator accepted a simplified `[plugins.params.<name>]` flat
// schema; that was removed because it didn't match what the runtime
// hub parses, which defeated the point of validating with the same
// plugin set.

/// Build a minimally well-formed canonical hub-config TOML for the
/// validator-mode tests. `extra_params` is appended verbatim inside
/// the validating-noop entry's `[plugins.params]` table, e.g.
/// `"make_me_fail = true\n"`.
fn canonical_config(extra_params: &str) -> String {
    format!(
        r#"
plugin_dir = "/etc/haproxy-spoa-hub/plugins"

[[listeners]]
type = "unix"
address = "/run/spoa/hub.sock"

[[plugins]]
name = "validating-noop"
library = "libvalidating_noop_plugin.so"
messages = ["test-msg"]

[plugins.params]
{extra_params}
"#,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_config_returns_valid() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: canonical_config(""),
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
                content: canonical_config("make_me_warn = true\n"),
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

/// Regression test for the silent-no-op bug fixed alongside the legacy-
/// schema removal. With the unfixed orchestrator, sending the canonical
/// `[[plugins]]` array shape made `as_table()` on `plugins` return
/// `None`, the dispatch loop never ran, and every `validate()` call
/// silently came back with zero diagnostics. Downstream HAPTIC's
/// admission webhook then accepted Ingresses carrying invalid
/// `nginx.ingress.kubernetes.io/modsecurity-snippet` `SecRules`, the
/// runtime hub blew up at next reload trying to load the unknown
/// directive — the whole point of `validate()` is to catch that at
/// admission time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fail_flag_yields_error() {
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: canonical_config("make_me_fail = true\n"),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(
            response.errors.len(),
            1,
            "expected exactly the validating-noop fail diagnostic; got {:?}",
            response.errors,
        );
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
[[plugins]]
name = "absent-plugin"
library = "libabsent.so"
messages = ["test-msg"]
[plugins.params]
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
async fn legacy_flat_plugins_table_is_rejected() {
    // The old `[plugins.params.<name>]` shape — accepted by drafts of
    // this validator before v0.5.1 — must now be rejected with a
    // schema-shape diagnostic. Producers were never supposed to use
    // it: the same TOML cannot be loaded by the runtime hub, which
    // defeats the point of a validate() that runs the same plugin
    // set as the data plane.
    run_against_server(|socket_path| async move {
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: r"
[plugins.params.validating-noop]
make_me_fail = true
"
                .to_owned(),
            }],
        };
        let response = round_trip(&socket_path, &request).await;
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(response.errors.len(), 1);
        assert!(
            response.errors[0].message.contains("Array of Tables"),
            "expected schema-shape diagnostic, got {:?}",
            response.errors[0].message
        );
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

/// Read one length-prefixed response frame from a stream. Used by the
/// keep-alive tests below that don't go through `round_trip` (which
/// connects a fresh stream per call).
async fn read_response_frame(stream: &mut UnixStream) -> ValidateResponse {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.expect("read len");
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.expect("read body");
    serde_json::from_slice(&body).expect("decode response")
}

async fn write_request_frame(stream: &mut UnixStream, request: &ValidateRequest) {
    let body = serde_json::to_vec(request).unwrap();
    write_frame(stream, &body, 1_048_576).await.expect("write");
}

fn valid_request() -> ValidateRequest {
    ValidateRequest {
        protocol_version: PROTOCOL_VERSION,
        files: vec![RequestFile {
            path: "config.toml".into(),
            content: canonical_config(""),
        }],
    }
}

/// One connection serves multiple sequential request-response cycles
/// (keep-alive). The HAPTIC-owned wire protocol mandates persistent
/// connections; pipelined N requests on one stream must each get an
/// in-order response without the server closing between cycles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_alive_serves_multiple_requests_on_one_connection() {
    run_against_server(|socket_path| async move {
        let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

        for _ in 0..3 {
            write_request_frame(&mut stream, &valid_request()).await;
            let response = read_response_frame(&mut stream).await;
            assert_eq!(response.result, ValidationResult::Valid);
        }

        // Connection still usable for one more — proves the server
        // didn't close after a fixed N.
        write_request_frame(&mut stream, &valid_request()).await;
        let response = read_response_frame(&mut stream).await;
        assert_eq!(response.result, ValidationResult::Valid);
    })
    .await;
}

/// Multiple concurrent connections from the same client run in
/// parallel. Each connection's request-response is independent of
/// the others; a slow validation on one MUST NOT block another.
/// Fires N requests concurrently on N distinct connections, expects
/// all to complete cleanly and within a generous bound (the server
/// uses `spawn_blocking` + tokio task per connection, so true
/// parallelism is available).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_connections_run_independently() {
    run_against_server(|socket_path| async move {
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let path = socket_path.clone();
            tasks.push(tokio::spawn(async move {
                let response = round_trip(&path, &valid_request()).await;
                assert_eq!(response.result, ValidationResult::Valid);
            }));
        }
        for t in tasks {
            t.await.expect("task panicked");
        }
    })
    .await;
}

/// Parallel plugin dispatch within a single validator request:
/// multiple `[[plugins]]` entries in the same TOML must run
/// concurrently rather than sequentially. Two entries each carrying
/// `sleep_ms = 100` should complete in ~100ms (max), not ~200ms (sum).
///
/// Both entries name the same loaded plugin (`validating-noop`) — the
/// orchestrator iterates `[[plugins]]` by index and resolves each
/// entry's name against the same plugin Arc, so duplicate entries are
/// legal and produce concurrent calls into one plugin instance. This
/// also exercises the thread-safety contract: `Plugin: Send + Sync`
/// + concurrent `&self` access on `validate()`.
///
/// Diagnostic ordering must match input order: entry 0's warning
/// comes before entry 1's error regardless of which thread finishes
/// first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_dispatch_within_request_runs_concurrently() {
    run_against_server(|socket_path| async move {
        // Two `[[plugins]]` entries, each with a 100ms sleep. Entry 0
        // emits a warning; entry 1 emits an error. Serial would take
        // ~200ms; parallel should take ~100ms plus thread-spawn
        // overhead. The 100ms unit is large enough that CI scheduling
        // jitter is well below the parallel/serial gap (~100ms).
        let toml = r#"
plugin_dir = "/etc/haproxy-spoa-hub/plugins"

[[listeners]]
type = "unix"
address = "/run/spoa/hub.sock"

[[plugins]]
name = "validating-noop"
library = "libvalidating_noop_plugin.so"
messages = ["test-msg"]

[plugins.params]
sleep_ms = 100
make_me_warn = true

[[plugins]]
name = "validating-noop"
library = "libvalidating_noop_plugin.so"
messages = ["test-msg"]

[plugins.params]
sleep_ms = 100
make_me_fail = true
"#;
        let request = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: toml.to_owned(),
            }],
        };

        // Warmup round-trip with a no-sleep config so the validator's
        // tokio blocking pool, dlopen lazy-binding, and JSON-Schema
        // compilation cache are all primed. Without this, the first
        // measured call carries ~30-40ms of one-shot overhead that
        // pollutes the timing assertion. The warmup also confirms the
        // server is fully up and responding.
        let warmup = ValidateRequest {
            protocol_version: PROTOCOL_VERSION,
            files: vec![RequestFile {
                path: "config.toml".into(),
                content: canonical_config(""),
            }],
        };
        let _ = round_trip(&socket_path, &warmup).await;

        let start = std::time::Instant::now();
        let response = round_trip(&socket_path, &request).await;
        let elapsed = start.elapsed();

        // Lower bound: the sleeps must actually have happened.
        assert!(
            elapsed >= Duration::from_millis(90),
            "round trip too fast ({elapsed:?}); sleeps were skipped?"
        );
        // Upper bound: serial dispatch would be >=200ms. We require
        // < 170ms to fail loud on regression, with 70ms of headroom
        // above the 100ms parallel work — covers thread spawn/join,
        // socket I/O, and CI scheduling jitter.
        assert!(
            elapsed < Duration::from_millis(170),
            "round trip too slow ({elapsed:?}); dispatch is serial, not parallel"
        );

        // Both diagnostics present, in input order.
        assert_eq!(response.result, ValidationResult::Error);
        assert_eq!(
            response.warnings.len(),
            1,
            "expected exactly the entry-0 warning; got {:?}",
            response.warnings,
        );
        assert_eq!(response.warnings[0].message, "warn flag was set");
        assert_eq!(
            response.errors.len(),
            1,
            "expected exactly the entry-1 error; got {:?}",
            response.errors,
        );
        assert_eq!(response.errors[0].message, "fail flag was set");
    })
    .await;
}

/// An application-level error (malformed JSON in the body) MUST keep
/// the connection open: the next frame on the same connection still
/// gets served. Only transport-level errors (oversized frame, partial
/// read) are allowed to poison the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_keeps_connection_open() {
    run_against_server(|socket_path| async move {
        let mut stream = UnixStream::connect(&socket_path).await.expect("connect");

        // First frame: garbage that decodes as bad JSON.
        let garbage = b"{this is not json";
        let len = u32::try_from(garbage.len()).unwrap();
        stream
            .write_all(&len.to_be_bytes())
            .await
            .expect("write len");
        stream.write_all(garbage).await.expect("write body");

        let resp = read_response_frame(&mut stream).await;
        assert_eq!(resp.result, ValidationResult::Error);
        assert!(
            resp.errors[0].message.contains("not valid JSON"),
            "expected JSON-decode protocol error; got {:?}",
            resp.errors[0].message
        );

        // Second frame on the same connection: well-formed. Must be
        // served — the server kept the connection alive after the
        // app-level error.
        write_request_frame(&mut stream, &valid_request()).await;
        let resp = read_response_frame(&mut stream).await;
        assert_eq!(
            resp.result,
            ValidationResult::Valid,
            "connection was closed after app-level error; keep-alive contract broken"
        );
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
