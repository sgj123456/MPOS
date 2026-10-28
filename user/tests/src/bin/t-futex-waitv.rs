//! t-futex-waitv - `futex_waitv(2)` (449): several futex words, one sleep.
//!
//! The call is the multi-word spelling of `FUTEX_WAIT`: the caller names up to
//! 128 `{ val, uaddr, flags, __reserved }` waiters, sleeps while every word
//! still holds the value it expected, and is woken by whichever one a
//! `FUTEX_WAKE` names - answering with the *index* of that word. A thread
//! runtime that parks on whichever of several events happens first has no way
//! to express that with one-address waits, which is why the call exists.
//!
//! # The shape, measured before it was written
//!
//! Measured on the host (Linux 7.2.11) before this kernel grew the arm, and
//! pinned here as the caller-visible contract:
//!
//! * the call's own `flags` word must be **zero** - `EINVAL` otherwise. The
//!   private/size flags belong on each waiter; a call carrying
//!   `FUTEX_PRIVATE_FLAG` here is the mistake this repository already made in
//!   its host probe, and Linux answers it `EINVAL` rather than reinterpreting
//!   it.
//! * `nr` is `1..=128`; zero, more than 128, or a NULL array is `EINVAL`.
//! * a waiter's `flags` carries exactly `FUTEX_32` (`0x02`) and
//!   `FUTEX_PRIVATE_FLAG` (`0x80`). A size bit is required (flags `0x80` alone
//!   is `EINVAL`), anything outside those two bits is `EINVAL`, and
//!   `__reserved` must be zero.
//! * a word that no longer holds the expected value answers `EAGAIN` - and it
//!   does so *before* a passed deadline is considered (measured: a past
//!   deadline on a mismatched word is `EAGAIN`, not `ETIMEDOUT`).
//! * a word not aligned to four bytes is `EINVAL`; an unreadable array or word
//!   is `EFAULT`.
//! * the timeout is an absolute `timespec`; the `clockid` argument is examined
//!   only when there is one - `CLOCK_MONOTONIC`/`CLOCK_REALTIME` are the two
//!   accepted, others `EINVAL`, while with a NULL timeout the argument is
//!   ignored entirely (measured: clockid 99 + NULL timeout = the value check's
//!   own answer, not `EINVAL`).
//!
//! # One divergence, named
//!
//! The *shared* spelling (a waiter without `FUTEX_PRIVATE_FLAG`) is
//! `EOPNOTSUPP` here. This kernel's wait table is one per user address, so two
//! processes' mappings of the same shared word cannot be matched; Linux serves
//! that shape, and a sleeper nobody can wake is a hang the caller cannot branch
//! on, so the refusal is named instead of approximated. The claim is made only
//! on this kernel, because the host is where the shape works.
//!
//! # Why `libc::syscall`
//!
//! `rustix` binds the four single-word futex calls (`t-futex` uses them) but
//! not `futex_waitv`, so the call goes through `libc::syscall` with the number
//! - 449 on both machines - spelled here. The wake half stays `rustix`'s:
//! `futex::wake` is the same `FUTEX_WAKE` the multi-wait is woken by, and using
//! the maintained binding keeps "2 = FUTEX_PRIVATE_FLAG" out of this file.

use mpos_user_tests::Report;
use rustix::io::Errno;
use rustix::thread::futex;
use std::sync::atomic::AtomicU32;

/// `futex_waitv(2)`, 449 on both machines: it landed after the two number
/// tables converged.
const FUTEX_WAITV: libc::c_long = 449;

/// `FUTEX2_SIZE_U32`: the per-waiter "the word is thirty-two bits". Linux's
/// only size this call accepts, which is why a waiter without it is refused.
const FUTEX_32: u32 = 0x02;
/// `FUTEX_PRIVATE_FLAG`: the waiter's word is this process's own.
const FUTEX_PRIVATE: u32 = 0x80;

/// Linux's per-call maximum.
const WAITV_MAX: usize = 128;

/// The word the first wake claim waits on.
static ONE: AtomicU32 = AtomicU32::new(7);
/// Two words for the index claims: a wake reaches each separately.
static PAIR: [AtomicU32; 2] = [AtomicU32::new(3), AtomicU32::new(4)];

/// One `struct futex_waitv`, exactly Linux's layout: two 64-bit fields and two
/// 32-bit ones, 24 bytes with no padding at either end.
#[repr(C)]
struct Waitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    reserved: u32,
}

impl Waitv {
    fn new(word: &AtomicU32, val: u64, flags: u32) -> Self {
        Waitv {
            val,
            uaddr: word as *const AtomicU32 as u64,
            flags,
            reserved: 0,
        }
    }
}

