//! t-signal-delivery - the one thing every earlier signal test left undone:
//! actually running a handler.
//!
//! `t-rt-sigaction` records a disposition and `t-rt-sigprocmask` records a
//! mask, but both stop short of the moment a signal reaches a process that
//! installed a handler for it. That moment is what Phase 2 is about: the
//! kernel must build an `rt_sigframe` on the user stack, point the task's
//! return at the handler, and - when the handler returns through the
//! `SA_RESTORER`'s `rt_sigreturn` - resume the interrupted code untouched.
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
//! The masks here follow mpos's own convention, shared by `blocked`, the
//! pending set and `rt_sigpending`: bit *n* names signal *n*, not the libc's
//! bit *n-1*. The kernel never sees the libc spelling, so a test that speaks
//! to the raw syscall speaks the kernel's.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// What the handler saw, reset before each phase and read back after. A plain
/// `static mut` would let the compiler cache the main line's read of it across
/// the handler's write, since nothing on the main line "touches" the handler;
/// atomics are the honest way to share one word across the frame boundary.
static RAN: AtomicUsize = AtomicUsize::new(0);
static LAST_SIG: AtomicI32 = AtomicI32::new(0);
static LAST_CODE: AtomicI32 = AtomicI32::new(0);
static LAST_VALUE: AtomicU64 = AtomicU64::new(0);
static LAST_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn handler(sig: i32, info: *mut lin::SigInfo, _ctx: *mut u8) {
    RAN.fetch_add(1, Ordering::Relaxed);
    LAST_SIG.store(sig, Ordering::Relaxed);
    if !info.is_null() {
        let info = unsafe { &*info };
        LAST_CODE.store(info.code, Ordering::Relaxed);
        LAST_VALUE.store(info.value, Ordering::Relaxed);
        LAST_PID.store(info.pid, Ordering::Relaxed);
    }
}

/// musl's `__restore_rt`, for a kernel with no libc: the handler's `ret` lands
/// here, and the only thing to do is hand the frame back to `rt_sigreturn`.
/// It never returns - the kernel resumes the interrupted code instead.
extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

fn arm(signum: i32) {
    let act = lin::SigAction {
        handler: handler as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER,
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    let r = lin::rt_sigaction(signum, &act, core::ptr::null_mut());
    assert!(r == 0, "installing the handler for {signum} failed");
}

fn reset() {
    RAN.store(0, Ordering::Relaxed);
    LAST_SIG.store(0, Ordering::Relaxed);
    LAST_CODE.store(0, Ordering::Relaxed);
    LAST_VALUE.store(0, Ordering::Relaxed);
    LAST_PID.store(0, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-signal-delivery\n");
    let mut failed = false;
    arm(lin::SIGTERM);

    // --- Phase 2.1: the handler runs, and the interrupted call resumes ------
    //
    // `kill` to our own pid answers 0 on the way out; the syscall gate builds
    // the frame and enters the handler *before* that 0 reaches us, and
    // `rt_sigreturn` puts us back at the instruction after the `syscall`. So
    // by the time this line reads `r`, the handler has already run once and `r`
    // is the kill's own return - proof the frame was both entered and unwound.
    reset();
    let r = lin::kill(lin::getpid() as i32, lin::SIGTERM);
    lin::check_eq(&mut failed, b"kill to self returns 0", r, 0);
    lin::check_eq(
        &mut failed,
        b"the handler ran exactly once",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"the handler saw SIGTERM",
        LAST_SIG.load(Ordering::Relaxed) as isize,
        lin::SIGTERM as isize,
    );
    // A `kill` is an `SI_USER` record, and `si_pid` names the (us) sender.
    lin::check_eq(
        &mut failed,
        b"a kill reports si_code SI_USER",
        LAST_CODE.load(Ordering::Relaxed) as isize,
        lin::SI_USER as isize,
    );
    lin::check_true(
        &mut failed,
        b"a kill reports a nonzero si_pid",
        LAST_PID.load(Ordering::Relaxed) == lin::getpid() as i32,
    );

    // --- Phase 2.2: a blocked signal waits to be delivered ------------------
    //
    // Blocking SIGTERM, then raising it: nothing runs, and it shows as pending
    // in `rt_sigpending`. Unblocking is the act that delivers it - the frame
    // goes on the stack on the unblock's own return, so `RAN` is one by the
    // time that call is back.
    reset();
    let bit = 1u64 << lin::SIGTERM;
    let r = lin::rt_sigprocmask(lin::SIG_BLOCK, &bit, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"blocking SIGTERM succeeds", r, 0);
    let r = lin::kill(lin::getpid() as i32, lin::SIGTERM);
    lin::check_eq(&mut failed, b"kill of a blocked signal returns 0", r, 0);
    lin::check_eq(
        &mut failed,
        b"a blocked signal does not run its handler",
        RAN.load(Ordering::Relaxed) as isize,
        0,
    );
    let mut pending = 0u64;
    let r = lin::rt_sigpending(&mut pending, 8);
    lin::check_eq(&mut failed, b"rt_sigpending succeeds", r, 0);
    lin::check_true(
        &mut failed,
        b"a blocked signal is reported pending",
        pending & bit != 0,
    );

    // The unblock call's own return is where the handler runs.
    let r = lin::rt_sigprocmask(lin::SIG_UNBLOCK, &bit, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"unblocking SIGTERM succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"unblocking delivers the held signal",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );

    // --- Phase 2.3: a sigqueue payload reaches the handler ------------------
    //
    // `rt_sigqueueinfo` with an `SI_QUEUE` code and a value word: the handler
    // must read both back, which is the part a bitmask-only queue could not
    // say. `si_pid` here is the sender (us) again.
    reset();
    let mut info = lin::SigInfo::zero();
    info.signo = lin::SIGTERM;
    info.code = lin::SI_QUEUE;
    info.value = 0x1122_3344_5566_7788;
    let r = lin::rt_sigqueueinfo(lin::getpid(), lin::SIGTERM, &info);
    lin::check_eq(&mut failed, b"sigqueue to self returns 0", r, 0);
    lin::check_eq(
        &mut failed,
        b"the sigqueue handler ran once",
        RAN.load(Ordering::Relaxed) as isize,
        1,
    );
    lin::check_eq(
        &mut failed,
        b"the handler read si_code SI_QUEUE",
        LAST_CODE.load(Ordering::Relaxed) as isize,
        lin::SI_QUEUE as isize,
    );
    lin::check_true(
        &mut failed,
        b"the sigqueue payload survived intact",
        LAST_VALUE.load(Ordering::Relaxed) == 0x1122_3344_5566_7788,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-signal-delivery: panicked (a handler assertion failed)\n")
}
