//! t-rt-sigtimedwait - `rt_sigtimedwait`: take a signal off the pending set
//! without running a handler for it.
//!
//! This is the door a daemon's main loop walks through. busybox `init` blocks
//! everything but `SIGCHLD` and parks here, so a child's exit is noticed the
//! moment the signal arrives rather than on a poll timer - which is why the
//! check below forks a child and waits for its `SIGCHLD` rather than only
//! handing the call a set it filled in itself.
//!
//! The call was gated `not(aarch64)`, and that gate was of the same stale kind
//! as `process_vm_*`'s and the `ifreq` link ioctls': nothing in it is the first
//! machine's. It reads the pending set and the blocked mask, sleeps on the tick
//! clock, and fills in the *generic* `siginfo_t` - 128 bytes on both machines,
//! `si_pid`/`si_uid` at 16/20, the `union sigval` at 24. It consults no
//! register frame at all, which is what the `ptrace` and signal-*handler* gates
//! beside it are for. The second machine's number table named
//! `RT_SIGTIMEDWAIT` (137 against x86_64's 128) all along.
//!
//! **What is pinned.** The signal comes back by number, the `siginfo_t` carries
//! the sender (`si_signo`, `si_code` = `SI_USER`, `si_pid` = the raiser), the
//! call *consumes* what it returns (a second look with a zero timeout is
//! `EAGAIN`), the `SIGCHLD` of a child this process forked arrives the same way
//! with a NULL timeout - the `init` pattern, and the only shape here that
//! blocks at all - and the two refusals a caller can reach: `sigsetsize` other
//! than 8 is `EINVAL`, and a NULL timeout with nothing to take is `EAGAIN`.
//!
//! Like `t-process-vm-libc` this one speaks `libc` rather than `rustix`: the
//! only `rt_sigtimedwait` binding `rustix` has is private to its backend, so
//! the call is made through `libc::syscall` with the number from `libc::SYS_*`
//! - a maintained per-arch table, which is what this crate's `rustix` rule
//! exists to ensure.

use std::io;
use std::os::raw::{c_int, c_long, c_void};
use std::process::exit;
use std::ptr;

// The numbers that are the ABI's rather than a machine's: the how-value of
// `rt_sigprocmask`, the two signals this test uses, and the errnos.
const SIG_BLOCK: c_int = 0;
const SIGUSR1: c_int = 10;
const SIGCHLD: c_int = 17;
const SI_USER: i32 = 0;
const EAGAIN: i32 = 11;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;

/// The wire's `sigset_t` as `rt_sigprocmask`/`rt_sigtimedwait` take it: 8 bytes,
/// signal `n` in bit `n - 1`. The kernel converts it with the same `from_wire`
/// both calls go through.
fn set_of(signals: &[c_int]) -> u64 {
    signals
        .iter()
        .fold(0u64, |set, &n| set | (1 << (n as u32 - 1)))
}

/// The report shape the std-side tests share.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: i32, want: i32) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }

    fn check_call(&mut self, name: &str, got: Result<c_int, i32>, want: c_int) {
        self.check(
            &format!("{name} (got {got:?}, want {want})"),
            got == Ok(want),
        );
    }
}

/// `rt_sigprocmask(SIG_BLOCK, ...)`: hold `signals` pending instead of
/// delivering them, which is what makes the wait below observably a wait.
fn block(signals: &[c_int]) -> Result<c_int, i32> {
    let set = set_of(signals);
    // SAFETY: the set is a word this process owns, and the old-set pointer is
    // null, which the ABI reads as "not interested".
    let rc = unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            SIG_BLOCK as c_long,
            &set as *const u64,
            ptr::null::<u64>(),
            8 as c_long,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc as c_int)
    }
}

/// `rt_sigtimedwait`, with the size left to the caller so the refusal can be
/// reached.
fn sigtimedwait(set: &u64, info: *mut u8, timeout: *const i64, size: usize) -> Result<c_int, i32> {
    // SAFETY: `set` and `info` are buffers this process owns, `info` is at
    // least the 128 bytes the ABI writes, and `timeout` is either null or a
    // two-word `timespec` this process owns.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_rt_sigtimedwait,
            set as *const u64,
            info,
            timeout,
            size as c_long,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    } else {
        Ok(rc as c_int)
    }
}

/// The `si_pid` field of the `siginfo_t` the call filled in.
fn si_pid(info: &[u8; 128]) -> i32 {
    i32::from_ne_bytes(info[16..20].try_into().unwrap())
}

fn si_signo(info: &[u8; 128]) -> i32 {
    i32::from_ne_bytes(info[..4].try_into().unwrap())
}

fn si_code(info: &[u8; 128]) -> i32 {
    i32::from_ne_bytes(info[8..12].try_into().unwrap())
}

