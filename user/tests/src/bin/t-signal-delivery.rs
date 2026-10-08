//! t-signal-delivery - the one thing every earlier signal test left undone:
//! actually running a handler.
//!
//! `t-rt-sigaction` records a disposition and `t-rt-sigprocmask` records a
//! mask, but both stop short of the moment a signal reaches a process that
//! installed a handler for it. That moment is what Phase 2 is about: the
//! kernel must build an `rt_sigframe` on the user stack, point the task's
//! return at the handler, and - when the handler returns through the
//! restorer's `rt_sigreturn` - resume the interrupted code untouched.
//! Until this runs, "recorded" and "honoured" are the same claim in the docs
//! and different things in the kernel.
//!
//! Three things are pinned, in order:
//!
//!   * a delivered signal runs its handler exactly once and execution resumes
//!     with the syscall's own return value intact (Phase 2.1);
//!   * a blocked signal is not delivered until it is unblocked, and the
//!     unblocking syscall's own return is what runs it (Phase 2.2);
//!   * a `sigqueue` payload reaches the handler as a real `siginfo_t` -
//!     `si_code == SI_QUEUE` and the caller's `si_value` word - where a bare
//!     `kill` reads back `SI_USER` (Phase 2.3).
//!
//! The signal is `SIGTERM`, and that is a fixture choice rather than a
//! preference: the runtime this program runs inside has already touched
//! `SIGPIPE` (ignored) and `SIGSEGV`/`SIGBUS` (armed on the guard stack), so a
//! number nothing has an opinion about is what makes "the disposition read
//! back" a claim about this program's own install. `t-signal-frame` uses
//! `SIGUSR1` for the same reason.
//!
//! # The doors
//!
//! The install, the send and the mask are the named calls - `libc::sigaction`
//! (whose musl build supplies the restorer word itself, measured: its
//! `__libc_sigaction` ORs `SA_RESTORER` into the flags and fills the slot on
//! both machines), `libc::kill` and `libc::sigqueue` (musl's `SI_QUEUE`
//! record, `si_code` at byte 8 and the value at 24, from the same archive), and
//! `libc::sigprocmask`. The handler's record is read through the machine's own
//! `libc::siginfo_t` accessors, which is where `t-waitid` put the offsets.
//!
//! The pending word is the one call that stays on the raw door: the claim is
//! *the wire's* 8-byte `sigset_t` with `sigsetsize` as the caller names it -
//! `pending & (1 << (SIGTERM - 1))`, the spelling `t-rt-sigprocmask` pins - and
//! no wrapper hands over that word. `libc::sigpending` fills the machine's own
//! 128-byte set and blocks-and-restores around the call, so the number the
//! kernel was given, and the bit it wrote, are both outside it (clause (c), and
//! the same reason `t-rt-sigprocmask` is raw throughout). The write itself is
//! over a seeded word, because a query that answers 0 and writes nothing would
//! hand the caller its own sentinel back.

use mpos_user_tests::Report;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// What the handler saw, reset before each phase and read back after. A plain
/// `static mut` would let the compiler cache the main line's read of it across
/// the handler's write, since nothing on the main line "touches" the handler;
/// atomics are the honest way to share one word across the frame boundary.
static RAN: AtomicUsize = AtomicUsize::new(0);
static LAST_SIG: AtomicI32 = AtomicI32::new(0);
static LAST_CODE: AtomicI32 = AtomicI32::new(0);
static LAST_VALUE: AtomicU64 = AtomicU64::new(0);
static LAST_PID: AtomicI32 = AtomicI32::new(0);

const SEED: u64 = 0x5a5a_5a5a_5a5a_5a5a;

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    RAN.fetch_add(1, Ordering::Relaxed);
    LAST_SIG.store(sig, Ordering::Relaxed);
    if !info.is_null() {
        // SAFETY: `info` is the record the kernel filled in for a `SA_SIGINFO`
        // handler; the fields and accessors are the machine's own.
        unsafe {
            LAST_CODE.store((*info).si_code, Ordering::Relaxed);
            LAST_VALUE.store(
                (*info).si_value().sival_ptr as usize as u64,
                Ordering::Relaxed,
            );
            LAST_PID.store((*info).si_pid(), Ordering::Relaxed);
        }
    }
}

/// Install the handler for `sig` through the named call. musl fills the
/// restorer slot and sets `SA_RESTORER` itself, so the record here is the
/// user-space one and no trampoline is this program's business.
fn arm(sig: i32) {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves.
    let rc = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
    assert!(rc == 0, "installing the handler for {sig} failed");
}

/// `rt_sigpending(set, sigsetsize)` with the caller's own 8-byte word, seeded
/// so a call that wrote nothing is visible.
fn sigpending_word() -> u64 {
    let mut word = SEED;
    // SAFETY: an 8-byte word this process owns and the ABI's own size.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_rt_sigpending,
            &mut word as *mut u64 as usize as libc::c_long,
            8 as libc::c_long,
        )
    };
    assert!(rc == 0, "rt_sigpending failed");
    word
}