/// The call itself. `Err` is the kernel's errno, unchanged - the claim is about
/// the number the caller reads, not about a wrapper's opinion of it.
fn waitv(
    waiters: *mut Waitv,
    nr: usize,
    flags: u32,
    timeout: Option<&libc::timespec>,
    clockid: libc::c_int,
) -> Result<i64, Errno> {
    let timeout = timeout.map_or(0, |ts| ts as *const libc::timespec as usize);
    let ret = unsafe {
        libc::syscall(
            FUTEX_WAITV,
            waiters as usize,
            nr,
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

/// "now" plus `millis` on `clock`, as an absolute `timespec` - the form the
/// call's timeout argument takes.
fn deadline(clock: libc::clockid_t, millis: i64) -> libc::timespec {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_nsec += millis * 1_000_000;
    while ts.tv_nsec >= 1_000_000_000 {
        ts.tv_nsec -= 1_000_000_000;
        ts.tv_sec += 1;
    }
    ts
}

/// Sleep long enough that the waiter thread is inside the kernel before the
/// wake is sent - the same 100 ms the host probe used.
fn let_the_waiter_park() {
    std::thread::sleep(std::time::Duration::from_millis(100));
}

fn main() {
    println!("t-futex-waitv");
    let mut r = Report::new();

    // A wait on a word that no longer holds the expected value does not sleep:
    // EAGAIN is the caller's "look at the word again".
    let word = AtomicU32::new(9);
    let mut one = [Waitv::new(&word, 8, FUTEX_32 | FUTEX_PRIVATE)];
    r.check(
        "a word that is not val is EAGAIN",
        matches!(
            waitv(one.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
            Err(Errno::AGAIN)
        ),
    );

    // One waiter, woken by the same FUTEX_WAKE a single-word wait uses: the
    // answer is the index of the word the wake named.
    let wake_handle = {
        let mut one = [Waitv::new(&ONE, 7, FUTEX_32 | FUTEX_PRIVATE)];
        std::thread::spawn(move || {
            let deadline = deadline(libc::CLOCK_MONOTONIC, 2_000);
            waitv(
                one.as_mut_ptr(),
                1,
                0,
                Some(&deadline),
                libc::CLOCK_MONOTONIC,
            )
            .ok()
        })
    };
    let_the_waiter_park();
    let woke = futex::wake(&ONE, futex::Flags::PRIVATE, 1).map(|n| n as i64);
    let answer = wake_handle.join().ok().flatten();
    r.check(
        "one matching word, FUTEX_WAKE answers index 0",
        answer == Some(0) && woke == Ok(1),
    );

    // Two words, woken one at a time: the index says which one fired, and it
    // is the caller's own array order. This is the claim a one-address wait
    // cannot express at all.
    for (index, expected) in [(1usize, 1i64), (0usize, 0i64)] {
        let wake_handle = {
            let mut two = [
                Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE),
                Waitv::new(&PAIR[1], 4, FUTEX_32 | FUTEX_PRIVATE),
            ];
            std::thread::spawn(move || {
                let deadline = deadline(libc::CLOCK_MONOTONIC, 2_000);
                waitv(
                    two.as_mut_ptr(),
                    2,
                    0,
                    Some(&deadline),
                    libc::CLOCK_MONOTONIC,
                )
                .ok()
            })
        };
        let_the_waiter_park();
        let _ = futex::wake(&PAIR[index], futex::Flags::PRIVATE, 1);
        let answer = wake_handle.join().ok().flatten();
        r.check(
            if index == 1 {
                "two words: waking the second answers index 1"
            } else {
                "two words: waking the first answers index 0"
            },
            answer == Some(expected),
        );
    }

    // An absolute monotonic deadline with nobody waking it.
    {
        let mut one = [Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE)];
        let ts = deadline(libc::CLOCK_MONOTONIC, 60);
        let (before, _) = {
            let mut now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
            (now, ())
        };
        let answer = waitv(one.as_mut_ptr(), 1, 0, Some(&ts), libc::CLOCK_MONOTONIC);
        let mut after = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut after) };
        let elapsed_ms =
            (after.tv_sec - before.tv_sec) * 1000 + (after.tv_nsec - before.tv_nsec) / 1_000_000;
        r.check(
            "a 60 ms deadline is ETIMEDOUT",
            matches!(answer, Err(Errno::TIMEDOUT)),
        );
        r.check(
            "the deadline fires at its own time, not earlier",
            elapsed_ms >= 40 && elapsed_ms <= 400,
        );
    }

    // The realtime clock is the other clock the argument may name, and the
    // deadline is still absolute.
    {
        let mut one = [Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE)];
        let ts = deadline(libc::CLOCK_REALTIME, 60);
        r.check(
            "CLOCK_REALTIME takes a realtime deadline",
            matches!(
                waitv(one.as_mut_ptr(), 1, 0, Some(&ts), libc::CLOCK_REALTIME),
                Err(Errno::TIMEDOUT)
            ),
        );
    }

    // The clockid is examined only when a timeout is passed.
    {
        let word = AtomicU32::new(4);
        let mut one = [Waitv::new(&word, 5, FUTEX_32 | FUTEX_PRIVATE)];
        r.check(
            "an invalid clockid with a timeout is EINVAL",
            matches!(
                waitv(
                    one.as_mut_ptr(),
                    1,
                    0,
                    Some(&deadline(libc::CLOCK_PROCESS_CPUTIME_ID, 20)),
                    libc::CLOCK_PROCESS_CPUTIME_ID
                ),
                Err(Errno::INVAL)
            ),
        );
        r.check(
            "an invalid clockid with no timeout is ignored (the value answers EAGAIN)",
            matches!(waitv(one.as_mut_ptr(), 1, 0, None, 99), Err(Errno::AGAIN)),
        );
    }

    // nr bounds, and the call-level flags word: Linux supports none of it.
    {
        let mut one = [Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE)];
        r.check(
            "nr=0 is EINVAL",
            matches!(
                waitv(one.as_mut_ptr(), 0, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
        r.check(
            "nr=129 is EINVAL",
            matches!(
                waitv(
                    one.as_mut_ptr(),
                    WAITV_MAX + 1,
                    0,
                    None,
                    libc::CLOCK_MONOTONIC
                ),
                Err(Errno::INVAL)
            ),
        );
        r.check(
            "a call whose own flags are not zero is EINVAL",
            matches!(
                waitv(
                    one.as_mut_ptr(),
                    1,
                    FUTEX_PRIVATE,
                    None,
                    libc::CLOCK_MONOTONIC
                ),
                Err(Errno::INVAL)
            ),
        );
        r.check(
            "a NULL waiters array is EINVAL",
            matches!(
                waitv(std::ptr::null_mut(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
    }

    // The waiter's own flags word: a size bit is required, and nothing outside
    // the two bits Linux defines may be set. `__reserved` must be zero.
    {
        let mut no_size = [Waitv::new(&PAIR[0], 3, FUTEX_PRIVATE)];
        r.check(
            "a waiter without a size bit is EINVAL",
            matches!(
                waitv(no_size.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
        let mut unknown = [Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE | 0x1000)];
        r.check(
            "a waiter with an undefined flag is EINVAL",
            matches!(
                waitv(unknown.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
        let mut reserved = [Waitv::new(&PAIR[0], 3, FUTEX_32 | FUTEX_PRIVATE)];
        reserved[0].reserved = 1;
        r.check(
            "a non-zero __reserved is EINVAL",
            matches!(
                waitv(reserved.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
    }

    // Words the caller's own ABI constrains: alignment and readability.
    {
        let mut buf = [0u8; 16];
        // A 32-bit word at offset 1: the call is about the word's address, and
        // Linux refuses a word it cannot read as one.
        let unaligned = buf.as_mut_ptr().wrapping_add(1);
        let mut one = [Waitv {
            val: 5,
            uaddr: unaligned as u64,
            flags: FUTEX_32 | FUTEX_PRIVATE,
            reserved: 0,
        }];
        r.check(
            "a word at an unaligned address is EINVAL",
            matches!(
                waitv(one.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::INVAL)
            ),
        );
        r.check(
            "an unreadable waiters array is EFAULT",
            matches!(
                waitv(0x10 as *mut Waitv, 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::FAULT)
            ),
        );
        let mut one = [Waitv {
            val: 1,
            uaddr: 0x10,
            flags: FUTEX_32 | FUTEX_PRIVATE,
            reserved: 0,
        }];
        r.check(
            "an unreadable word is EFAULT",
            matches!(
                waitv(one.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::FAULT)
            ),
        );
    }

    // The order of the two halves: the value check comes before the deadline,
    // so a word that has already moved answers EAGAIN even when the deadline
    // has passed.
    {
        // PAIR[0] holds 3, so 2 is the value that has already moved.
        let mut one = [Waitv::new(&PAIR[0], 2, FUTEX_32 | FUTEX_PRIVATE)];
        let mut past = deadline(libc::CLOCK_MONOTONIC, 0);
        past.tv_sec -= 1;
        r.check(
            "a past deadline on a mismatched word is EAGAIN, not ETIMEDOUT",
            matches!(
                waitv(one.as_mut_ptr(), 1, 0, Some(&past), libc::CLOCK_MONOTONIC),
                Err(Errno::AGAIN)
            ),
        );
    }

    // The one named divergence: this kernel's wait table is keyed by user
    // address, so a shared word two processes map cannot be matched. The host
    // serves the shape, so the claim is only made here.
    if mpos_user_tests::host_run() {
        println!(
            "ok: the shared spelling is the host's to serve; this kernel names it a gap, \
             so the refusal is claimed on a guest only"
        );
    } else {
        let mut shared = [Waitv::new(&PAIR[0], 3, FUTEX_32)];
        r.check(
            "a shared waiter without FUTEX_PRIVATE_FLAG is EOPNOTSUPP (named gap)",
            matches!(
                waitv(shared.as_mut_ptr(), 1, 0, None, libc::CLOCK_MONOTONIC),
                Err(Errno::NOTSUP)
            ),
        );
    }

    r.finish()
}
