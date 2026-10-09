//! t-rt-signal-queue - every copy of a real-time signal is delivered.
//!
//! The one thing a `Pending` bitmask and a `siginfo_t` slot *per number* cannot
//! say: Linux queues real-time signals, so N copies of `SIGRTMIN+k` all arrive,
//! first-in-first-out, each carrying its own `si_value` - whether they came from
//! `sigqueue` or from N plain `kill`s. A coalescing kernel delivers one copy and
//! the caller silently loses the rest. This is the behaviour NOTES.md §2.3 stage C
//! adds; it is measured against Linux (7.2.7: three `sigqueue`s of one number all
//! come back with their own values; two `kill`s of an RT number yield two pending;
//! the queue is bounded per-user and a full `sigqueue` is `EAGAIN`).
//!
//! Three paths are pinned, all impossible for a coalescing queue:
//!   * `sigtimedwait` drains the three queued copies in order with their three
//!     `si_value`s, then finds nothing (stage C's pop path, also `signalfd`'s);
//!   * two `kill`s of one RT number are two pending copies, not one;
//!   * a handler installed on an RT number runs once per queued copy, each time
//!     reading its own `si_value` (the `take_deliverable_siginfo` path).
//!
//! Every one of the three is *queued* rather than delivered, which is what makes
//! the queue observable: the RT numbers are blocked for the whole first half, so
//! each send lands in the pending record, and the second half unblocks the one
//! number whose three copies must reach the handler in turn.
//!
//! # The door
//!
//! This program's subject is what the *queue* does, not the shape of the calls
//! that reach it, so the doors are the named ones: `libc::sigqueue` (which musl
//! builds as the 128-byte record `rt_sigqueueinfo` takes - `si_signo` at 0,
//! `si_code` `SI_QUEUE` (-1) at 8, the sender at 16/20 and the value at 24,
//! measured by disassembling the crate's own `libc.a`), `libc::sigtimedwait`
//! (the same archive's build of it passes `sigsetsize` 8 and retries `EINTR`),
//! `libc::sigprocmask`, `libc::sigaction` and `libc::kill`. The *record* a
//! waiting caller reads back is the machine's own `libc::siginfo_t`, and the
//! value comes out of its union through libc's accessor - the layout
//! `t-waitid` pinned on both machines, not an offset written down here.
//!
//! The one place a libc would have hidden a claim is the *number*: musl's
//! `sigaction` refuses the three real-time numbers it keeps for itself (32,
//! 33, 34) before the syscall. This program's three live in the range a caller
//! may use - 35, 36 and 37 - which is why it can install a handler through the
//! wrapper at all; `t-rt-sigaction` is where the raw boundary itself is pinned.

use mpos_user_tests::Report;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// `SIGRTMIN` on the wire, the number `t-rt-sigaction` pins as installable.
const RTMIN: i32 = 32;
const SIG_A: i32 = RTMIN + 3; // 35: the sigtimedwait FIFO
const SIG_B: i32 = RTMIN + 4; // 36: the two kills
const SIG_C: i32 = RTMIN + 5; // 37: the handler's three copies

/// Handler path: how many copies arrived, and the sum of their `si_value`s.
/// Two distinct copies of the same number summing to `0x101+0x102+0x103` is the
/// proof no copy was coalesced away.
static COUNT: AtomicUsize = AtomicUsize::new(0);
static SUM: AtomicU64 = AtomicU64::new(0);

extern "C" fn handler(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    if !info.is_null() {
        // SAFETY: `info` is the `siginfo_t` the kernel filled in for a
        // `SA_SIGINFO` handler, and `si_value()` reads the union's second word
        // - the same accessor `t-rt-sigtimedwait` reads `si_pid()` through.
        let value = unsafe { (*info).si_value().sival_ptr as usize } as u64;
        SUM.fetch_add(value, Ordering::Relaxed);
    }
}

/// The wire's `sigset_t` as the libc spells it, built through the libc's own
/// `sigaddset` so the bit arithmetic stays the machine's business.
fn set_of(signals: &[i32]) -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    for &sig in signals {
        // SAFETY: `set` is a `sigset_t` this process owns and `sig` is one of
        // the numbers above.
        unsafe { libc::sigaddset(&mut set, sig) };
    }
    set
}

fn block(sig: i32) {
    let set = set_of(&[sig]);
    // SAFETY: a set this process owns, and no old-set word is wanted.
    let rc = unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    assert!(rc == 0, "blocking {sig} failed");
}