fn block(sig: i32) -> bool {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a set this process owns.
    unsafe { libc::sigaddset(&mut set, sig) };
    // SAFETY: as above; no old set is wanted.
    unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0 }
}

fn unblock(sig: i32) -> bool {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a set this process owns.
    unsafe { libc::sigaddset(&mut set, sig) };
    // SAFETY: as above; no old set is wanted.
    unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) == 0 }
}

fn reset() {
    RAN.store(0, Ordering::Relaxed);
    LAST_SIG.store(0, Ordering::Relaxed);
    LAST_CODE.store(0, Ordering::Relaxed);
    LAST_VALUE.store(0, Ordering::Relaxed);
    LAST_PID.store(0, Ordering::Relaxed);
}

fn main() {
    println!("t-signal-delivery");
    let mut r = Report::new();
    // SAFETY: no arguments, no preconditions.
    let me = unsafe { libc::getpid() };
    arm(libc::SIGTERM);

    // --- Phase 2.1: the handler runs, and the interrupted call resumes ------
    //
    // `kill` to our own pid answers 0 on the way out; the syscall gate builds
    // the frame and enters the handler *before* that 0 reaches us, and
    // `rt_sigreturn` puts us back at the instruction after the call. So by the
    // time this line reads `rc`, the handler has already run once and `rc` is
    // the kill's own return - proof the frame was both entered and unwound.
    reset();
    // SAFETY: signalling this process, which has nothing to lose by it.
    let rc = unsafe { libc::kill(me, libc::SIGTERM) };
    r.check("kill to self returns 0", rc == 0);
    r.check(
        "the handler ran exactly once",
        RAN.load(Ordering::Relaxed) as isize == 1,
    );
    r.check(
        "the handler saw SIGTERM",
        LAST_SIG.load(Ordering::Relaxed) as isize == libc::SIGTERM as isize,
    );
    // A `kill` is an `SI_USER` record, and `si_pid` names the (us) sender.
    r.check(
        "a kill reports si_code SI_USER",
        LAST_CODE.load(Ordering::Relaxed) == libc::SI_USER,
    );
    r.check(
        "a kill reports a nonzero si_pid",
        LAST_PID.load(Ordering::Relaxed) == me,
    );

    // --- Phase 2.2: a blocked signal waits to be delivered ------------------
    //
    // Blocking SIGTERM, then raising it: nothing runs, and it shows as pending
    // in `rt_sigpending`. Unblocking is the act that delivers it - the frame
    // goes on the stack on the unblock's own return, so `RAN` is one by the
    // time that call is back.
    reset();
    // The wire's mask, as a libc spells it: signal *n* in bit *n-1* (the
    // convention `t-rt-sigprocmask` pins). The kernel converts at the call.
    let bit = 1u64 << (libc::SIGTERM - 1);
    r.check("blocking SIGTERM succeeds", block(libc::SIGTERM));
    // SAFETY: signalling this process while the signal is held, not delivered.
    let rc = unsafe { libc::kill(me, libc::SIGTERM) };
    r.check("kill of a blocked signal returns 0", rc == 0);
    r.check(
        "a blocked signal does not run its handler",
        RAN.load(Ordering::Relaxed) == 0,
    );
    let pending = sigpending_word();
    r.check("rt_sigpending succeeds", pending != SEED);
    r.check("a blocked signal is reported pending", pending & bit != 0);

    // The unblock call's own return is where the handler runs.
    r.check("unblocking SIGTERM succeeds", unblock(libc::SIGTERM));
    r.check(
        "unblocking delivers the held signal",
        RAN.load(Ordering::Relaxed) as isize == 1,
    );

    // --- Phase 2.3: a sigqueue payload reaches the handler ------------------
    //
    // `sigqueue` with its `SI_QUEUE` code and a value word: the handler must
    // read both back, which is the part a bitmask-only queue could not say.
    // `si_pid` here is the sender (us) again.
    reset();
    r.check(
        "sigqueue to self returns 0",
        queue_self(libc::SIGTERM, 0x1122_3344_5566_7788),
    );
    r.check(
        "the sigqueue handler ran once",
        RAN.load(Ordering::Relaxed) as isize == 1,
    );
    r.check(
        "the handler read si_code SI_QUEUE",
        LAST_CODE.load(Ordering::Relaxed) == libc::SI_QUEUE,
    );
    r.check(
        "the sigqueue payload survived intact",
        LAST_VALUE.load(Ordering::Relaxed) == 0x1122_3344_5566_7788,
    );

    r.finish()
}

/// `sigqueue` to this process, answering whether the libc reported success.
fn queue_self(sig: i32, value: u64) -> bool {
    let val = libc::sigval {
        sival_ptr: value as usize as *mut libc::c_void,
    };
    // SAFETY: signalling this process, whose handler for `sig` is installed.
    unsafe { libc::sigqueue(libc::getpid(), sig, val) == 0 }
}
