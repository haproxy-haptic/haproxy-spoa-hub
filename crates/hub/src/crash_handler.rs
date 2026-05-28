//! Fatal-signal backtrace capture.
//!
//! Installs a `sigaction` handler for SIGSEGV / SIGABRT / SIGBUS / SIGILL
//! / SIGFPE that prints a raw backtrace to file descriptor 2 (stderr) and
//! then re-raises the signal with the default disposition so the process
//! exits with `128 + signo` — preserving the original termination cause
//! and any core-dump generation the kernel would have done.
//!
//! ## Why this exists
//!
//! Rust-side `tracing` / `eprintln!` / the standard `backtrace` crate are
//! NOT async-signal-safe. Calling them from a fatal-signal handler can
//! deadlock on the global allocator (the offending thread may have been
//! holding the malloc lock when the SIGSEGV fired) or recursively fault.
//! The result is a silent crash with no diagnostic output — exactly the
//! failure mode HAPTIC's e2e [3.3] shard hit on pipeline 2559668798
//! (spoa-hub container exit code 139, no Rust panic or stack in the
//! container logs).
//!
//! `libc::backtrace` + `libc::backtrace_symbols_fd` are the canonical
//! signal-safe primitives for raw stack capture. They write directly to a
//! file descriptor using a pre-allocated scratch buffer (no allocation,
//! no locks) — exactly what an async-signal-safe handler is allowed to
//! do.
//!
//! ## Output shape
//!
//! When a fatal signal fires, the handler writes:
//!
//! ```text
//! === FATAL: spoa-hub received signal NN — backtrace follows ===
//! /usr/local/bin/haproxy-spoa-hub(symbol_name+0xoff) [0xaddress]
//! /usr/local/bin/haproxy-spoa-hub(+0xoff) [0xaddress]
//! ...
//! === end backtrace; re-raising signal NN ===
//! ```
//!
//! Resolving stripped addresses to source locations is a post-mortem step:
//! `addr2line -e /path/to/debug-build/haproxy-spoa-hub 0xaddress` against
//! a binary that retains debuginfo. The release image ships stripped,
//! but the debug binary is preserved in the upstream package registry
//! alongside the release for exactly this purpose.
//!
//! ## Re-raise discipline
//!
//! After the backtrace lands, the handler restores the signal's default
//! disposition and re-raises. This matters for two reasons:
//!
//! - Exit code: the kernel reports `128 + signo` when a default handler
//!   terminates, which is what container runtimes / Kubernetes inspect
//!   to classify the crash. Calling `std::process::abort()` instead
//!   would emit code 134 (SIGABRT) for every signal — losing the
//!   distinction between SIGSEGV, SIGBUS, and friends.
//! - Core dumps: if `ulimit -c` permits, the default disposition writes
//!   a core file. An `abort()` shortcut would suppress that.
//!
//! ## Signals NOT caught
//!
//! `SIGTERM` / `SIGINT` / `SIGHUP` are handled elsewhere in the binary
//! (graceful shutdown / config reload). This module is strictly for
//! programming-error signals where the process state is already
//! compromised and the only sensible action is to dump and die.

use std::os::raw::{c_int, c_void};

/// Maximum stack frames we capture. 128 is a generous bound for any
/// realistic SPOE / FFI call depth; the static array sits on stack so
/// the cost is just frame space (≈ 1 KiB on a 64-bit target).
const MAX_FRAMES: usize = 128;

/// Async-signal-safe sigaction handler.
///
/// SAFETY: invoked by the kernel on signal delivery. Must only call
/// async-signal-safe libc functions (POSIX.1 lists the safe set; the
/// relevant ones here are `write`, `backtrace`, `backtrace_symbols_fd`,
/// `signal`, `raise`). Allocator calls, locking, formatted printing —
/// all forbidden.
extern "C" fn handle_fatal_signal(sig: c_int) {
    // The buffer holds raw frame addresses captured by libc::backtrace.
    // Allocated on stack to avoid touching the (possibly poisoned) heap.
    let mut frames: [*mut c_void; MAX_FRAMES] = [std::ptr::null_mut(); MAX_FRAMES];

    // SAFETY: backtrace() is documented as async-signal-safe on Linux
    // glibc. It walks the stack via _Unwind_Backtrace (or equivalent)
    // and writes function-call return addresses to our buffer. Returns
    // the count of frames written, capped at MAX_FRAMES.
    //
    // MAX_FRAMES is a small const (128), so the usize → c_int cast is
    // guaranteed lossless. clippy can't see through const inputs.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let max_frames_c: c_int = MAX_FRAMES as c_int;
    let n = unsafe { libc::backtrace(frames.as_mut_ptr(), max_frames_c) };

    // Banner. Hand-built byte slices (no formatting) so we don't pull in
    // anything that allocates. Include the signal number in single-digit
    // form for the common range (SIGSEGV=11, SIGABRT=6, SIGBUS=7,
    // SIGILL=4, SIGFPE=8); two-digit signal numbers also encode
    // correctly because `format_signo` writes digits in order.
    write_str(b"\n=== FATAL: spoa-hub received signal ");
    format_signo(sig);
    write_str(b" \xe2\x80\x94 backtrace follows ===\n");

    // SAFETY: backtrace_symbols_fd is async-signal-safe (it does NOT
    // call malloc, unlike its sibling backtrace_symbols). It writes
    // one line per frame to fd 2 (stderr).
    unsafe {
        libc::backtrace_symbols_fd(frames.as_ptr(), n, libc::STDERR_FILENO);
    }

    write_str(b"=== end backtrace; re-raising signal ");
    format_signo(sig);
    write_str(b" ===\n");

    // Restore the default disposition for this signal and re-raise.
    // The default disposition for SIGSEGV/SIGABRT/SIGBUS/SIGILL/SIGFPE
    // is `Core` — kernel terminates the process with exit code
    // 128 + signo and writes a core file if ulimit -c permits.
    //
    // SAFETY: signal() is async-signal-safe; SIG_DFL is the well-known
    // default sentinel. raise() is async-signal-safe.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Signal-safe stderr write of a static byte slice. Returns
/// unconditionally — partial writes are accepted as best-effort because
/// we're already in a fatal path; better to lose a few bytes of the
/// banner than to spin on EINTR.
fn write_str(bytes: &[u8]) {
    // SAFETY: write(2) is async-signal-safe. fd 2 is the stderr the
    // process inherited; we don't redirect it elsewhere in main.rs.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            bytes.as_ptr().cast::<c_void>(),
            bytes.len(),
        );
    }
}