fn main() {
    println!("t-rt-sigtimedwait");
    let mut r = Report { failed: false };

    // SAFETY: no arguments, no preconditions.
    let me = unsafe { libc::getpid() };

    // Nothing is pending yet, and a zero timeout is "check once": `EAGAIN`,
    // not a block and not an error a caller would branch on differently.
    let nothing = set_of(&[SIGUSR1]);
    let zero = [0i64; 2];
    let mut info = [0u8; 128];
    r.check_errno(
        "an empty pending set with a zero timeout is EAGAIN",
        sigtimedwait(&nothing, info.as_mut_ptr(), zero.as_ptr(), 8).unwrap_err(),
        EAGAIN,
    );

    // Block the signal first, so raising it leaves it *pending* rather than
    // delivered - with no handler installed, its default action is to end the
    // process, and this test is not about that path.
    r.check_call("SIGUSR1 can be blocked", block(&[SIGUSR1]), 0);
    // SAFETY: signalling this process, which has nothing to lose by it.
    let raised = unsafe { libc::kill(me, SIGUSR1) };
    r.check("raising a blocked signal succeeds", raised == 0);

    // The wait takes it, and by number.
    r.check_call(
        "rt_sigtimedwait returns the signal it took",
        sigtimedwait(&nothing, info.as_mut_ptr(), zero.as_ptr(), 8),
        SIGUSR1,
    );
    r.check(
        &format!("and the siginfo names it (si_signo {})", si_signo(&info)),
        si_signo(&info) == SIGUSR1,
    );
    // `SI_USER` is the code every `kill(2)` carries, and it is what a waiter
    // branches on to tell a deliberate signal from the hardware's.
    r.check(
        &format!("with si_code SI_USER (got {})", si_code(&info)),
        si_code(&info) == SI_USER,
    );
    r.check(
        &format!("and si_pid the sender (got {})", si_pid(&info)),
        si_pid(&info) == me,
    );

    // It was *consumed*: the same call finds nothing now. A waiter that got the
    // signal every time it looked would never make progress.
    r.check_errno(
        "the signal it returned is no longer pending",
        sigtimedwait(&nothing, info.as_mut_ptr(), zero.as_ptr(), 8).unwrap_err(),
        EAGAIN,
    );

    // The `init` pattern: block SIGCHLD, fork, and wait on it with a NULL
    // timeout. This is the only shape here that blocks, which is the point -
    // a wait that returned before the child existed would be a different call.
    let child_set = set_of(&[SIGCHLD]);
    r.check_call("SIGCHLD can be blocked", block(&[SIGCHLD]), 0);
    // SAFETY: as above; the child exits immediately and allocates nothing.
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(0) }
    }
    r.check("fork gave the parent a child pid", child > 0);
    if child > 0 {
        let mut child_info = [0u8; 128];
        r.check_call(
            "the child's SIGCHLD arrives without a timeout",
            sigtimedwait(&child_set, child_info.as_mut_ptr(), ptr::null(), 8),
            SIGCHLD,
        );
        r.check(
            "and it is reported as SIGCHLD",
            si_signo(&child_info) == SIGCHLD,
        );
        let mut status = 0 as c_int;
        // SAFETY: `status` is a word this process owns and `child` is a pid it
        // forked; the wait cannot block, the child has already gone.
        let reaped = unsafe { libc::waitpid(child, &mut status, 0) };
        r.check("and the child is there to be reaped", reaped == child);
    }

    // `sigsetsize` is part of the ABI: a caller that names the wrong width is
    // refused rather than half-served. The wire's set is 8 bytes on both
    // machines, which is why this is a constant here and not a per-machine one.
    r.check_errno(
        "a sigsetsize other than 8 is EINVAL",
        sigtimedwait(&nothing, info.as_mut_ptr(), zero.as_ptr(), 4).unwrap_err(),
        EINVAL,
    );

    // A null set pointer is not an empty set. This is where the call parts
    // company with `rt_sigprocmask`, which reads a null set as "just tell me
    // the mask I have": a wait with no set has nothing to wait for, and Linux
    // answers the copy failure rather than waiting for nothing.
    let mut null_buffer = [0u8; 128];
    // SAFETY: the pointer is deliberately null, which is the case under test;
    // the info buffer is a real one and the call cannot write to it.
    let refused = unsafe {
        libc::syscall(
            libc::SYS_rt_sigtimedwait,
            ptr::null::<u64>(),
            null_buffer.as_mut_ptr().cast::<c_void>(),
            zero.as_ptr(),
            8 as c_long,
        )
    };
    // Read the errno before anything else can make a syscall of its own.
    let refused_errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    r.check("a null set pointer is refused", refused < 0);
    r.check_errno("and that answer is EFAULT", refused_errno, EFAULT);

    if r.failed {
        println!("t-rt-sigtimedwait: FAILED");
        exit(1);
    }
    println!("t-rt-sigtimedwait: all passed");
}
