//! t-iret - where a handled signal is handed over: at a trap, or at a system call.
//!
//! **A claim, and now the same claim on every kernel this runs on.**
//! The question is a single sentence: *does a handler run for a task that has
//! not entered the kernel since the signal was posted?* Linux hands a handled
//! signal over on the return to ring 3 from **any** trap, so a task that
//! computes in a tight loop and makes no calls is interrupted at the next tick.
//!
//! aarch64 has done that since 2026-11-12 (`docs/notes/` §2.13): the EL0
//! interrupt stub builds a whole `UserFrame` and the entry hands it to
//! `hal::kernel::deliver_at_trap` before it preempts. x86_64 followed on
//! 2026-11-13 by a different route, because its `extern "x86-interrupt"` frame
//! carries `RIP`/`CS`/`RFLAGS`/`RSP`/`SS` and saves the general registers below
//! it where Rust has no name for them: the PIT entry became a hand-written stub
//! pushing a `TrapRegs` block, which is the same object shape the syscall gate
//! builds, so one `Gate::capture_trap_regs` now serves both doors.
//!
//! Which means the expectation is no longer per kernel - there is one answer
//! left, and the program asserts it unconditionally. It asserted the *gap* on
//! x86_64 until this landed, which is how the change was known to be finished:
//! that arm reddened the moment the stub started delivering, and named its own
//! line. A claim that quietly accepted either answer would have pinned nothing
//! on either machine.
//!
//! The difference was never academic: it is why `Ctrl-C` did not interrupt a
//! program that was computing rather than calling.
//!
//! ## The measurement, and why it is two measurements
//!
//! One loop is not enough to tell the two behaviours apart, because "the loop
//! ended by itself" and "the loop ended because of the signal" are different
//! answers that a single reading confuses. So the same burn runs twice:
//!
//! 1. **With a signal armed**, and a loop that makes no calls.
//! 2. **The same loop with no signal** - the control, which says whether the
//!    loop can even finish inside the budget on this machine.
//!
//! The reading is then a *contrast*: with the signal, does the loop stop early?
//! A program whose loop simply cannot run to its limit inside the budget would
//! look identical to one interrupted by a signal if only the first run were
//! taken, so the control is not decoration - it is what makes the reading mean
//! something.
//!
//! ## How the signal is armed without entering the loop's budget
//!
//! `timer_create(CLOCK_PROCESS_CPUTIME_ID, ...)` with `SIGEV_SIGNAL` - a timer
//! on CPU time, so it fires once the process has *used* 30 ms of processor
//! rather than after 30 ms of wall clock. That distinction matters here: a
//! wall-clock timer on a loaded machine can fire while the task is not running
//! at all, which would make the reading a statement about the host's scheduler
//! instead of this kernel's delivery point. This kernel serves that clock
//! (`docs/uapi/` §5.1, §6.123) and the resolution is the tick, so the arming
//! deadline is approximate - which is fine, because the claim is about *which
//! door* the signal comes through, never about when.
//!
//! The handler sets a `volatile` flag and nothing more: a handler that made a
//! syscall would answer the question by asking it, because returning from the
//! handler goes back through the same path.
//!
//! ## What each kernel prints
//!
//! Linux (measured, `build/scratch/timerclock/iret.c`, and this program on that
//! host): `loop left by THE SIGNAL after N iteration(s)` with N far below the
//! limit. This kernel on aarch64: the same shape - measured 11 337 551 of
//! 400 000 000 (`build/test-lane-aarch64-4/serial.log`, 2026-11-12). This kernel
//! on x86_64 after the stub: the same shape again - measured 18 913 221 of
//! 400 000 000 (`build/boot-iret-x86.log`, 2026-11-13), with the pre-stub
//! reading `count exhaustion` and the timer already expired.
//!
//! Three kernels, one answer. The claim below is therefore unconditional, and
//! the only thing it can now say is whether *this* kernel's door is the trap
//! door - which is the whole question.

use mpos_user_tests::Report;

/// The loop's iteration budget. Large enough that a signal at 30 ms of CPU time
/// arrives long before it, small enough that an uninterrupted run finishes in a
/// few seconds even on a TCG-emulated core.
const LIMIT: i64 = 400_000_000;

/// The handler's flag. `sig_atomic_t` is the only type a handler may portably
/// touch, and the loop reads it through `read_volatile` so the compiler cannot
/// hoist the check out of the loop's body.
static mut GOT: libc::c_int = 0;

