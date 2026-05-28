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

#[test]
fn handler_prints_backtrace_on_segv() {
    if std::env::var(CRASH_ENV).is_ok() {
        // Child path. Install the handler and force a SIGSEGV via a
        // volatile write to a non-null but unmapped address. We use
        // 0xDEAD as the target rather than 0 because some glibc /
        // kernel hardening configurations special-case the
        // null-page in a way that surfaces SIGBUS instead of SIGSEGV
        // depending on mode — 0xDEAD is reliably unmapped.
        // ptr::write_volatile keeps LLVM from optimizing the access
        // away.
        haproxy_spoa_hub::crash_handler::install().expect("install crash handler");
        let p = 0xDEAD as *mut u8;
        // SAFETY: intentional: we want the SIGSEGV to validate that
        // the handler runs.
        unsafe { std::ptr::write_volatile(p, 0u8) };
        // Unreachable — the kernel delivered SIGSEGV to this thread
        // before this line could execute. If we somehow get here,
        // the test silently passes (which is fine; the parent's
        // status-code check still catches a regression).
        return;
    }

    // Parent path. Re-invoke ourselves with the env var so the matching
    // test branches into the child path above.
    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(&exe)
        .args(["--exact", "handler_prints_backtrace_on_segv", "--nocapture"])
        .env(CRASH_ENV, "1")
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
    let signal = unix_signal(&output.status);
    assert!(
        matches!(signal, Some(libc::SIGSEGV) | Some(libc::SIGBUS)),
        "expected SIGSEGV (or SIGBUS on some hardened kernels); got code={code:?} signal={signal:?}",
    );
}

#[cfg(unix)]
fn unix_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn unix_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}
