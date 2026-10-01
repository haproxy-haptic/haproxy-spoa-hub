//! Validator-mode unix socket server.
//!
//! Accepts unix-socket connections and serves length-prefixed JSON
//! request frames over each one until the client closes, the
//! connection goes idle past `idle_timeout`, the cooperative
//! `shutdown` token fires, or a transport-level frame error poisons
//! the byte stream. Each connection is processed in its own tokio
//! task so concurrent connections run in parallel; within one
//! connection, requests are serialised pipelined per the wire
//! protocol contract owned by HAPTIC.
//!
//! Per-request semantics:
//!
//! - Application-level errors (malformed JSON, validation timeout,
//!   protocol-version mismatch) write a synthetic error response and
//!   keep the connection open — the next frame might be well-formed.
//! - Transport-level errors (oversized frame, partial read, write
//!   failure) close the connection. The client reconnects.
//!
//! The validation work itself runs on tokio's blocking pool via
//! `spawn_blocking` so a slow `validate()` (Coraza WAF init,
//! regex-heavy CRS rule compile) doesn't pin one of the small
//! number of async worker threads that drive I/O.

use std::{path::Path, sync::Arc, time::Duration};

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::framing::{FrameError, read_frame_bounded, write_frame};
use super::orchestrator::{PluginMap, validate_request};
use super::protocol::{ValidateRequest, ValidateResponse};