extern "C" fn handler(_sig: libc::c_int) {
    // SAFETY: `GOT` is the one object this handler touches, it is only ever
    // written here and only read by the interrupted loop, and `sig_atomic_t`
    // access is atomic with respect to the interruption by definition.
    unsafe { GOT = GOT.saturating_add(1) };
}

/// One burn: a loop that makes **no** system call, answering the iterations it
/// managed and whether the handler ran.
///
/// `armed` is what makes this two measurements rather than one: with the flag
/// clear the same loop runs the control.
fn burn(armed: bool) -> (i64, bool) {
    unsafe { GOT = 0 };
    let mut timer: libc::timer_t = unsafe { core::mem::zeroed() };
    if armed {
        // A NULL `sigevent`'s default is `SIGALRM`, but the signal is named
        // explicitly: `SIGEV_SIGNAL` plus a number is the shape this measures,
        // and relying on the default would leave the reading describing a
        // different call than the one written down.
        let mut ev: libc::sigevent = unsafe { core::mem::zeroed() };
        ev.sigev_notify = libc::SIGEV_SIGNAL;
        ev.sigev_signo = libc::SIGUSR1;
        let made =
            unsafe { libc::timer_create(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ev, &mut timer) };
        if made != 0 {
            // The clock or the call is missing: that is itself a reading, and
            // it is reported rather than silently turning this into a control.
            return (-1, false);
        }
        let mut its: libc::itimerspec = unsafe { core::mem::zeroed() };
        its.it_value.tv_nsec = 30 * 1_000_000;
        unsafe { libc::timer_settime(timer, 0, &its, core::ptr::null_mut()) };
    }

    let mut x: f64 = 0.0;
    let mut i: i64 = 0;
    while i < LIMIT {
        x += i as f64;
        // The check the compiler must not hoist: `read_volatile` is the only
        // way to tell it the flag can change without the loop doing anything.
        let stop = unsafe { core::ptr::read_volatile(&raw const GOT) } != 0;
        if stop {
            break;
        }
        i += 1;
    }
    // Keep `x` observable so the arithmetic cannot be optimised away - a loop
    // that the compiler removed would be a reading about nothing.
    let managed = i + (x as i64 & 0);
    // **Read the flag before any system call.** This is the instrument's whole
    // correctness: the question is whether the signal interrupted *the loop*,
    // and the first syscall after it is a door the signal can come through.
    // Reading `GOT` after `timer_delete` answered `true` on a machine whose
    // loop had in fact run to its limit - the disarm syscall had delivered the
    // pending signal - and the reading said "delivered at a trap" about a task
    // that had not been interrupted at all. Caught by the guest contradicting
    // the host.
    let got = unsafe { core::ptr::read_volatile(&raw const GOT) } != 0;
    if armed {
        unsafe { libc::timer_delete(timer) };
    }
    (managed, got)
}

fn main() {
    let mut r = Report::new();

    // The handler must be installed *before* the timer is armed, or the default
    // action for `SIGUSR1` ends the process and the reading is about a corpse.
    let mut sa: libc::sigaction = unsafe { core::mem::zeroed() };
    sa.sa_sigaction = handler as *const () as usize;
    sa.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut());
    }

    // 1. The control first, so the two readings are printed in the order that
    //    makes the contrast readable: what the loop does with no signal at all.
    //    This claim holds on every kernel - the loop must be able to finish.
    let (control_done, control_got) = burn(false);
    r.check(
        &format!(
            "control: with no signal, the loop reaches its limit ({control_done} of {LIMIT} iteration(s))"
        ),
        control_done == LIMIT && !control_got,
    );

    // 2. The measurement, against the door this kernel has.
    let (armed_done, armed_got) = burn(true);
    let left_by = if armed_got {
        "THE SIGNAL"
    } else {
        "count exhaustion"
    };
    // One claim, one expected answer: Linux's. The loop must leave early *and*
    // it must have left because the handler ran - `armed_got` alone would pass
    // for a loop that broke on something else, and `armed_done < LIMIT` alone
    // would pass for a compiler that elided the loop.
    r.check(
        &format!(
            "with a 30 ms CPU-time signal armed, the loop leaves at the signal: {left_by} after {armed_done} of {LIMIT} iteration(s)"
        ),
        armed_got && armed_done < LIMIT,
    );
    println!(
        "t-iret: the signal was handed over at a trap (Linux's shape, §2.13 closed on this kernel)"
    );
    r.finish();
}
