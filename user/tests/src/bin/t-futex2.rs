//! t-futex2 - `futex_wake(2)` (454) and `futex_wait(2)` (455): the futex2 pair.
//!
//! The pair is the syscall-numbered spelling of `FUTEX_WAKE`/`FUTEX_WAIT`: one
//! thirty-two-bit word, a bitset as the wake's mask, and - on the wait side -
//! an absolute deadline and a `clockid` argument. A runtime that already speaks
//! `futex_waitv(2)` meets the same vocabulary here; a runtime that wants one
//! address and a bitset has no reason to reach for the old operation-word call.
//!
//! # The shape, measured before it was written
//!
//! Measured on the host (Linux 7.2.11) with `build/scratch/futex2/probe*.c`, and
//! pinned here as the caller-visible contract:
//!
//! * the call's `flags` word names the word's shape: `FUTEX2_SIZE_U32` (`0x02`)
//!   is required and is the *only* size either call accepts - `SIZE_U8`/`U16`/
//!   `U64` spelling and a missing size bit are all `EINVAL`. `FUTEX2_PRIVATE`
//!   (`0x80`) may be added.
//! * the `mask` argument is a bitset over the word: wider than thirty-two bits
//!   is `EINVAL` (so `~0UL` is the wrong spelling and `~0u` the right one), and
//!   zero is `EINVAL` on both calls.
//! * `futex_wake`'s `nr` is an `int`: zero wakes nothing and answers `0`, and so
//!   does a negative count (Linux's walk runs no iterations either way). The
//!   answer is the number actually woken, never more than `nr`.
//! * `futex_wait`'s `val` is thirty-two bits: a value above `u32::MAX` is
//!   `EINVAL` rather than truncated, a word that no longer holds it is `EAGAIN`,
//!   and a past deadline on a matching word is `ETIMEDOUT`.
//! * an address unaligned to four bytes is `EINVAL` on both calls; one that is
//!   aligned and unreadable is `EFAULT` on both (the wake half validates the
//!   word too, measured).
//! * the wait's deadline is absolute; `clockid` is read only when a deadline is
//!   given (`0` realtime, `1` monotonic, anything else `EINVAL`), and with a
//!   NULL deadline the argument is ignored - measured, clockid `99` there gives
//!   the value check's own answer.
//!
//! # Two divergences, named
//!
//! * The **private bit is not part of this kernel's key** - one wait table per
//!   user address, exactly as `futex(2)`'s `FUTEX_PRIVATE_FLAG` is not - so a
//!   shared wake reaches a private waiter here where Linux keeps two key
//!   spaces. The same divergence the old operation-word call already carries.
//! * `FUTEX2_NUMA` (`0x04`) and `FUTEX2_MPOL` (`0x08`) are `EOPNOTSUPP`: Linux
//!   7.2 serves both, and a NUMA word is *two* words (the value and a node), so
//!   this kernel refuses instead of answering from the first one. The claim is
//!   made on this kernel only; the host is where the flag works.
//!
//! # Why `libc::syscall`
//!
//! `rustix` does not bind these two numbers (`t-futex` covers the operation-
//! word calls), so the numbers - 454 and 455 on both machines, they landed after
//! the two tables converged - are spelled here.

use mpos_user_tests::Report;
use rustix::io::Errno;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// `futex_wake(2)`, 454 on both machines.
const FUTEX2_WAKE: libc::c_long = 454;
/// `futex_wait(2)`, 455 on both machines.
const FUTEX2_WAIT: libc::c_long = 455;

/// `FUTEX2_SIZE_U32`: the word is thirty-two bits. The only size either call
/// accepts, and the reason a call without it is refused.
const SIZE_U32: u32 = 0x02;
/// `FUTEX2_PRIVATE`: the word is this process's own table entry.
const PRIVATE: u32 = 0x80;
/// `FUTEX2_NUMA`: the word names a node after it - a shape this kernel refuses.
const NUMA: u32 = 0x04;

fn wake2(word: &AtomicU32, mask: libc::c_ulong, nr: libc::c_int, flags: u32) -> Result<i64, Errno> {
    let ret = unsafe {
        libc::syscall(
            FUTEX2_WAKE,
            word as *const AtomicU32 as usize,
            mask as usize,
            nr as usize,
            flags as usize,
        )
    };
    if ret < 0 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default(),
        ))
    } else {
        Ok(ret as i64)
    }
}

