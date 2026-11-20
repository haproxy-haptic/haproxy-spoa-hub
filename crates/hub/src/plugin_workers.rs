//! On a one-CPU hub normal frames are serialized by the frame gate. Reserve
//! one blocking slot for them, keeping overflow capacity for parallel groups
//! and plugin calls that remain in FFI after their frame times out.
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::{
    runtime::{Builder, Handle, Runtime},
    task::JoinHandle,
};

struct Primary {
    handle: Handle,
    busy: AtomicBool,
}

static PRIMARY: OnceLock<Primary> = OnceLock::new();

/// Partition the daemon's existing blocking budget before starting the listener.
/// A one-slot pool already provides affinity without a separate runtime.
pub fn configure(
    capacity: usize,
    keep_alive: Duration,
) -> std::io::Result<(Option<Runtime>, usize)> {
    if capacity == 1 || std::thread::available_parallelism().is_ok_and(|cores| cores.get() > 1) {
        return Ok((None, capacity));
    }
    let rt = Builder::new_current_thread()
        .max_blocking_threads(1)
        .thread_name("spoa-primary")
        .thread_keep_alive(keep_alive)
        .build()?;
    PRIMARY
        .set(Primary {
            handle: rt.handle().clone(),
            busy: AtomicBool::new(false),
        })
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "plugin worker already configured",
            )
        })?;
    Ok((Some(rt), capacity - 1))
}

pub(crate) struct Lease(&'static AtomicBool);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) fn spawn<T: Send + 'static>(
    work: impl FnOnce(Option<Lease>) -> T + Send + 'static,
) -> JoinHandle<T> {
    spawn_with(PRIMARY.get(), work)
}

fn spawn_with<T: Send + 'static>(
    primary: Option<&'static Primary>,
    work: impl FnOnce(Option<Lease>) -> T + Send + 'static,
) -> JoinHandle<T> {
    if let Some(primary) = primary {
        if primary
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let lease = Lease(&primary.busy);
            let caller = Handle::current();
            return primary.handle.spawn_blocking(move || {
                let _context = caller.enter();
                work(Some(lease))
            });
        }
    }
    tokio::task::spawn_blocking(move || work(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_calls_reuse_primary_and_detached_work_leaves_overflow_available() {
        let primary_rt = Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let shared = Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        static LOCAL: OnceLock<Primary> = OnceLock::new();
        let primary = LOCAL.get_or_init(|| Primary {
            handle: primary_rt.handle().clone(),
            busy: AtomicBool::new(false),
        });
        shared.block_on(async {
            let id = || std::thread::current().id();
            let first = spawn_with(Some(primary), move |_lease| id()).await.unwrap();
            let (release, blocked) = std::sync::mpsc::channel();
            let stuck = spawn_with(Some(primary), move |_lease| {
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                id()
            });
            let overflow = spawn_with(Some(primary), move |_lease| id()).await.unwrap();
            assert_ne!(first, overflow);
            release.send(()).unwrap();
            assert_eq!(first, stuck.await.unwrap());
            assert_eq!(
                first,
                spawn_with(Some(primary), move |_lease| id()).await.unwrap()
            );
        });
    }
}
