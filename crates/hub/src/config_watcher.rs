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

use std::path::Path;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_util::sync::CancellationToken;

/// Watch the config file's parent directory. Matching notifications coalesce
/// while a reload is pending. Dropping the watcher stops the filesystem watch;
/// cancelling `shutdown` closes the trigger channel.
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
    })?.to_owned();

    // notify already invokes the callback on its own thread. Filter there and
    // keep at most one pending change, including under a burst of inotify events.
    let (notify_tx, mut changes) = tokio_mpsc::channel(1);
    let mut watcher = notify::recommended_watcher(move |event| match event {
        Ok(event) if event_targets_path(&event, &target) => {
            let _ = notify_tx.try_send(());
        }
        Err(e) => tracing::warn!(error = %e, "config-watch event stream error"),
        Ok(_) => {}
    })?;
    watcher.watch(&parent, RecursiveMode::NonRecursive)?;

    // Leave the blocking pool available for plugin calls, including on hosts
    // where the single plugin and the pool both have a concurrency limit of one.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                event = changes.recv() => match event {
                    Some(()) => { let _ = tx.try_send(()); }
                    None => break,
                },
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
