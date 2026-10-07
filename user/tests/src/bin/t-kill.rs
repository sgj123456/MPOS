//! t-kill - one test for kill, with signal 0 as the only signal this test
//! ever puts on the wire.
//!
//! The man page pins: kill(pid, 0) performs the permission and existence
//! checks but delivers nothing - the classic liveness probe - ESRCH for a
//! pid no process answers to, and EINVAL for a signal number outside the
//! range the kernel names — Linux's `_NSIG`, so `1..=64` including the
//! real-time signals (stage B). The range is checked before anything else, so
//! the refused numbers here (65, 999) are never delivered. No valid nonzero
//! signal is sent in this test: with no handler machinery a delivered signal
//! is fatal, and the test process has work to finish.
//!
//! Pure `rustix::process` for the identity calls, and the raw door for the
//! three claims its vocabulary cannot name: signal 0 (no `Signal` variant),
//! 65 and 999 past `_NSIG`. That is clause (c) - a plain enum with nowhere
//! to put an out-of-vocabulary word - measured the same way batch 8's
//! whence and advice were. The numbers come from libc's per-machine table
//! (`SYS_kill`: 62 / 129).

use mpos_user_tests::Report;
use rustix::process::{Signal, getpid, kill_process};

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-kill");
    let mut r = Report::new();
    let me = getpid();

    // The existence probe: signal 0 delivers nothing. rustix's `Signal`
    // vocabulary has no zero ("the none of the enum"), so this first claim -
    // like the two refused numbers below - rides the raw door, which is the
    // call itself with libc's per-machine number (62 / 129).
    let me_raw = me.as_raw_nonzero().get();
    let r0 = unsafe { libc::syscall(libc::SYS_kill, me_raw, 0) as isize };
    r.check("kill(self, 0) probes existence and succeeds", r0 == 0);

    // A pid past the whole task table names no process: ESRCH.
    let r_srch = unsafe { libc::syscall(libc::SYS_kill, 99999 as libc::pid_t, 0) as isize };
    r.check(
        "kill on a pid nobody owns is ESRCH",
        refused(r_srch, libc::ESRCH),
    );

    // The signal number is range-checked before anything else. The kernel
    // names `1..=64` (`_NSIG`) — the real-time range is valid from stage B
    // on — so the refused numbers are 65 (just past it) and 999 (far past
    // it), not 32. These two go raw for clause (c): rustix's `Signal` is a
    // plain enum whose vocabulary cannot name 65 or 999 at all.
    let r65 = unsafe { libc::syscall(libc::SYS_kill, me_raw, 65) as isize };
    r.check(
        "kill(self, 65) past _NSIG is EINVAL",
        refused(r65, libc::EINVAL),
    );
    let r999 = unsafe { libc::syscall(libc::SYS_kill, me_raw, 999) as isize };
    r.check("kill(self, 999) is EINVAL", refused(r999, libc::EINVAL));

    // The control that the raw door really reaches the dispatch: a *named*
    // signal through the same raw call answers 0 (SIG_DFL delivery of
    // SIGCONT kills nothing here).
    let r_cont = unsafe { libc::syscall(libc::SYS_kill, me_raw, libc::SIGCONT) as isize };
    r.check(
        "the raw door carries a real signal too (SIGCONT answers 0)",
        r_cont == 0,
    );
    // And the rustix door agrees about the same named signal, so the probe
    // above is measuring the kernel and not the spelling.
    r.check(
        "rustix's kill_process agrees (SIGCONT answers Ok)",
        kill_process(me, Signal::CONT).is_ok(),
    );

    r.finish()
}
