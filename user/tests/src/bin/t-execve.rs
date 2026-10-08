//! t-execve - one test for execve, the first syscall in a musl binary's strace.
//!
//! `execve` replaces the calling process: the old image is gone, the new one
//! starts at its entry point, and **the call never returns**. That makes the
//! success case impossible to observe from inside the process that calls it, so
//! the test splits in two.
//!
//! The failures are checked in-process, because those *do* return: a path that
//! names nothing is ENOENT, and a real file with no execute bit anywhere is
//! EACCES - the DAC check answers before the loader sees the bytes, root
//! included, which is Linux's `CAP_DAC_OVERRIDE` sentence.
//!
//! The success is checked through a child. The child calls `execve` and the
//! parent reaps it; if the call had returned in the child, the child would exit
//! with 127 instead, and the parent's status check would catch it.
//!
//! # The migration
//!
//! `libc::execve` is the door on both machines: the crate *does* carry an
//! `execve`, but in `rustix::runtime`, which is doc-hidden, explicitly
//! experimental and outside the enabled feature set (`all-apis` does not list
//! `runtime`) - clause (a), the reason `t-wait4` gives for the same call. The
//! child-side machinery is batch 18's: `libc::fork` for the creation, the raw
//! `SYS_exit_group` for the code a child leaves with (a forked std child holds a
//! copy of the parent's buffered stdout, and only a non-flushing exit leaves it
//! behind), and `rustix::process::waitpid` - `wait4` with a null `rusage` - with
//! `WaitStatus::exited`/`exit_status` for the two macros the no-std version
//! called.
//!
//! One fixture moved and the claim did not: the no-std version's
//! non-executable real file was `/etc/motd`, which the second machine's payload
//! does not carry at all (batch 3 measured its `/etc`). This test now makes its
//! own under `/tmp`, and the mode - `0644 & !umask`, no execute bit for anyone -
//! is part of the claim rather than a property of the image, which is the shape
//! batch 5's `t-access` gives the same kind of probe.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, unlinkat};
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, WaitStatus, waitpid};
use std::ffi::CStr;

/// The target the child replaces itself with: it exits 0 without a word.
const PROG: &[u8] = b"/bin/true\0";
const ARG0: &[u8] = b"true\0";
/// A path that names nothing.
const MISSING: &[u8] = b"/no/such\0";
const MISSING_ARG0: &[u8] = b"nothing\0";
/// The real file with no execute bit anywhere - this test's own, see above.
const DATA: &str = "/tmp/t-execve_data";
/// The same name as the execve door takes it.
const DATA_C: &[u8] = b"/tmp/t-execve_data\0";
const DATA_ARG0: &[u8] = b"data\0";

/// What the child leaves with if `execve` came back, which can only mean it
/// failed. Distinct from any code `/bin/true` would produce.
const NO_EXEC: i32 = 127;

/// A NUL-terminated literal as a `CStr`.
fn cstr(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(bytes).expect("each literal above ends in one NUL")
}

/// `execve(path, [arg0, NULL], [NULL])`, answering the errno it was refused
/// with. There is no success to return: a successful `execve` replaces the image
/// and never comes back, so every return from here is a refusal.
fn execve_refused(prog: &CStr, arg0: &CStr) -> Errno {
    let argv: [*const libc::c_char; 2] = [arg0.as_ptr(), core::ptr::null()];
    let envp: [*const libc::c_char; 1] = [core::ptr::null()];
    unsafe {
        let _ = libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
    }
    Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// Leave through `exit_group`, the door that cannot flush the stdout copy this
/// child inherited.
fn leave(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit_group, code as libc::c_long);
        libc::_exit(code)
    }
}

/// The blocking `wait4(pid)`: `Some(status)` when that child was the one
/// collected, `None` when the call answered anything else.
fn collect(pid: i32) -> Option<WaitStatus> {
    let want = Pid::from_raw(pid);
    match waitpid(want, WaitOptions::empty()) {
        Ok(Some((got, status))) if Some(got) == want => Some(status),
        _ => None,
    }
}

fn main() {
    println!("t-execve");
    let mut r = Report::new();

    // The fixture first: a real file with no execute bit anywhere. Removing any
    // leftover makes the mode this test's own rather than a previous run's.
    let _ = unlinkat(CWD, DATA, AtFlags::empty());
    let made = openat(
        CWD,
        DATA,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    );
    r.check("a real file with no execute bit was made", made.is_ok());
    drop(made);

    // A missing path: ENOENT, the errno the man page pins for a file that is not
    // there.
    r.check(
        "execve on a missing path is ENOENT",
        execve_refused(cstr(MISSING), cstr(MISSING_ARG0)) == Errno::NOENT,
    );

    // A file that exists but is not marked executable: Linux's
    // `CAP_DAC_OVERRIDE` grants root execute only where some execute bit
    // exists, and this file has none for anyone, so the DAC check refuses it
    // with `EACCES` before the loader ever runs - root included. The old
    // outright pass for uid 0 let this reach the loader and answer `ENOEXEC`;
    // that reading invented a permission nobody set, which is the shape a
    // caller cannot fall back around.
    r.check(
        "execve of non-executable data is EACCES even for root",
        execve_refused(cstr(DATA_C), cstr(DATA_ARG0)) == Errno::ACCESS,
    );

    // The success case, seen from the parent: the child becomes /bin/true and is
    // reaped with 0. A child that had returned from execve would have left 127.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        r.check("fork for the execve child succeeds", false);
        r.finish()
    }
    if pid == 0 {
        let _ = execve_refused(cstr(PROG), cstr(ARG0));
        // Reached only when the exec did not happen.
        leave(NO_EXEC);
    }
    r.check("fork for the execve child succeeds", pid > 0);

    let status = collect(pid);
    r.check("the exec'd child is reaped by its pid", status.is_some());
    r.check(
        "execve replaced the child: /bin/true ran and exited 0",
        status.and_then(WaitStatus::exit_status) == Some(0),
    );

    let _ = unlinkat(CWD, DATA, AtFlags::empty());
    r.finish()
}
