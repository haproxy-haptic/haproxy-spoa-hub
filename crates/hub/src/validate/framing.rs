//! Length-prefixed JSON framing for the validator socket protocol.
//!
//! Frame layout:
//!
//! ```text
//! | 4 bytes (big-endian, unsigned) | N bytes        |
//! |          length = N            |  JSON payload  |
//! ```
//!
//! Connections are persistent: a client opens once and pipelines
//! multiple request frames, with the server writing one response
//! frame per request in arrival order. Either side closes the
//! connection at inter-frame boundaries (idle timeout, graceful
//! shutdown, transport error). The wire-protocol contract is owned
//! by HAPTIC; see haproxy-template-ic `docs/development/validator-protocol.md`.
//!
//! The maximum frame size is configurable via the validator-mode CLI
//! flag and applies to both request and response frames.

use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default maximum frame size — 8 MiB. Overridable at startup.
///
/// Raised from 1 MiB when requests began carrying the data files a config
/// references, which are whole rule sets. Measured: the OWASP CRS the coraza
/// plugin embeds (51 files, 713 KB on disk) JSON-encodes to 794 KB — 76% of the
/// old ceiling before an operator adds a custom rule, with the config sharing
/// the same frame.
///
/// Must stay in step with HAPTIC's `MaxFrameSize`
/// (`pkg/controller/pluggablevalidator/protocol.go`): the controller refuses to
/// encode a larger frame, and this side refuses to read one, so a mismatch
/// fails every request in whichever direction is tighter.
pub const DEFAULT_MAX_FRAME_BYTES: u32 = 8_388_608;

/// Length-prefix size in bytes (big-endian u32).
const LENGTH_PREFIX: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("connection closed before reading the length prefix")]
    ConnectionClosed,
    #[error("frame size {size} exceeds maximum {max}")]
    FrameTooLarge { size: u32, max: u32 },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Read one length-prefixed frame from `reader`. Returns the frame's
/// JSON bytes (without the length prefix). Errors on close-before-data,
/// frames exceeding `max_bytes`, or any I/O error.
pub async fn read_frame<R>(reader: &mut R, max_bytes: u32) -> Result<Vec<u8>, FrameError>
where
    R: AsyncReadExt + Unpin,
{
    let len = read_length(reader, max_bytes).await?;
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

pub(super) async fn read_frame_bounded<R>(
    reader: &mut R,
    max_bytes: u32,
    budget: &Arc<Semaphore>,
    body_timeout: Duration,
) -> Result<(Vec<u8>, OwnedSemaphorePermit), FrameError>
where
    R: AsyncReadExt + Unpin,
{
    let len = read_length(reader, max_bytes).await?;
    let permit = budget
        .clone()
        .acquire_many_owned(len)
        .await
        .map_err(io::Error::other)?;
    let mut payload = vec![0u8; len as usize];
    tokio::time::timeout(body_timeout, reader.read_exact(&mut payload))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "frame body read timed out"))??;
    Ok((payload, permit))
}

async fn read_length<R>(reader: &mut R, max_bytes: u32) -> Result<u32, FrameError>
where
    R: AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; LENGTH_PREFIX];
    let n = reader.read_exact(&mut len_buf).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::ConnectionClosed
        } else {
            FrameError::Io(e)
        }
    })?;
    debug_assert_eq!(n, LENGTH_PREFIX);

    let len = u32::from_be_bytes(len_buf);
    if len > max_bytes {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: max_bytes,
        });
    }

    Ok(len)
}

