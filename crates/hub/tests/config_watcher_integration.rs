//! Integration test for `config_watcher::spawn`.
//!
//! Exercises the real notify backend + tokio bridging + parent-dir
//! watch logic against the actual filesystem operation pattern that
//! HAPTIC's dataplane API uses to push hub configs (atomic rename via
//! `renameio`-style write-tmp-then-rename).

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use haproxy_spoa_hub::config_watcher;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn the watcher, atomically replace the target file the way the
/// `HAProxy` `DataPlane` API does, and assert the change notification
/// arrives on the channel within a reasonable timeout.
///
/// "Atomic replace" here mirrors `renameio::WriteFile`: write the new
/// content to a sibling tmp file, then `rename(tmp, target)`. On
/// Linux this ends up as inotify `CREATE+MOVED_TO` events on the
/// parent directory, with the target inode now pointing at the new
/// file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_observes_atomic_rename() {
    let dir = tempdir().expect("tempdir");
    let target: PathBuf = dir.path().join("config.toml");

    // Initial content so the file exists when the watcher attaches —
    // makes canonicalisation succeed and matches the production
    // bootstrap (the chart writes a placeholder before the hub starts).
    fs::write(&target, b"# placeholder\n").expect("write initial");

    let (tx, mut rx) = mpsc::channel::<()>(1);
    let shutdown = CancellationToken::new();
    let _watcher = config_watcher::spawn(&target, tx, shutdown.clone()).expect("spawn watcher");

    // Atomic rename: write to sibling tmp, rename over target.
    let tmp = dir.path().join("config.toml.tmp");
    fs::write(&tmp, b"# new content\n").expect("write tmp");
    fs::rename(&tmp, &target).expect("rename");

    // Allow up to 2s for the inotify roundtrip + tokio scheduling.
    // Local runs see this in <50ms; we leave headroom for CI.
    let deadline = Duration::from_secs(2);
    let started = Instant::now();
    let received = tokio::time::timeout(deadline, rx.recv()).await;
    assert!(
        received.is_ok(),
        "expected change notification within {deadline:?}, got nothing after {:?}",
        started.elapsed()
    );
    assert!(received.unwrap().is_some(), "channel closed unexpectedly");

    // Verify shutdown cancels the bridging task. We can't easily
    // observe the task exiting (it's spawn_blocking), but cancelling
    // is part of the contract — at minimum it shouldn't deadlock.
    shutdown.cancel();
}

/// A modify on an unrelated sibling file in the watched directory must
/// NOT trigger a notification. The watcher watches the *parent dir*
/// (so it survives atomic renames), so it sees events for every file
/// in that dir; the path-filter must reject siblings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_ignores_sibling_changes() {
    let dir = tempdir().expect("tempdir");
    let target: PathBuf = dir.path().join("config.toml");
    let unrelated: PathBuf = dir.path().join("unrelated.toml");

    fs::write(&target, b"# target\n").expect("write target");
    fs::write(&unrelated, b"# unrelated\n").expect("write unrelated");

    let (tx, mut rx) = mpsc::channel::<()>(1);
    let shutdown = CancellationToken::new();
    let _watcher = config_watcher::spawn(&target, tx, shutdown.clone()).expect("spawn watcher");

    // Touch only the unrelated file.
    fs::write(&unrelated, b"# unrelated changed\n").expect("rewrite unrelated");

    // No event should arrive. Wait briefly to give a hypothetical
    // false-positive a chance to surface.
    let result = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(
        result.is_err(),
        "got unexpected change event for sibling file: {result:?}"
    );

    shutdown.cancel();
}
