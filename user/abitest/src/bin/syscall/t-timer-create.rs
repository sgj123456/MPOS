//! t-timer-create - the POSIX interval timer family, in the shape a libc
//! actually sends.
//!
//! The five calls are `timer_create`, `timer_settime`, `timer_gettime`,
//! `timer_getoverrun` and `timer_delete`, and the shape that matters is the one
//! musl builds for a *callback* timer: `timer_create(CLOCK_MONOTONIC, ...,
//! SIGEV_THREAD)` becomes a helper thread blocked in a wait, plus a
//! `SIGEV_THREAD_ID` timer aimed at that thread's tid carrying `SIGRTMIN` -
//! which is why this test creates a real thread, blocks `SIGRTMIN` in it, and
//! waits for the expiry with `rt_sigtimedwait`, exactly as musl's helper does.
//!
//! Pinned here:
//!
//! * a `clockid` this kernel has no clock for is `EINVAL` (the CPU-time
//!   clocks, above all);
//! * a `sigev_notify` outside `{NONE, SIGNAL, THREAD_ID}` is `EINVAL` -
//!   including `SIGEV_THREAD` (2), which is a libc value that never reaches
//!   the kernel;
//! * a signal number outside `1..=SIGRTMIN` is `EINVAL`, and so is a
//!   `SIGEV_THREAD_ID` with no tid;
//! * a `SIGEV_NONE` timer counts down, reports its remaining time through
//!   `timer_gettime`, does not fire a signal, and a one-shot that expired
//!   reports zero remaining rather than an error;
//! * a `SIGEV_THREAD_ID` timer carrying `SIGRTMIN` wakes the named thread's
//!   `rt_sigtimedwait` with `si_code == SI_TIMER` and the `sigval` the creator
//!   set - the whole of what musl's helper checks before calling a callback;
//! * `timer_delete` frees the id, and a deleted id is `EINVAL` afterwards;
//! * `TIMER_ABSTIME` is refused: this kernel keeps one clock and has no
//!   absolute domain to measure a deadline against, the same answer
//!   `clock_nanosleep` gives.
//!
//! `SIGRTMIN` (32) is the one real-time number this kernel queues; a
//! *disposition* for it is still `EINVAL`, which the test leaves alone.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// What the waiting thread saw: an `si_code` (0 means "nothing yet"), and the
/// `sigval` word the timer carried.
static THREAD_CODE: AtomicI32 = AtomicI32::new(0);
static THREAD_VALUE: AtomicU64 = AtomicU64::new(0);
static THREAD_RAN: AtomicI32 = AtomicI32::new(0);

/// The payload the timer must carry to whoever waits for it.
const PAYLOAD: u64 = 0x7469_6d65_7231_3233;

/// `SIGRTMIN`'s bit in the wire's mask: signal *n* is bit *n-1*.
const RTMIN_BIT: u64 = 1u64 << (lin::SIGRTMIN - 1);

/// The thread's whole life: wait on `rt_sigtimedwait` for `SIGRTMIN` - the
/// exact call musl's `SIGEV_THREAD` helper parks in - and record what arrived.
/// The mask was inherited from the creator, which blocked the signal before
/// the thread existed, so there is no window in which the signal could run a
/// default action instead.
extern "C" fn waiter(_arg: usize) -> ! {
    let set = RTMIN_BIT;
    let mut info = lin::SigInfo::zero();
    let r = lin::rt_sigtimedwait(&set, &mut info, core::ptr::null::<lin::TimeSpec>());
    if r == lin::SIGRTMIN as isize {
        THREAD_VALUE.store(info.value, Ordering::SeqCst);
        THREAD_CODE.store(
            if info.code == 0 { -1 } else { info.code },
            Ordering::SeqCst,
        );
    } else {
        THREAD_CODE.store(-1000, Ordering::SeqCst);
    }
    THREAD_RAN.store(1, Ordering::SeqCst);
    lin::exit(0)
}

