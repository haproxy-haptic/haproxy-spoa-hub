//! Self-fork integration test for the fatal-signal handler.
//!
//! The unit tests in `crash_handler.rs` exercise install + the
//! formatter; they cannot verify the signal-delivery path because a
//! SIGSEGV inside `cargo test` terminates the test binary. This test
//! works around that by self-forking: the parent process spawns its own
//! test binary with an env-var trigger, the child installs the handler
//! and crashes, and the parent captures the child's stderr to assert
//! the backtrace banner landed.

const CRASH_ENV: &str = "SPOA_HUB_CRASH_HANDLER_TEST_TRIGGER";
const CRASH_THREAD_ENV: &str = "SPOA_HUB_CRASH_HANDLER_TEST_TRIGGER_THREAD";

/// Force a SIGSEGV by writing to a non-null but reliably-unmapped
/// address. We use `0xDEAD` rather than `0` because some glibc/kernel
/// hardening configs special-case the null page into a SIGBUS depending
/// on mode. `write_volatile` keeps LLVM from optimizing the access away.
fn force_segv() {
    let p = 0xDEAD as *mut u8;
    // SAFETY: intentional — we want the SIGSEGV to validate the handler.
    unsafe { std::ptr::write_volatile(p, 0u8) };
}

/// Re-invoke this test binary so the matching `#[test]` branches into
/// its child path (gated on `env`), capture the child's stderr, and
/// assert it crashed AND wrote the full fatal-signal backtrace banner.
fn assert_child_dumps_backtrace(test_name: &str, env: &str) {
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(&exe)
        .args(["--exact", test_name, "--nocapture"])
        .env(env, "1")
        .output()
        .expect("spawn child");

    assert!(
        !output.status.success(),
        "child should have crashed but exited cleanly. stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("FATAL: spoa-hub received signal"),
        "expected fatal-signal banner on child's stderr; got:\n{stderr}",
    );
    assert!(
        stderr.contains("end backtrace"),
        "expected 'end backtrace' footer on child's stderr; got:\n{stderr}",
    );

    // The signal number is printed without a 'SIG' prefix; SIGSEGV is
    // 11 on Linux. Don't pin the exact value — different OSes (e.g.
    // x32 ABI, future kernels) may renumber.
    let code = output.status.code();
    let signal = unix_signal(output.status);
    assert!(
        matches!(signal, Some(libc::SIGSEGV | libc::SIGBUS)),
        "expected SIGSEGV (or SIGBUS on some hardened kernels); got code={code:?} signal={signal:?}",
    );
}

#[test]
fn handler_prints_backtrace_on_segv() {
    if std::env::var(CRASH_ENV).is_ok() {
        // Child path: install the handler and fault on the main thread.
        haproxy_spoa_hub::crash_handler::install().expect("install crash handler");
        force_segv();
        // Unreachable — the kernel delivered SIGSEGV before this line.
        // If we somehow get here the test silently passes; the parent's
        // status-code check still catches a regression.
        return;
    }
    assert_child_dumps_backtrace("handler_prints_backtrace_on_segv", CRASH_ENV);
}

/// Regression guard for issue #4's "no backtrace written" symptom: the
/// real-world SIGSEGV fires on a tokio blocking-pool thread, not the
/// main thread. The handler must still capture a backtrace there. This
/// is exactly the path that relied on `install()` priming `libgcc_s`
/// process-wide — without priming, a freshly-spawned thread's first
/// in-signal `backtrace()` would `dlopen` the unwinder from inside the
/// handler and could deadlock, producing the silent crash we saw.
#[test]
fn handler_prints_backtrace_on_segv_from_spawned_thread() {
    if std::env::var(CRASH_THREAD_ENV).is_ok() {
        // Child path: install on the main thread (which primes
        // libgcc_s), then crash on a DIFFERENT, freshly-spawned thread.
        haproxy_spoa_hub::crash_handler::install().expect("install crash handler");
        let handle = std::thread::spawn(force_segv);
        let _ = handle.join();
        return;
    }
    assert_child_dumps_backtrace(
        "handler_prints_backtrace_on_segv_from_spawned_thread",
        CRASH_THREAD_ENV,
    );
}

#[cfg(unix)]
fn unix_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn unix_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}