/// The unblock is the act that delivers, so its own answer is a claim.
fn unblock(sig: i32) -> bool {
    let set = set_of(&[sig]);
    // SAFETY: as above.
    unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) == 0 }
}

/// `sigqueue` one copy of `sig` carrying `value` as its `si_value`. The libc
/// sets `si_code` to `SI_QUEUE` and names the sender itself, which is the
/// `sigqueue(3)` contract rather than this test's spelling of it.
fn queue(sig: i32, value: u64) {
    let val = libc::sigval {
        sival_ptr: value as usize as *mut libc::c_void,
    };
    // SAFETY: signalling this process, which has nothing to lose by it.
    let rc = unsafe { libc::sigqueue(libc::getpid(), sig, val) };
    assert!(rc == 0, "sigqueue of {sig} failed");
}

/// A zero timeout: "look once", which is the shape a drain wants.
fn now() -> libc::timespec {
    libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }
}

/// A record seeded with 0xFF, so a field the kernel writes is a field the
/// caller can see.
fn poisoned() -> libc::siginfo_t {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the record's own bytes, which the kernel will overwrite.
    unsafe {
        std::ptr::write_bytes(
            (&raw mut info).cast::<u8>(),
            0xFF,
            std::mem::size_of::<libc::siginfo_t>(),
        );
    }
    info
}

/// `sigtimedwait` with a zero timeout: take one copy off the queue or nothing.
fn take(set: &libc::sigset_t) -> (i32, libc::siginfo_t) {
    let mut info = poisoned();
    let zero = now();
    // SAFETY: the set and the record are this process's own words.
    let r = unsafe { libc::sigtimedwait(set, &mut info, &zero) };
    (r, info)
}

fn main() {
    println!("t-rt-signal-queue");
    let mut r = Report::new();

    // Block all three so every send below is queued, not delivered.
    block(SIG_A);
    block(SIG_B);
    block(SIG_C);

    // --- 1. sigtimedwait drains every copy, in order, with its own value ---
    queue(SIG_A, 0x101);
    queue(SIG_A, 0x102);
    queue(SIG_A, 0x103);
    let set_a = set_of(&[SIG_A]);
    for want in [0x101u64, 0x102, 0x103] {
        let (got, info) = take(&set_a);
        r.check("sigtimedwait returns the RT number", got == SIG_A);
        r.check(
            "and the copy's own si_value, in FIFO order",
            // SAFETY: `info` was written by the call that just returned.
            unsafe { info.si_value() }.sival_ptr as usize as u64 == want,
        );
    }
    // A fourth finds nothing - the three copies were all consumed, not more.
    let (got, _) = take(&set_a);
    r.check(
        "after draining, sigtimedwait finds no fourth copy",
        got == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN),
    );

    // --- 2. two plain kills of one RT number are two pending copies ---
    // SAFETY: signalling this process.
    unsafe {
        assert!(libc::kill(libc::getpid(), SIG_B) == 0);
        assert!(libc::kill(libc::getpid(), SIG_B) == 0);
    }
    let set_b = set_of(&[SIG_B]);
    let mut seen = 0;
    for _ in 0..3 {
        let (got, _) = take(&set_b);
        if got == SIG_B {
            seen += 1;
        }
    }
    r.check(
        "two kill()s of an RT number queue two copies (not coalesced to one)",
        seen == 2,
    );

    // --- 3. a handler runs once per queued copy, reading each si_value ---
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO;
    // SAFETY: `act` is a fully filled `sigaction` for a number the wrapper
    // serves (35 is past musl's own three), and no old record is wanted.
    r.check(
        "installing a handler on the RT number succeeds",
        unsafe { libc::sigaction(SIG_C, &act, std::ptr::null_mut()) } == 0,
    );

    queue(SIG_C, 0x101);
    queue(SIG_C, 0x102);
    queue(SIG_C, 0x103);
    // Unblock: all three queued copies are delivered to the handler in turn,
    // on the unblocking call's own return.
    r.check("unblocking the RT number succeeds", unblock(SIG_C));
    r.check(
        "the handler ran once per queued copy (three)",
        COUNT.load(Ordering::Relaxed) as isize == 3,
    );
    r.check(
        "and read all three distinct payloads (sum)",
        SUM.load(Ordering::Relaxed) as isize == 0x101 + 0x102 + 0x103,
    );

    r.finish()
}
