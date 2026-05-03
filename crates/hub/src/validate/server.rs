//! Validator-mode unix socket server.
//!
//! Accepts unix-socket connections, reads one length-prefixed JSON
//! request per connection, dispatches to the orchestrator, writes
//! the response, and closes (FR-008). Each request is processed in a
//! tokio task so concurrent connections don't block each other.

use std::{path::Path, sync::Arc, time::Duration};

use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use super::framing::{FrameError, read_frame, write_frame};
use super::orchestrator::{PluginMap, validate_request};
use super::protocol::{ValidateRequest, ValidateResponse};

/// Default per-request timeout (FR-013).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for the validator socket server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub max_frame_bytes: u32,
    pub request_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: super::framing::DEFAULT_MAX_FRAME_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Bind a unix socket at `socket_path` and serve validator requests
/// until `shutdown` is cancelled. The socket file is created (and
/// any pre-existing socket file at the path is removed first); on
/// graceful shutdown the file is also removed so the socket path
/// doesn't leak across restarts.
///
/// Each accepted connection is processed in a spawned task so slow
/// validations don't block other clients.
pub async fn serve(
    socket_path: &Path,
    plugins: Arc<PluginMap>,
    config: ServerConfig,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    // Remove any stale socket file from a prior crash.
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(
        socket = %socket_path.display(),
        max_frame_bytes = config.max_frame_bytes,
        request_timeout_ms = u64::try_from(config.request_timeout.as_millis()).unwrap_or(u64::MAX),
        "validator socket listening"
    );

    loop {
        let (stream, _addr) = tokio::select! {
            res = listener.accept() => res?,
            () = shutdown.cancelled() => {
                tracing::info!("validator socket shutting down");
                break;
            }
        };

        let plugins = plugins.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, plugins, cfg).await {
                tracing::warn!(error = %e, "validator request failed");
            }
        });
    }

    // Clean up the socket file on graceful shutdown so it doesn't
    // leak between restarts. Best-effort: if the file is already
    // gone (concurrent removal, fs unmount, etc.) we ignore the
    // error.
    let _ = std::fs::remove_file(socket_path);

    Ok(())
}

/// Handle one accepted connection: read one request, dispatch, write
/// one response, close. Per-request timeout (FR-013) applies to the
/// entire dispatch path — read, parse, validate, AND write — so a
/// stalled or malicious client that never reads from its end can't
/// pin a tokio task indefinitely on a write that the kernel can't
/// drain.
async fn handle_connection(
    mut stream: UnixStream,
    plugins: Arc<PluginMap>,
    config: ServerConfig,
) -> anyhow::Result<()> {
    let timeout_ms = config.request_timeout.as_millis();
    let outcome = tokio::time::timeout(config.request_timeout, async {
        // Read the request frame.
        let payload = match read_frame(&mut stream, config.max_frame_bytes).await {
            Ok(p) => p,
            Err(FrameError::FrameTooLarge { size, max }) => {
                let resp = ValidateResponse::protocol_error(format!(
                    "request frame exceeds maximum size of {max} bytes (got {size})"
                ));
                return write_response(&mut stream, &resp, config.max_frame_bytes).await;
            }
            Err(FrameError::ConnectionClosed) => {
                // Client closed before sending anything; nothing to
                // respond to.
                return Ok(());
            }
            Err(FrameError::Io(e)) => {
                let resp = ValidateResponse::protocol_error(format!("io error: {e}"));
                return write_response(&mut stream, &resp, config.max_frame_bytes).await;
            }
        };

        // Parse JSON.
        let request: ValidateRequest = match serde_json::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                let resp = ValidateResponse::protocol_error(format!(
                    "request body is not valid JSON: {e}"
                ));
                return write_response(&mut stream, &resp, config.max_frame_bytes).await;
            }
        };

        // Dispatch to orchestrator and write the response — all
        // inside the single timeout so the write can't outlive the
        // budget.
        let response = validate_request(&request, &plugins);
        write_response(&mut stream, &response, config.max_frame_bytes).await
    })
    .await;

    match outcome {
        Ok(write_result) => write_result,
        Err(_elapsed) => {
            // Best-effort write of the timeout diagnostic. If the
            // peer is the cause (slow reader) this write will likely
            // also stall, but we attempt it anyway with a fresh
            // grace window so the operator's logs reflect what
            // happened. Errors are swallowed — the connection is
            // about to be dropped.
            let resp = ValidateResponse::protocol_error(format!(
                "validation timed out after {timeout_ms}ms"
            ));
            let grace = Duration::from_millis(100);
            let _ = tokio::time::timeout(
                grace,
                write_response(&mut stream, &resp, config.max_frame_bytes),
            )
            .await;
            Ok(())
        }
    }
}

/// Serialize and write one response frame. Centralised so the
/// timeout-wrapped path and the timeout-elapsed-fallback path share
/// the same serialization logic.
async fn write_response(
    stream: &mut UnixStream,
    response: &ValidateResponse,
    max_frame_bytes: u32,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(response)?;
    write_frame(stream, &body, max_frame_bytes)
        .await
        .map_err(|e| anyhow::anyhow!("failed to write response: {e}"))?;
    Ok(())
}