/// Signal-safe formatter for a small positive integer (signal number).
/// Writes the decimal digits in order to stderr. Avoids itoa / Display
/// because both touch the allocator on long enough integers and aren't
/// guaranteed signal-safe regardless. Signal numbers are always small
/// positive ints (POSIX caps at 64), so a two-digit fast path suffices.
fn format_signo(sig: c_int) {
    if !(0..=99).contains(&sig) {
        write_str(b"?");
        return;
    }
    // sig is in 0..=99 by the guard above; the digit/u8 casts are
    // guaranteed lossless. clippy can't infer the range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let sig_u8: u8 = sig as u8;
    if sig_u8 >= 10 {
        let buf = [b'0' + (sig_u8 / 10), b'0' + (sig_u8 % 10)];
        write_str(&buf);
    } else {
        let buf = [b'0' + sig_u8];
        write_str(&buf);
    }
}

/// Install the fatal-signal handler. Idempotent (re-installing replaces
/// the previous disposition) but should be called exactly once from
/// `main()` before any tokio runtime / plugin loading.
///
/// Returns `Ok(())` on success, or the first errno-bearing
/// [`std::io::Error`] from `sigaction(2)` if installation fails. A
/// failure is recoverable: the binary still runs, just without the
/// backtrace capture, so the caller can log the error and continue.
pub fn install() -> std::io::Result<()> {
    // SA_RESETHAND would auto-restore SIG_DFL on first delivery, but the
    // handler does that explicitly via signal() — we want the explicit
    // form so the order is unambiguous (capture → restore → raise),
    // and we want to keep the handler installed if we later add
    // SA_SIGINFO-based extensions that need the siginfo_t.
    //
    // SA_NODEFER lets a nested fatal signal during handler execution
    // proceed (rather than blocking until the handler returns); the
    // handler's re-raise depends on this not being blocked by the kernel
    // signal mask. Without SA_NODEFER, raise(sig) inside a handler for
    // sig would be queued until the handler returns — which works for
    // our usage (we want the signal to fire after the handler's last
    // statement), but SA_NODEFER makes the intent explicit and protects
    // against a future maintainer adding code after raise() that would
    // execute before the actual termination.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = handle_fatal_signal as *const () as libc::sighandler_t;
    sa.sa_flags = libc::SA_NODEFER;
    // SAFETY: sigemptyset() initialises sa_mask; on platforms where
    // sa_mask is already zeroed by `std::mem::zeroed()` (Linux), this
    // is redundant-safe — and it's portable across BSD where the
    // representation differs.
    unsafe {
        libc::sigemptyset(&raw mut sa.sa_mask);
    }

    for &sig in FATAL_SIGNALS {
        // SAFETY: sigaction() with a properly-initialised struct
        // sigaction; we pass null for the previous-action out param
        // because we don't need to restore at process exit.
        let rc = unsafe { libc::sigaction(sig, &raw const sa, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Signals to capture. Each one indicates a programming bug or
/// catastrophic state corruption — none of these are normal flow.
const FATAL_SIGNALS: &[c_int] = &[
    libc::SIGSEGV, // dereference of invalid pointer
    libc::SIGBUS,  // misaligned access / mmap'd file truncated
    libc::SIGILL,  // execution of illegal instruction
    libc::SIGFPE,  // arithmetic error (integer divide-by-zero etc.)
    libc::SIGABRT, // abort() — typically from a panic that crossed
                   // an extern "C" boundary, or libc detecting heap
                   // corruption via MALLOC_CHECK_ / fortified strings.
];

#[cfg(test)]
mod tests {
    use super::*;

    /// `install` succeeds in the typical test environment. We can't
    /// portably assert "the handler runs on SIGSEGV" from a unit test
    /// (the test process would die), so coverage of the actual capture
    /// path is integration-only — see the docs in this module's header
    /// for the failure mode this exists to debug.
    #[test]
    fn install_succeeds() {
        // Idempotency: calling install twice is fine.
        install().expect("first install");
        install().expect("second install (re-registration)");
    }

    /// The signal-number formatter handles the standard fatal-signal
    /// range without truncating. We can't directly observe stderr from
    /// the unit test, but the function returns silently on success.
    /// The real verification is the integration-shape output captured
    /// in the docs header.
    #[test]
    fn format_signo_accepts_two_digit_signals() {
        format_signo(libc::SIGSEGV); // 11 on Linux
        format_signo(libc::SIGABRT); // 6
        format_signo(libc::SIGFPE); // 8
        // Out-of-range signo doesn't panic.
        format_signo(-1);
        format_signo(200);
    }
}