/// Write one length-prefixed frame to `writer`. Returns an error if
/// the payload would exceed `max_bytes`.
pub async fn write_frame<W>(
    writer: &mut W,
    payload: &[u8],
    max_bytes: u32,
) -> Result<(), FrameError>
where
    W: AsyncWriteExt + Unpin,
{
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::FrameTooLarge {
        size: u32::MAX,
        max: max_bytes,
    })?;
    if len > max_bytes {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: max_bytes,
        });
    }
    let len_buf = len.to_be_bytes();
    writer.write_all(&len_buf).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn bounded_frame_holds_capacity_until_released() {
        let (mut sender, mut receiver) = duplex(64);
        write_frame(&mut sender, b"first", 8).await.unwrap();
        let budget = Arc::new(Semaphore::new(8));
        let (payload, permit) =
            read_frame_bounded(&mut receiver, 8, &budget, Duration::from_secs(1))
                .await
                .unwrap();
        assert_eq!(payload, b"first");
        assert_eq!(budget.available_permits(), 3);
        write_frame(&mut sender, b"next", 8).await.unwrap();
        let next = read_frame_bounded(&mut receiver, 8, &budget, Duration::from_secs(1));
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut next)
                .await
                .is_err()
        );
        drop(permit);
        let (payload, permit) = next.await.unwrap();
        assert_eq!(payload, b"next");
        assert_eq!(budget.available_permits(), 4);
        drop(permit);
        assert_eq!(budget.available_permits(), 8);
    }

    #[tokio::test]
    async fn stalled_body_deadline_releases_capacity() {
        let (mut sender, mut receiver) = duplex(64);
        sender.write_all(&8u32.to_be_bytes()).await.unwrap();
        sender.write_all(b"{").await.unwrap();
        let budget = Arc::new(Semaphore::new(8));
        let error = read_frame_bounded(&mut receiver, 8, &budget, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Io(error) if error.kind() == io::ErrorKind::TimedOut));
        assert_eq!(budget.available_permits(), 8);
    }

    #[tokio::test]
    async fn cancelled_partial_frame_releases_capacity() {
        let (mut sender, mut receiver) = duplex(64);
        sender.write_all(&8u32.to_be_bytes()).await.unwrap();
        let budget = Arc::new(Semaphore::new(8));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                read_frame_bounded(&mut receiver, 8, &budget, Duration::from_secs(1)),
            )
            .await
            .is_err()
        );
        assert_eq!(budget.available_permits(), 8);
    }

    #[tokio::test]
    async fn round_trips_small_payload() {
        let (mut a, mut b) = duplex(64);
        let payload = b"hello world";
        write_frame(&mut a, payload, 1024).await.unwrap();
        let received = read_frame(&mut b, 1024).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn write_rejects_oversized_payload() {
        let (mut a, _b) = duplex(64);
        let payload = vec![0u8; 100];
        let err = write_frame(&mut a, &payload, 50).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::FrameTooLarge { size: 100, max: 50 }
        ));
    }

    #[tokio::test]
    async fn read_rejects_oversized_frame() {
        // Manually craft a length-prefix that exceeds the max; reader
        // should reject before allocating the buffer.
        let (mut a, mut b) = duplex(64);
        let big_len: u32 = 1_000_000;
        a.write_all(&big_len.to_be_bytes()).await.unwrap();
        // Don't write any payload — read_frame should error on the
        // length check before trying to read it.
        let err = read_frame(&mut b, 100).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::FrameTooLarge {
                size: 1_000_000,
                max: 100
            }
        ));
    }

    #[tokio::test]
    async fn read_errors_on_premature_close() {
        let (a, mut b) = duplex(64);
        drop(a);
        let err = read_frame(&mut b, 1024).await.unwrap_err();
        assert!(matches!(err, FrameError::ConnectionClosed));
    }

    #[tokio::test]
    async fn read_errors_on_truncated_payload() {
        // Write a length prefix saying 100 bytes follow, then close
        // after writing only 10. The reader should fail on the
        // payload read.
        let (mut a, mut b) = duplex(1024);
        let len: u32 = 100;
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&[0u8; 10]).await.unwrap();
        drop(a);
        let err = read_frame(&mut b, 1024).await.unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[tokio::test]
    async fn handles_empty_payload() {
        // Zero-length frame is valid wire-wise (the orchestrator will
        // reject empty JSON, but framing accepts it).
        let (mut a, mut b) = duplex(64);
        write_frame(&mut a, b"", 1024).await.unwrap();
        let received = read_frame(&mut b, 1024).await.unwrap();
        assert!(received.is_empty());
    }
}