fn wait2(
    word: &AtomicU32,
    val: libc::c_ulong,
    mask: libc::c_ulong,
    flags: u32,
    timeout: Option<&libc::timespec>,
    clockid: libc::clockid_t,
) -> Result<i64, Errno> {
    let timeout = timeout.map_or(0, |ts| ts as *const libc::timespec as usize);
    let ret = unsafe {
        libc::syscall(
            FUTEX2_WAIT,
            word as *const AtomicU32 as usize,
            val as usize,
            mask as usize,
            flags as usize,
            timeout,
            clockid as usize,
        )
    };
    if ret < 0 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default(),
        ))
    } else {
        Ok(ret as i64)
    }
}

/// "now" plus `millis` on `clock`, as the absolute `timespec` both calls take.
fn deadline(clock: libc::clockid_t, millis: i64) -> libc::timespec {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_nsec += millis * 1_000_000;
    // Both directions: a `millis` of `-50` on a clock whose nanosecond field is
    // under 50 ms borrows from the seconds field, and the resulting `timespec`
    // still has to be a *valid* one - Linux refuses a negative `tv_nsec` with
    // `EINVAL` rather than reading it as an earlier instant.
    while ts.tv_nsec >= 1_000_000_000 {
        ts.tv_nsec -= 1_000_000_000;
        ts.tv_sec += 1;
    }
    while ts.tv_nsec < 0 {
        ts.tv_nsec += 1_000_000_000;
        ts.tv_sec -= 1;
    }
    ts
}

