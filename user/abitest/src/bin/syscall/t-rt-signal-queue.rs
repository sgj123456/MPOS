//! t-rt-signal-queue - every copy of a real-time signal is delivered.
//!
//! The one thing a `Pending` bitmask and a `siginfo_t` slot *per number* cannot
//! say: Linux queues real-time signals, so N copies of `SIGRTMIN+k` all arrive,
//! first-in-first-out, each carrying its own `si_value` — whether they came from
//! `sigqueue` or from N plain `kill`s. A coalescing kernel delivers one copy and
//! the caller silently loses the rest. This is the behaviour PLAN.md §2.3 stage C
//! adds; it is measured against Linux (7.2.7: three `sigqueue`s of one number all
//! come back with their own values; two `kill`s of an RT number yield two pending;
//! the queue is bounded per-user and a full `sigqueue` is `EAGAIN`).
//!
//! Three paths are pinned, all impossible for a coalescing queue:
//!   * `rt_sigtimedwait` drains the three queued copies in order with their three
//!     `si_value`s, then finds nothing (stage C's pop path, also `signalfd`'s);
//!   * two `kill`s of one RT number are two pending copies, not one;
//!   * a handler installed on an RT number runs once per queued copy, each time
//!     reading its own `si_value` (the `take_deliverable_siginfo` path).

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const RTMIN: i32 = lin::SIGRTMIN;
const SIG_A: i32 = RTMIN + 3; // 35: sigtimedwait FIFO
const SIG_B: i32 = RTMIN + 4; // 36: kill-queues
const SIG_C: i32 = RTMIN + 5; // 37: handler multi-copy

/// Handler path: how many copies arrived, and the sum of their `si_value`s.
/// Two distinct copies of the same number summing to `0x101+0x102+0x103` is the
/// proof no copy was coalesced away.
static COUNT: AtomicUsize = AtomicUsize::new(0);
static SUM: AtomicU64 = AtomicU64::new(0);

extern "C" fn handler(_sig: i32, info: *mut lin::SigInfo, _ctx: *mut u8) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    if !info.is_null() {
        let v = unsafe { (*info).value };
        SUM.fetch_add(v, Ordering::Relaxed);
    }
}

extern "C" fn restorer() {
    let _ = lin::rt_sigreturn();
}

/// The wire `sigset_t` bit for a signal: signal `n` is bit `n-1`.
fn bit(sig: i32) -> u64 {
    1u64 << (sig - 1)
}

fn block(sig: i32) {
    let m = bit(sig);
    assert!(lin::rt_sigprocmask(lin::SIG_BLOCK, &m, core::ptr::null_mut()) == 0);
}

/// `sigqueue` one copy of `sig` carrying `value` as its `si_value`.
fn queue(sig: i32, value: u64) {
    let mut info = lin::SigInfo::zero();
    info.signo = sig;
    info.code = lin::SI_QUEUE;
    info.value = value;
    assert!(lin::rt_sigqueueinfo(lin::getpid(), sig, &info) == 0);
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-rt-signal-queue\n");
    let mut failed = false;
    let zero = lin::TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    // Block all three so every send below is queued, not delivered.
    block(SIG_A);
    block(SIG_B);
    block(SIG_C);

    // --- 1. rt_sigtimedwait drains every copy, in order, with its own value ---
    queue(SIG_A, 0x101);
    queue(SIG_A, 0x102);
    queue(SIG_A, 0x103);
    let set_a = bit(SIG_A);
    for want in [0x101u64, 0x102, 0x103] {
        let mut got = lin::SigInfo::zero();
        let r = lin::rt_sigtimedwait(&set_a, &mut got, &zero);
        lin::check_eq(
            &mut failed,
            b"sigtimedwait returns the RT number",
            r,
            SIG_A as isize,
        );
        lin::check_true(
            &mut failed,
            b"and the copy's own si_value, in FIFO order",
            got.value == want,
        );
    }
    // A fourth finds nothing — the three copies were all consumed, not more.
    let mut extra = lin::SigInfo::zero();
    let r = lin::rt_sigtimedwait(&set_a, &mut extra, &zero);
    lin::check_eq(
        &mut failed,
        b"after draining, sigtimedwait finds no fourth copy",
        r,
        lin::EAGAIN,
    );

    // --- 2. two plain kills of one RT number are two pending copies ---
    assert!(lin::kill(lin::getpid() as i32, SIG_B) == 0);
    assert!(lin::kill(lin::getpid() as i32, SIG_B) == 0);
    let set_b = bit(SIG_B);
    let mut seen = 0;
    for _ in 0..3 {
        let mut got = lin::SigInfo::zero();
        let r = lin::rt_sigtimedwait(&set_b, &mut got, &zero);
        if r == SIG_B as isize {
            seen += 1;
        }
    }
    lin::check_eq(
        &mut failed,
        b"two kill()s of an RT number queue two copies (not coalesced to one)",
        seen,
        2,
    );

    // --- 3. a handler runs once per queued copy, reading each si_value ---
    let act = lin::SigAction {
        handler: handler as extern "C" fn(i32, *mut lin::SigInfo, *mut u8) as usize,
        flags: lin::SA_SIGINFO | lin::SA_RESTORER,
        restorer: restorer as extern "C" fn() as usize,
        mask: 0,
    };
    lin::check_eq(
        &mut failed,
        b"installing a handler on the RT number succeeds",
        lin::rt_sigaction(SIG_C, &act, core::ptr::null_mut()),
        0,
    );
    queue(SIG_C, 0x101);
    queue(SIG_C, 0x102);
    queue(SIG_C, 0x103);
    // Unblock: all three queued copies are delivered to the handler in turn.
    let m_c = bit(SIG_C);
    lin::check_eq(
        &mut failed,
        b"unblocking the RT number succeeds",
        lin::rt_sigprocmask(lin::SIG_UNBLOCK, &m_c, core::ptr::null_mut()),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the handler ran once per queued copy (three)",
        COUNT.load(Ordering::Relaxed) as isize,
        3,
    );
    lin::check_eq(
        &mut failed,
        b"and read all three distinct payloads (sum)",
        SUM.load(Ordering::Relaxed) as isize,
        0x101 + 0x102 + 0x103,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rt-signal-queue: panicked\n")
}