/// Default per-request timeout (FR-013). Covers parse + validate +
/// write of one request-response cycle. Read-frame waits are bounded
/// by `idle_timeout`, not this.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Default idle timeout — close the connection after this long with
/// no incoming bytes. The client is expected to reconnect
/// transparently. 60 seconds matches typical HTTP keep-alive defaults
/// and is well below most operating-system file-descriptor pressure
/// thresholds.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for the validator socket server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub max_frame_bytes: u32,
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_connections: usize,
    pub max_concurrent_requests: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: super::framing::DEFAULT_MAX_FRAME_BYTES,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_connections: 8,
            max_concurrent_requests: 2,
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
/// validations on one connection don't block other clients. Within a
/// single connection, requests are sequential pipelined per the
/// HAPTIC wire-protocol contract.
pub async fn serve(
    socket_path: &Path,
    plugins: Arc<PluginMap>,
    config: ServerConfig,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.max_connections > 0,
        "max_connections must be positive"
    );
    anyhow::ensure!(
        config.max_concurrent_requests > 0,
        "max_concurrent_requests must be positive"
    );
    let connections = Arc::new(Semaphore::new(config.max_connections));
    let requests = Arc::new(Semaphore::new(config.max_concurrent_requests));
    let frame_budget = Arc::new(Semaphore::new(config.max_frame_bytes as usize));
    // Remove any stale socket file from a prior crash.
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(
        socket = %socket_path.display(),
        max_frame_bytes = config.max_frame_bytes,
        request_timeout_ms = u64::try_from(config.request_timeout.as_millis()).unwrap_or(u64::MAX),
        idle_timeout_ms = u64::try_from(config.idle_timeout.as_millis()).unwrap_or(u64::MAX),
        "validator socket listening"
    );

    loop {
        let connection_permit = tokio::select! {
            permit = connections.clone().acquire_owned() => permit?,
            () = shutdown.cancelled() => break,
        };
        let (stream, _addr) = tokio::select! {
            res = listener.accept() => res?,
            () = shutdown.cancelled() => {
                tracing::info!("validator socket shutting down");
                break;
            }
        };

        let plugins = plugins.clone();
        let cfg = config.clone();
        let shutdown = shutdown.clone();
        let requests = requests.clone();
        let frame_budget = frame_budget.clone();
        tokio::spawn(async move {
            let _connection_permit = connection_permit;
            if let Err(e) =
                handle_connection(stream, plugins, cfg, shutdown, requests, frame_budget).await
            {
                tracing::warn!(error = %e, "validator connection ended with error");
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

/// Handle one accepted connection: loop on read-frame / dispatch /
/// write-response until the client closes, the connection goes idle,
/// shutdown is signalled, or a transport-level error poisons the
/// stream. Each iteration is one request-response cycle.
async fn handle_connection(
    mut stream: UnixStream,
    plugins: Arc<PluginMap>,
    config: ServerConfig,
    shutdown: CancellationToken,
    requests: Arc<Semaphore>,
    frame_budget: Arc<Semaphore>,
) -> anyhow::Result<()> {
    loop {
        // Wait for the next frame, with idle and shutdown gates. Idle
        // close lets the validator reclaim file descriptors when the
        // controller has been quiet for a while; the controller's
        // pool will reopen the connection on next use.
        let payload = tokio::select! {
            frame = tokio::time::timeout(
                config.idle_timeout,
                read_frame_bounded(&mut stream, config.max_frame_bytes, &frame_budget),
            ) => match frame {
                Ok(Ok(payload)) => payload,
                Ok(Err(FrameError::ConnectionClosed)) => {
                    // Clean client close between frames.
                    return Ok(());
                }
                Ok(Err(FrameError::FrameTooLarge { size, max })) => {
                    // Stream sync is unrecoverable: we don't know
                    // whether the next bytes are still part of this
                    // oversized frame. Best-effort write the error
                    // and close.
                    let resp = ValidateResponse::protocol_error(format!(
                        "request frame exceeds maximum size of {max} bytes (got {size})"
                    ));
                    let _ = write_response(&mut stream, &resp, config.max_frame_bytes).await;
                    return Ok(());
                }
                Ok(Err(FrameError::Io(e))) => {
                    // Transport poison; close.
                    return Err(anyhow::anyhow!("read frame: {e}"));
                }
                Err(_elapsed) => {
                    // Idle timeout. Close cleanly so the controller
                    // sees EOF on its next request and reconnects.
                    tracing::debug!("validator connection idle, closing");
                    return Ok(());
                }
            },
            () = shutdown.cancelled() => {
                tracing::debug!("validator connection closed by shutdown");
                return Ok(());
            }
        };

        // One request-response cycle, with the per-request timeout
        // covering parse + validate + write. Application-level
        // errors (malformed JSON, timeout) keep the connection open;
        // only write failures poison it.
        let cycle_outcome = tokio::select! {
            outcome = run_cycle(&mut stream, payload, &plugins, &config, &requests) => outcome,
            () = shutdown.cancelled() => return Ok(()),
        };
        match cycle_outcome {
            CycleOutcome::Continue => {}
            CycleOutcome::Close => return Ok(()),
            CycleOutcome::Error(e) => return Err(e),
        }
    }
}

/// Outcome of one request-response cycle.
enum CycleOutcome {
    /// Cycle completed (well-formed or app-level error response sent);
    /// keep the connection open for the next frame.
    Continue,
    /// Cycle ended in a state where the byte stream is no longer
    /// trustworthy (write failure mid-frame, timeout that interrupted
    /// a write); close the connection cleanly.
    Close,
    /// Cycle hit a fatal transport error worth bubbling up to the
    /// connection-level error log.
    Error(anyhow::Error),
}

/// Run one request-response cycle: parse JSON, dispatch to the
/// orchestrator on the blocking pool, write the response. Wrapped in
/// the per-request timeout so a slow `validate()` can't pin the
/// connection.
async fn run_cycle(
    stream: &mut UnixStream,
    frame: (Vec<u8>, OwnedSemaphorePermit),
    plugins: &Arc<PluginMap>,
    config: &ServerConfig,
    requests: &Arc<Semaphore>,
) -> CycleOutcome {
    let timeout_ms = config.request_timeout.as_millis();

    let outcome = tokio::time::timeout(config.request_timeout, async {
        let plugins = plugins.clone();
        let response = execute_bounded(requests, move || {
            let (payload, frame_permit) = frame;
            let request: ValidateRequest = match serde_json::from_slice(&payload) {
                Ok(request) => request,
                Err(error) => {
                    return ValidateResponse::protocol_error(format!(
                        "request body is not valid JSON: {error}"
                    ));
                }
            };
            drop(payload);
            drop(frame_permit);
            validate_request(&request, &plugins)
        })
        .await
        .unwrap_or_else(|error| {
            ValidateResponse::protocol_error(format!("internal validator error: {error}"))
        });

        write_response(stream, &response, config.max_frame_bytes).await
    })
    .await;

    match outcome {
        Ok(Ok(())) => CycleOutcome::Continue,
        Ok(Err(e)) => {
            // Write or serialize failed; the byte stream is
            // mid-frame. Surface the error and close.
            CycleOutcome::Error(e)
        }
        Err(_elapsed) => {
            // Best-effort write of the timeout diagnostic with a
            // grace window. After a timeout we can't trust the
            // stream state — close.
            let resp = ValidateResponse::protocol_error(format!(
                "validation timed out after {timeout_ms}ms"
            ));
            let grace = Duration::from_millis(100);
            let _ =
                tokio::time::timeout(grace, write_response(stream, &resp, config.max_frame_bytes))
                    .await;
            CycleOutcome::Close
        }
    }
}

async fn execute_bounded<F, T>(requests: &Arc<Semaphore>, work: F) -> anyhow::Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = requests.clone().acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        // A client timeout cannot cancel compilation or release its capacity.
        let _permit = permit;
        work()
    })
    .await?)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_request_keeps_capacity_until_work_finishes() {
        let requests = Arc::new(Semaphore::new(1));
        let gate = requests.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            execute_bounded(&gate, move || {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
            })
            .await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(requests.available_permits(), 0);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                execute_bounded(&requests, || panic!(
                    "queued validation ran without capacity"
                ))
            )
            .await
            .is_err()
        );
        finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), execute_bounded(&requests, || 42))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.available_permits(), 1);
    }

    #[tokio::test]
    async fn panicking_work_releases_capacity() {
        let requests = Arc::new(Semaphore::new(1));
        assert!(
            execute_bounded(&requests, || panic!("test panic"))
                .await
                .is_err()
        );
        assert_eq!(requests.available_permits(), 1);
        assert_eq!(execute_bounded(&requests, || 42).await.unwrap(), 42);
    }
}
