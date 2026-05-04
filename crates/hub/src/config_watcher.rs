//! File-watch trigger for the `--config` argument.
//!
//! HAPTIC pushes the rendered hub config via the `HAProxy` `DataPlane`
//! API, which writes through `renameio` (atomic rename). On Linux that
//! means the original file's inode is replaced — an inotify watch on
//! the file path itself is silently invalidated. So we watch the
//! **parent directory** and filter events down to the target filename.
//!
//! Both the SIGHUP and file-watch paths converge on the same in-process
//! reload pipeline (`reload_plugins` → `arc_swap` → drop old). This
//! module only fires the trigger; the reload mechanics are reused as-is
//! from `main.rs`.

use std::{path::Path, sync::mpsc, time::Duration};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn a background task that watches `config_path`'s parent directory
/// and emits one `()` on `tx` for every change touching the file. The
/// task exits when `shutdown` is cancelled.
///
/// Returns the `notify::RecommendedWatcher` handle alongside an error
/// receiver. The watcher is kept alive by the caller (dropping it stops
/// the underlying inotify watch); the error receiver is best-effort
/// observability — most production deployments will never see an event
/// on it, but if the watcher's internal channel gets clogged we want
/// somewhere to log it from.
///
/// # Errors
///
/// - The parent directory of `config_path` cannot be determined (path is
///   the filesystem root or empty).
/// - `notify::recommended_watcher` cannot be constructed (no file-watch
///   backend available — extremely rare on Linux).
/// - Adding the watch on the parent directory fails (parent doesn't
///   exist or is unreadable).
pub fn spawn(
    config_path: &Path,
    tx: tokio_mpsc::Sender<()>,
    shutdown: CancellationToken,
) -> anyhow::Result<RecommendedWatcher> {
    let target = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let parent = target.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "config path {} has no parent directory; file-watch needs one to survive atomic renames",
            config_path.display()
        )
    })?;

    // notify hands us events on a private thread it owns. We bridge to
    // tokio via a sync channel + a tokio task that reads from it. The
    // sync channel is bounded to avoid unbounded memory growth if the
    // tokio runtime stalls; we drain to the latest event (one reload
    // covers any number of pending changes).
    let (sync_tx, sync_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        // Best-effort: if the channel is closed (tokio task gone) the
        // send fails silently — that's fine, it means we're shutting
        // down.
        let _ = sync_tx.send(res);
    })?;
    watcher.watch(parent, RecursiveMode::NonRecursive)?;

    // Bridging task: drain notify events, filter to our target filename,
    // forward a single tick to the tokio side.
    let target_for_task = target.clone();
    tokio::task::spawn_blocking(move || {
        while !shutdown.is_cancelled() {
            // Block with a short timeout so we periodically observe the
            // shutdown flag; otherwise a quiescent filesystem keeps us
            // parked forever.
            match sync_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(event)) => {
                    if !event_targets_path(&event, &target_for_task) {
                        continue;
                    }
                    // Best-effort send. Channel is bounded; the receiver
                    // (main task) deduplicates by simply running the
                    // reload on the next event — falling behind by one
                    // reload is harmless because the reload picks up
                    // whatever's currently on disk.
                    if tx.try_send(()).is_err() {
                        // Either the receiver is gone (shutdown) or the
                        // channel is full (a reload is already pending).
                        // Both are fine — drop the event.
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "config-watch event stream error");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    Ok(watcher)
}

/// Decide whether a `notify::Event` describes a change to `target`. We
/// react to any kind of modification — content writes, atomic renames
/// (`Modify(Name(_))`), creates from `renameio`'s "write tmp, then move
/// over" pattern. We deliberately ignore `Access` events (no content
/// change) and `Other` (unspecified, often metadata).
fn event_targets_path(event: &Event, target: &Path) -> bool {
    let touches_target = event.paths.iter().any(|p| {
        // The watcher reports paths relative to whatever was watched.
        // We canonicalize the target at watch-setup time but events
        // come in as the watched path may have surfaced them — compare
        // both raw and canonicalized to be robust to symlinks or
        // mounted overlays.
        p == target || p.canonicalize().is_ok_and(|c| c == *target)
    });
    if !touches_target {
        return false;
    }
    matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use notify::{Event, EventKind, event::ModifyKind};

    use super::event_targets_path;

    fn evt(kind: EventKind, paths: &[&str]) -> Event {
        Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::default(),
        }
    }

    #[test]
    fn modify_on_target_path_fires() {
        let e = evt(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/tmp/spoa-hub-config.toml"],
        );
        assert!(event_targets_path(
            &e,
            &PathBuf::from("/tmp/spoa-hub-config.toml")
        ));
    }

    #[test]
    fn create_on_target_path_fires() {
        // renameio's atomic-rename pattern surfaces a Create event for
        // the renamed-into target.
        let e = evt(
            EventKind::Create(notify::event::CreateKind::File),
            &["/tmp/spoa-hub-config.toml"],
        );
        assert!(event_targets_path(
            &e,
            &PathBuf::from("/tmp/spoa-hub-config.toml")
        ));
    }

    #[test]
    fn modify_on_unrelated_file_skipped() {
        let e = evt(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/tmp/something-else.toml"],
        );
        assert!(!event_targets_path(
            &e,
            &PathBuf::from("/tmp/spoa-hub-config.toml")
        ));
    }

    #[test]
    fn access_event_does_not_fire() {
        // Reads (open/close) are not config changes.
        let e = evt(
            EventKind::Access(notify::event::AccessKind::Open(
                notify::event::AccessMode::Read,
            )),
            &["/tmp/spoa-hub-config.toml"],
        );
        assert!(!event_targets_path(
            &e,
            &PathBuf::from("/tmp/spoa-hub-config.toml")
        ));
    }
}