/// Sleep long enough that a waiter thread is inside the kernel before the wake
/// is sent - the same 100 ms the host probes used.
fn park() {
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// The word the round-trip claims use; the value never changes, so the wait
/// only ever leaves through a wake.
static ONE: AtomicU32 = AtomicU32::new(7);
/// Two waiters for the count claims.
static PAIR: AtomicU32 = AtomicU32::new(7);

fn main() {
    println!("t-futex2");
    let mut r = Report::new();

    // --- futex_wait: the word, the value, the mask ---
    let word = AtomicU32::new(9);
    r.check(
        "wait: a word that is not val is EAGAIN",
        matches!(
            wait2(
                &word,
                8,
                0xffff_ffff,
                SIZE_U32,
                None,
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::AGAIN)
        ),
    );
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    r.check(
        "wait: flags 0 is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                0,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    r.check(
        "wait: SIZE_U16 is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                0x01,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    r.check(
        "wait: SIZE_U64 is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                0x03,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    r.check(
        "wait: an unknown flag bit is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                SIZE_U32 | 0x100,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    r.check(
        "wait: mask 0 is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0,
                SIZE_U32,
                None,
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    r.check(
        "wait: mask wider than 32 bits is EINVAL",
        matches!(
            wait2(
                &word,
                9,
                0x1_0000_0000,
                SIZE_U32,
                None,
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );
    r.check(
        "wait: val above 32 bits is EINVAL",
        matches!(
            wait2(
                &word,
                0x1_0000_0009,
                0xffff_ffff,
                SIZE_U32,
                None,
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::INVAL)
        ),
    );

    // The unreadable and unaligned word: the alignment check comes first, and
    // NULL is aligned, so it reads as EFAULT.
    let misaligned = unsafe { (word.as_ptr() as *const AtomicU32).byte_add(1) };
    let bad_wait = |addr: *const AtomicU32| -> Result<i64, Errno> {
        let ret = unsafe {
            libc::syscall(
                FUTEX2_WAIT,
                addr as usize,
                9usize,
                0xffff_ffffusize,
                SIZE_U32 as usize,
                0usize,
                1usize,
            )
        };
        if ret < 0 {
            Err(Errno::from_raw_os_error(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or_default(),
            ))
        } else {
            Ok(ret as i64)
        }
    };
    r.check(
        "wait: a word not aligned to four bytes is EINVAL",
        matches!(bad_wait(misaligned as *const AtomicU32), Err(Errno::INVAL)),
    );
    r.check(
        "wait: an unreadable word is EFAULT",
        matches!(bad_wait(0x10 as *const AtomicU32), Err(Errno::FAULT)),
    );

    // --- futex_wait: the deadline ---
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    let start = std::time::Instant::now();
    let answer = wait2(
        &word,
        9,
        0xffff_ffff,
        SIZE_U32 | PRIVATE,
        Some(&d),
        libc::CLOCK_MONOTONIC as libc::clockid_t,
    );
    let elapsed = start.elapsed();
    r.check(
        "wait: a 50 ms monotonic deadline is ETIMEDOUT",
        matches!(answer, Err(Errno::TIMEDOUT)),
    );
    r.check(
        "wait: the deadline fires at its own time, not earlier",
        elapsed >= std::time::Duration::from_millis(20)
            && elapsed < std::time::Duration::from_millis(2_000),
    );
    let d = deadline(libc::CLOCK_REALTIME as libc::clockid_t, 50);
    r.check(
        "wait: CLOCK_REALTIME takes a realtime deadline",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                SIZE_U32,
                Some(&d),
                libc::CLOCK_REALTIME as libc::clockid_t
            ),
            Err(Errno::TIMEDOUT)
        ),
    );
    let past = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, -50);
    r.check(
        "wait: a past deadline is ETIMEDOUT",
        matches!(
            wait2(
                &word,
                9,
                0xffff_ffff,
                SIZE_U32,
                Some(&past),
                libc::CLOCK_MONOTONIC as libc::clockid_t
            ),
            Err(Errno::TIMEDOUT)
        ),
    );
    let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 50);
    r.check(
        "wait: an invalid clockid with a deadline is EINVAL",
        matches!(
            wait2(&word, 9, 0xffff_ffff, SIZE_U32, Some(&d), 99),
            Err(Errno::INVAL)
        ),
    );
    r.check(
        "wait: an invalid clockid with no deadline is ignored",
        matches!(
            wait2(&word, 8, 0xffff_ffff, SIZE_U32, None, 99),
            Err(Errno::AGAIN)
        ),
    );

    // --- futex_wake: the arguments ---
    let word = AtomicU32::new(9);
    r.check(
        "wake: flags 0 is EINVAL",
        matches!(wake2(&word, 0xffff_ffff, 1, 0), Err(Errno::INVAL)),
    );
    r.check(
        "wake: mask 0 is EINVAL",
        matches!(wake2(&word, 0, 1, SIZE_U32), Err(Errno::INVAL)),
    );
    r.check(
        "wake: a mask wider than 32 bits is EINVAL",
        matches!(
            wake2(&word, u64::MAX as libc::c_ulong, 1, SIZE_U32),
            Err(Errno::INVAL)
        ),
    );
    let bad_wake = |addr: *const AtomicU32| -> Result<i64, Errno> {
        let ret = unsafe {
            libc::syscall(
                FUTEX2_WAKE,
                addr as usize,
                0xffff_ffffusize,
                1usize,
                SIZE_U32 as usize,
            )
        };
        if ret < 0 {
            Err(Errno::from_raw_os_error(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or_default(),
            ))
        } else {
            Ok(ret as i64)
        }
    };
    r.check(
        "wake: a word not aligned to four bytes is EINVAL",
        matches!(bad_wake(misaligned as *const AtomicU32), Err(Errno::INVAL)),
    );
    r.check(
        "wake: an unreadable word is EFAULT",
        matches!(bad_wake(0x10 as *const AtomicU32), Err(Errno::FAULT)),
    );
    r.check(
        "wake: nr 0 answers 0",
        wake2(&word, 0xffff_ffff, 0, SIZE_U32) == Ok(0),
    );
    r.check(
        "wake: a negative nr answers 0",
        wake2(&word, 0xffff_ffff, -1, SIZE_U32) == Ok(0),
    );
    r.check(
        "wake: no waiters answers 0",
        wake2(&word, 0xffff_ffff, 1, SIZE_U32) == Ok(0),
    );

    // --- the round trips ---
    let done = Arc::new(AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 2_000);
            let answer = wait2(
                &ONE,
                7,
                0xffff_ffff,
                SIZE_U32 | PRIVATE,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            done.store(answer == Ok(0), Ordering::SeqCst);
        });
    }
    park();
    let woke = wake2(&ONE, 0xffff_ffff, 1, SIZE_U32 | PRIVATE);
    std::thread::sleep(std::time::Duration::from_millis(50));
    r.check(
        "a private waiter is woken by a private wake, which counts it",
        woke == Ok(1) && done.load(Ordering::SeqCst),
    );

    // The shared spelling: same pair of words without FUTEX2_PRIVATE. This
    // kernel's table does not key the bit, so the pair has to work here too.
    let done = Arc::new(AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 2_000);
            let answer = wait2(
                &PAIR,
                7,
                0xffff_ffff,
                SIZE_U32,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            done.store(answer == Ok(0), Ordering::SeqCst);
        });
    }
    park();
    let woke = wake2(&PAIR, 0xffff_ffff, 1, SIZE_U32);
    std::thread::sleep(std::time::Duration::from_millis(50));
    r.check(
        "the shared spelling round-trips too",
        woke == Ok(1) && done.load(Ordering::SeqCst),
    );

    // The bitset: a waiter that asked for bit 2 is not reached by a mask of
    // bit 1, and the wake's answer says so (0), then bit 2 reaches it.
    let done = Arc::new(AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 2_000);
            let answer = wait2(
                &ONE,
                7,
                0x2,
                SIZE_U32 | PRIVATE,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            done.store(answer == Ok(0), Ordering::SeqCst);
        });
    }
    park();
    let wrong_bits = wake2(&ONE, 0x1, 1, SIZE_U32 | PRIVATE);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let still_parked = !done.load(Ordering::SeqCst);
    let right_bits = wake2(&ONE, 0x2, 1, SIZE_U32 | PRIVATE);
    std::thread::sleep(std::time::Duration::from_millis(50));
    r.check(
        "the bitset decides who a wake reaches",
        wrong_bits == Ok(0) && still_parked && right_bits == Ok(1) && done.load(Ordering::SeqCst),
    );

    // Two waiters, one wake of nr 1: the answer counts one, and the other is
    // left to its own deadline.
    let pair = Arc::new(AtomicU32::new(7));
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    {
        let first = Arc::clone(&first);
        let pair = Arc::clone(&pair);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 300);
            let answer = wait2(
                &pair,
                7,
                0xffff_ffff,
                SIZE_U32,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            first.store(answer == Ok(0), Ordering::SeqCst);
        });
    }
    {
        let second = Arc::clone(&second);
        let pair = Arc::clone(&pair);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 300);
            let answer = wait2(
                &pair,
                7,
                0xffff_ffff,
                SIZE_U32,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            second.store(answer == Ok(0), Ordering::SeqCst);
        });
    }
    park();
    let woken = wake2(&pair, 0xffff_ffff, 1, SIZE_U32);
    std::thread::sleep(std::time::Duration::from_millis(400));
    r.check(
        "a wake of nr 1 counts one and leaves the other parked",
        woken == Ok(1) && (first.load(Ordering::SeqCst) ^ second.load(Ordering::SeqCst)),
    );

    // Two waiters and a wake larger than the set: the answer caps at the
    // waiters that existed.
    let pair = Arc::new(AtomicU32::new(7));
    let count = Arc::new(AtomicU32::new(0));
    for _ in 0..2 {
        let count = Arc::clone(&count);
        let pair = Arc::clone(&pair);
        std::thread::spawn(move || {
            let d = deadline(libc::CLOCK_MONOTONIC as libc::clockid_t, 500);
            let answer = wait2(
                &pair,
                7,
                0xffff_ffff,
                SIZE_U32,
                Some(&d),
                libc::CLOCK_MONOTONIC as libc::clockid_t,
            );
            if answer == Ok(0) {
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
    }
    park();
    let woken = wake2(&pair, 0xffff_ffff, 99, SIZE_U32);
    std::thread::sleep(std::time::Duration::from_millis(50));
    r.check(
        "a wake counts no more waiters than it finds",
        woken == Ok(2) && count.load(Ordering::SeqCst) == 2,
    );

    // --- the shape this kernel refuses, on this kernel only ---
    if !mpos_user_tests::host_run() {
        let word = AtomicU32::new(9);
        r.check(
            "a NUMA word is EOPNOTSUPP, not a half answer",
            matches!(
                wait2(
                    &word,
                    9,
                    0xffff_ffff,
                    SIZE_U32 | NUMA,
                    None,
                    libc::CLOCK_MONOTONIC as libc::clockid_t
                ),
                Err(Errno::NOTSUP)
            ) && matches!(
                wake2(&word, 0xffff_ffff, 1, SIZE_U32 | NUMA),
                Err(Errno::NOTSUP)
            ),
        );
    } else {
        println!(
            "note: the host serves the NUMA spelling (this kernel answers EOPNOTSUPP); \
             the claim is made on mpos only"
        );
    }

    r.finish();
}