/// A `SIGEV_NONE` timer armed for `ms`, then read back: how many whole
/// milliseconds of `remaining` it reported, or a negative errno.
fn none_timer_remaining_after(arm_ms: i64, wait_ms: u64) -> (isize, i64, isize) {
    let mut event = lin::SigEvent::new();
    event.notify = lin::SIGEV_NONE;
    let mut id: i32 = -1;
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    if r != 0 {
        return (r, -1, -2);
    }
    let mut spec = lin::Itimerspec::new();
    spec.value.tv_sec = arm_ms / 1000;
    spec.value.tv_nsec = (arm_ms % 1000) * 1_000_000;
    let r = lin::timer_settime(id, 0, &spec, core::ptr::null_mut());
    if r != 0 {
        return (r, -1, -2);
    }
    lin::sleep_ms(wait_ms);
    let mut now = lin::Itimerspec::new();
    let r = lin::timer_gettime(id, &mut now);
    let overrun = lin::timer_getoverrun(id);
    let remaining_ms = now.value.tv_sec * 1000 + now.value.tv_nsec / 1_000_000;
    (r, remaining_ms, overrun)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-timer-create\n");
    let mut failed = false;
    let mut id: i32 = -1;

    // --- the refusals -------------------------------------------------------
    let mut event = lin::SigEvent::new();
    event.notify = lin::SIGEV_NONE;
    // Measured on Linux 6.x: the CPU-time clocks answer EOPNOTSUPP ("this
    // clock exists, this timer cannot be built on it"), not EINVAL. This
    // kernel refuses them for the reason that errno names - no per-task CPU
    // accounting - rather than accepting a timer that would never fire.
    let r = lin::timer_create(2, &event, &mut id); // CLOCK_PROCESS_CPUTIME_ID
    lin::check_eq(
        &mut failed,
        b"a clock with no accounting behind it is EOPNOTSUPP",
        r,
        lin::EOPNOTSUPP,
    );

    // A clock id that names no clock at all is EINVAL, as on Linux.
    let r = lin::timer_create(99, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"a clock id no clock has is EINVAL",
        r,
        lin::EINVAL,
    );

    event.notify = 2; // SIGEV_THREAD: a libc value the kernel never sees
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"SIGEV_THREAD is EINVAL at the kernel",
        r,
        lin::EINVAL,
    );

    event.notify = lin::SIGEV_SIGNAL;
    event.signo = 0;
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"a signal number of 0 is EINVAL",
        r,
        lin::EINVAL,
    );

    // Linux accepts 1..=64 here; stage B opened the real-time range, so a
    // number inside it names a valid timer signal, and only past `_NSIG` is
    // refused — rather than a number up to 64 being accepted and then silently
    // undeliverable for want of a disposition slot.
    event.signo = 40; // inside SIGRTMIN..=SIGRTMAX
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"a real-time signal number (40) is accepted",
        r,
        0,
    );

    event.signo = 65; // past _NSIG
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"a signal number past _NSIG (64) is EINVAL",
        r,
        lin::EINVAL,
    );

    event.notify = lin::SIGEV_THREAD_ID;
    event.signo = lin::SIGRTMIN;
    event.tid = 0;
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &event, &mut id);
    lin::check_eq(
        &mut failed,
        b"SIGEV_THREAD_ID without a tid is EINVAL",
        r,
        lin::EINVAL,
    );

    // --- a NULL sigevent is SIGEV_SIGNAL + SIGALRM -------------------------
    //
    // Measured on Linux 6.x at the raw-syscall level: a NULL `evp` creates a
    // timer whose expiry queues `SIGALRM` - not a silent `SIGEV_NONE` timer,
    // which is what reading only "create succeeded" would suggest. Blocked
    // `SIGALRM` makes that visible in `rt_sigpending`.
    {
        let alarm_bit: u64 = 1u64 << (lin::SIGALRM as usize - 1); // wire
        let r = lin::rt_sigprocmask(lin::SIG_BLOCK, &alarm_bit, core::ptr::null_mut());
        lin::check_eq(
            &mut failed,
            b"SIGALRM is blocked for the null-event check",
            r,
            0,
        );
        let mut null_id: i32 = -1;
        let r = lin::timer_create(
            lin::CLOCK_MONOTONIC,
            core::ptr::null::<lin::SigEvent>(),
            &mut null_id,
        );
        lin::check_eq(&mut failed, b"a null sigevent is accepted", r, 0);
        let mut arm = lin::Itimerspec::new();
        arm.value.tv_nsec = 30_000_000;
        let r = lin::timer_settime(null_id, 0, &arm, core::ptr::null_mut());
        lin::check_eq(&mut failed, b"the null-event timer arms", r, 0);
        lin::sleep_ms(80);
        let mut pend: u64 = 0;
        let r = lin::rt_sigpending(&mut pend, 8);
        lin::check_eq(&mut failed, b"rt_sigpending reads for the check", r, 0);
        lin::check_true(
            &mut failed,
            b"the expiry queued SIGALRM (a null event is SIGEV_SIGNAL/SIGALRM)",
            pend & alarm_bit != 0,
        );
        let r = lin::timer_delete(null_id);
        lin::check_eq(&mut failed, b"the null-event timer deletes", r, 0);
        // Take the pending SIGALRM out before unblocking: unblocking a
        // pending, unhandled signal runs its default action, and the default
        // action of SIGALRM is to end this process. A zero timeout makes the
        // wait a poll, and the answer is the signal.
        let alarm_set: u64 = 1u64 << (lin::SIGALRM as usize - 1);
        let mut seen = lin::SigInfo::zero();
        let zero = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let r = lin::rt_sigtimedwait(&alarm_set, &mut seen, &zero);
        lin::check_eq(
            &mut failed,
            b"the queued SIGALRM can be taken without running a handler",
            r,
            lin::SIGALRM as isize,
        );
        lin::check_eq(
            &mut failed,
            b"and its record is the timer's SI_TIMER",
            seen.code as isize,
            lin::SI_TIMER as isize,
        );
        let r = lin::rt_sigprocmask(lin::SIG_UNBLOCK, &alarm_bit, core::ptr::null_mut());
        lin::check_eq(&mut failed, b"SIGALRM is unblocked again", r, 0);
    }

    // --- a SIGEV_NONE timer: it counts, it reports, it stays silent ---------
    //
    // The remaining time comes back at the machine's tick granularity (4 ms at
    // 250 Hz) and is rounded *up* when armed, so a 50 ms request can read as
    // 52 ms - the timer never fires before its deadline. The bounds say that
    // rather than pretending to nanosecond precision the clock does not have.
    let (r, remaining, overrun) = none_timer_remaining_after(50, 0);
    lin::check_eq(&mut failed, b"a SIGEV_NONE timer is made and armed", r, 0);
    lin::check_true(
        &mut failed,
        b"its remaining time is within the 50 ms it was armed for, plus a tick",
        (0..=60).contains(&remaining),
    );
    let (r, remaining, overrun2) = none_timer_remaining_after(50, 80);
    lin::check_eq(&mut failed, b"reading an expired timer succeeds", r, 0);
    lin::check_eq(
        &mut failed,
        b"a one-shot that expired reports zero remaining",
        remaining as isize,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a SIGEV_NONE timer counts no overruns",
        overrun,
        0,
    );
    let _ = overrun2;

    // The id dies with the timer.
    let mut silent = lin::SigEvent::new();
    silent.notify = lin::SIGEV_NONE;
    let mut victim: i32 = -1;
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &silent, &mut victim);
    lin::check_eq(&mut failed, b"a timer to delete is made", r, 0);
    let r = lin::timer_delete(victim);
    lin::check_eq(&mut failed, b"timer_delete succeeds", r, 0);
    let mut spec = lin::Itimerspec::new();
    spec.value.tv_nsec = 1_000_000;
    let r = lin::timer_settime(victim, 0, &spec, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"a deleted id is EINVAL for timer_settime",
        r,
        lin::EINVAL,
    );
    let mut now = lin::Itimerspec::new();
    let r = lin::timer_gettime(victim, &mut now);
    lin::check_eq(
        &mut failed,
        b"a deleted id is EINVAL for timer_gettime",
        r,
        lin::EINVAL,
    );

    // --- TIMER_ABSTIME: the honest refusal ---------------------------------
    let mut plain = lin::SigEvent::new();
    plain.notify = lin::SIGEV_NONE;
    let mut absid: i32 = -1;
    let r = lin::timer_create(lin::CLOCK_MONOTONIC, &plain, &mut absid);
    lin::check_eq(&mut failed, b"a timer for the abstime check is made", r, 0);
    let r = lin::timer_settime(absid, lin::TIMER_ABSTIME, &spec, core::ptr::null_mut());
    lin::check_eq(
        &mut failed,
        b"TIMER_ABSTIME is EINVAL (one clock, no absolute domain)",
        r,
        lin::EINVAL,
    );

    // --- the musl shape: a thread, SIGRTMIN, SIGEV_THREAD_ID ---------------
    //
    // The creator blocks SIGRTMIN *before* the thread exists, so the thread
    // inherits the block and the expiry can never run a default action.
    let set = RTMIN_BIT;
    let r = lin::rt_sigprocmask(lin::SIG_BLOCK, &set, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"the creator blocks SIGRTMIN", r, 0);

    let stack_top = lin::thread_stack_top();
    lin::check_true(
        &mut failed,
        b"a stack was mapped for the waiting thread",
        stack_top > 0,
    );
    if stack_top > 0 {
        THREAD_CODE.store(0, Ordering::SeqCst);
        THREAD_VALUE.store(0, Ordering::SeqCst);
        let mut tid_slot: i32 = 0;
        let tid = lin::thread_create(stack_top as usize, waiter, 0, &mut tid_slot);
        lin::check_true(&mut failed, b"the waiting thread was created", tid > 0);

        if tid > 0 {
            // The clone's own answer is the thread's tid - the `tid_slot`
            // is written by the *child* (`CLONE_CHILD_SETTID`), so it can
            // still be zero at this point and is only checked once the
            // thread has run.
            let mut ev = lin::SigEvent::new();
            ev.notify = lin::SIGEV_THREAD_ID;
            ev.signo = lin::SIGRTMIN;
            ev.tid = tid as i32;
            ev.value = PAYLOAD;
            let mut timer_id: i32 = -1;
            let r = lin::timer_create(lin::CLOCK_MONOTONIC, &ev, &mut timer_id);
            lin::check_eq(
                &mut failed,
                b"a SIGEV_THREAD_ID timer aimed at the thread is made",
                r,
                0,
            );

            let mut arm = lin::Itimerspec::new();
            arm.value.tv_nsec = 30_000_000; // 30 ms
            let r = lin::timer_settime(timer_id, 0, &arm, core::ptr::null_mut());
            lin::check_eq(&mut failed, b"the 30 ms timer is armed", r, 0);

            // Wait for the thread to be woken by the expiry. A bounded wait:
            // a timer that never fires must fail the test, not hang it.
            let mut waited = 0u64;
            while THREAD_CODE.load(Ordering::SeqCst) == 0 && waited < 2000 {
                lin::sleep_ms(1);
                waited += 1;
            }
            lin::check_eq(
                &mut failed,
                b"the thread was woken by the timer with SI_TIMER",
                THREAD_CODE.load(Ordering::SeqCst) as isize,
                lin::SI_TIMER as isize,
            );
            lin::check_true(
                &mut failed,
                b"the sigval the creator set arrived with it",
                THREAD_VALUE.load(Ordering::SeqCst) == PAYLOAD,
            );
            lin::check_eq(
                &mut failed,
                b"the timer is one-shot, so it read zero",
                lin::timer_gettime(timer_id, &mut now),
                0,
            );
            let r = lin::timer_delete(timer_id);
            lin::check_eq(&mut failed, b"the thread's timer is deleted", r, 0);

            // Let the thread finish before unblocking, so the process never
            // sees a pending SIGRTMIN with no disposition.
            let mut spins = 0usize;
            while THREAD_RAN.load(Ordering::SeqCst) == 0 && spins < 100_000 {
                spins += 1;
                lin::sched_yield();
            }
        }
    }
    let r = lin::rt_sigprocmask(lin::SIG_UNBLOCK, &set, core::ptr::null_mut());
    lin::check_eq(&mut failed, b"SIGRTMIN is unblocked again", r, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-timer-create: panicked\n")
}
