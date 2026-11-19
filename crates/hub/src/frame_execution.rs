//! Let the first standalone plugin on a frame build and write its ACK on the
//! thread that finishes the plugin. Socket I/O and plugin deadlines remain
//! asynchronous. Dispatch outside this frame driver uses ordinary Tokio
//! blocking jobs, as do groups that need several plugins to run in parallel.

use futures::FutureExt;
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    sync::{Arc, Mutex},
    task::Poll,
};
use tokio::{sync::oneshot, time::Instant};

struct PendingCall {
    deadline: Instant,
    call: Box<dyn FnOnce() + Send>,
}

tokio::task_local! {
    static NEXT_CALL: RefCell<Option<PendingCall>>;
}

pub(crate) fn can_defer() -> bool {
    NEXT_CALL
        .try_with(|slot| slot.borrow().is_none())
        .unwrap_or(false)
}

pub(crate) async fn defer<T: Send + 'static>(
    deadline: Instant,
    call: impl FnOnce() -> T + Send + 'static,
) -> Result<Result<T, oneshot::error::RecvError>, tokio::time::error::Elapsed> {
    let (send, recv) = oneshot::channel();
    NEXT_CALL.with(|slot| {
        *slot.borrow_mut() = Some(PendingCall {
            deadline,
            call: Box::new(move || {
                let _ = send.send(call());
            }),
        });
    });
    // Yield the outer frame to the driver before registering a result or timer
    // waker. The worker resumes the frame after returning from FFI, and the
    // driver resumes it independently at the deadline.
    let mut first_poll = true;
    poll_fn(|_| {
        if std::mem::take(&mut first_poll) {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    tokio::time::timeout_at(deadline, recv).await
}

/// The blocking worker claims the frame on return. The Tokio task can claim
/// it first at the deadline. Exactly one resumes execution from the await.
pub(crate) async fn run(frame: impl Future<Output = ()> + Send + 'static) {
    let mut frame = Box::pin(frame);
    let call = NEXT_CALL
        .scope(
            RefCell::new(None),
            poll_fn(|cx| match frame.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(None),
                Poll::Pending => match NEXT_CALL.with(RefCell::take) {
                    Some(call) => Poll::Ready(Some(call)),
                    None => Poll::Pending,
                },
            }),
        )
        .await;
    let Some(call) = call else {
        return;
    };

    let suspended = Arc::new(Mutex::new(Some(frame)));
    let ready = Arc::clone(&suspended);
    let deadline = call.deadline;
    let mut worker = tokio::task::spawn_blocking(move || {
        (call.call)();
        let mut frame = ready.lock().unwrap().take()?;
        // Usually all that remains is building and sending the ACK. Return any
        // pending socket I/O or later plugin to Tokio without holding this slot.
        frame.as_mut().now_or_never().is_none().then_some(frame)
    });

    // When the worker has already claimed the frame at the deadline it can
    // only be doing the single nonblocking poll above. Otherwise the caller
    // takes the frame immediately and reports the timeout while FFI finishes.
    let frame = match tokio::time::timeout_at(deadline, &mut worker).await {
        Ok(Ok(pending)) => pending,
        Ok(Err(error)) => {
            tracing::error!(%error, "frame worker failed");
            suspended.lock().unwrap().take()
        }
        Err(_) => {
            let pending = suspended.lock().unwrap().take();
            if pending.is_some() {
                pending
            } else {
                worker.await.ok().flatten()
            }
        }
    };
    if let Some(frame) = frame {
        frame.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_frame_releases_the_only_blocking_slot() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            let (finished, observed) = oneshot::channel();
            tokio::time::timeout_at(
                deadline,
                run(async move {
                    let id = defer(deadline, || std::thread::current().id())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(id, std::thread::current().id(), "worker resumes the frame");
                    tokio::task::spawn_blocking(|| ()).await.unwrap();
                    finished.send(()).unwrap();
                }),
            )
            .await
            .expect("frame kept its slot while awaiting later work");
            assert!(observed.await.is_ok());
        });
    }
}
