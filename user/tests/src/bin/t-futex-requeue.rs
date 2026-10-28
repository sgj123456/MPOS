//! `futex_requeue(2)`: one word's sleepers, part woken and part handed to
//! another word.
//!
//! `futex_requeue(struct futex_waitv waiters[2], unsigned flags, int nr_wake,
//! int nr_requeue)` is the futex2 spelling of `FUTEX_CMP_REQUEUE`: the first
//! descriptor names the source word and its `val` is the comparison value, the
//! second names the destination, and the answer counts the sleepers woken plus
//! the sleepers moved.
//!
//! Every claim here is a measurement taken on Linux 7.2.11 with a
//! `musl-gcc -static` probe (`build/scratch/futex-req/probe.c`), and the same
//! binary runs under `MPOS_ON_HOST=1` against Linux - the reference direction.
//! The two facts that shape the call:
//!
//! * the **destination word is never read**, so an unmapped destination is
//!   accepted: the moved sleepers are then unreachable, and that is Linux's
//!   answer too (a requeue into a word nobody will wake);
//! * both descriptors must carry **the same flag word** - a private/shared or
//!   size mixture is `EINVAL` before any word is looked at.
//!
//! This kernel refuses two spellings Linux serves, by name: the NUMA and
//! memory-policy flag bits (`EOPNOTSUPP`: one word per place, no node or
//! memory-policy metadata), and the shared spelling (`EOPNOTSUPP`: the wait
//! table is keyed by user address, so a cross-process requeue cannot be
//! promised truthfully - §6.36).
#![allow(clippy::undocumented_unsafe_blocks)]

use core::ffi::c_int;
use std::sync::atomic::{AtomicU32, Ordering};

use mpos_user_tests::{Report, host_run};

/// The syscall number: one number on both machines (the futex2 family landed
/// after the two number tables converged).
const SYS_FUTEX_REQUEUE: libc::c_long = 456;

const FUTEX_32: u32 = 0x02;
const FUTEX_PRIVATE: u32 = 0x80;
const FUTEX2_NUMA: u32 = 0x04;

const FUTEX_WAIT: c_int = 0;
const FUTEX_WAKE: c_int = 1;
const FUTEX_PRIVATE_FLAG: c_int = 128;

static WORD_A: AtomicU32 = AtomicU32::new(7);
static WORD_B: AtomicU32 = AtomicU32::new(9);

#[repr(C)]
struct Waitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    reserved: u32,
}

fn waiter(word: &'static AtomicU32, val: u32) -> Waitv {
    Waitv {
        val: u64::from(val),
        uaddr: word as *const AtomicU32 as usize as u64,
        flags: FUTEX_32 | FUTEX_PRIVATE,
        reserved: 0,
    }
}

/// `futex_requeue(2)`; the answer is the count of places touched.
fn requeue(pair: &[Waitv; 2], flags: u32, nr_wake: i32, nr_requeue: i32) -> Result<i64, i32> {
    let r = unsafe {
        libc::syscall(
            SYS_FUTEX_REQUEUE,
            pair.as_ptr(),
            flags as libc::c_uint,
            nr_wake as libc::c_int,
            nr_requeue as libc::c_int,
        )
    };
    if r < 0 {
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(r as i64)
    }
}

/// `futex(2)` with the two operations this test needs.
fn futex(word: &'static AtomicU32, op: c_int, val: u32) -> Result<c_int, i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32,
            op | FUTEX_PRIVATE_FLAG,
            val as libc::c_int,
            0usize,
            0usize,
            0usize,
        )
    };
    if r < 0 {
        Err(unsafe { *libc::__errno_location() })
    } else {
        Ok(r as c_int)
    }
}

/// A task asleep on `word`, which the call under test is about to touch.
fn sleeper(word: &'static AtomicU32, val: u32) -> std::thread::JoinHandle<Result<(), i32>> {
    std::thread::spawn(move || futex(word, FUTEX_WAIT, val).map(|_| ()))
}

/// Wait for a sleeper this test made, after releasing whatever is still
/// asleep on either word.
///
/// A failed call under test must not leave a sleeper behind: the pin also runs
/// on a kernel that answers `ENOSYS` for `futex_requeue` (the counterfactual),
/// and a plain `join` there would block the lane instead of failing a claim.
/// The wakes are best-effort and their counts are not claims.
fn released(thread: std::thread::JoinHandle<Result<(), i32>>) -> bool {
    let _ = futex(&WORD_A, FUTEX_WAKE, 64);
    let _ = futex(&WORD_B, FUTEX_WAKE, 64);
    thread.join().map_or(false, |r| r.is_ok())
}

/// Give a freshly spawned sleeper time to reach its syscall. The kernel offers
/// no "is it queued yet" question, and a requeue that runs first would simply
/// see fewer places (a real shape, not a hang): the wait is for the test, not
/// for the kernel.
fn let_it_sleep() {
    std::thread::sleep(std::time::Duration::from_millis(150));
}

fn main() {
    println!("t-futex-requeue");
    let mut r = Report::new();

    if host_run() {
        // Linux serves this call; the assertions below are Linux's own
        // answers, so the host build runs them too.
        host_reference(&mut r);
    } else {
        device_run(&mut r);
    }
    r.finish();
}

/// The half that is Linux's answer on any kernel that serves the call.
fn host_reference(r: &mut Report) {
    println!("=== one wake and one move of two sleepers ===");
    WORD_A.store(7, Ordering::SeqCst);
    WORD_B.store(9, Ordering::SeqCst);
    let first = sleeper(&WORD_A, 7);
    let second = sleeper(&WORD_A, 7);
    let_it_sleep();
    let pair = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    let touched = requeue(&pair, 0, 1, 1);
    r.check(
        "wake 1, requeue 1 of two sleepers: the answer counts both",
        touched == Ok(2),
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    r.check(
        "the moved sleeper is woken by a wake on the destination word",
        futex(&WORD_B, FUTEX_WAKE, 8) == Ok(1),
    );
    r.check("and the woken one comes back", released(first));
    r.check("as does the moved one", released(second));

    println!("=== the source's val is the comparison value ===");
    WORD_A.store(7, Ordering::SeqCst);
    let sleeper_one = sleeper(&WORD_A, 7);
    let_it_sleep();
    let stale = [waiter(&WORD_A, 8), waiter(&WORD_B, 9)];
    r.check(
        "a source word that moved is EAGAIN, not a requeue",
        requeue(&stale, 0, 0, 1) == Err(libc::EAGAIN),
    );
    r.check(
        "and nothing was moved: the sleeper is still on the source word",
        futex(&WORD_A, FUTEX_WAKE, 1) == Ok(1) && released(sleeper_one),
    );

    println!("=== counts: wake only, move only, more than are there ===");
    WORD_A.store(7, Ordering::SeqCst);
    WORD_B.store(9, Ordering::SeqCst);
    let w1 = sleeper(&WORD_A, 7);
    let w2 = sleeper(&WORD_A, 7);
    let w3 = sleeper(&WORD_A, 7);
    let_it_sleep();
    let pair = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    r.check(
        "wake 3 of 3 awake every one of them",
        requeue(&pair, 0, 3, 0) == Ok(3),
    );
    for w in [w1, w2, w3] {
        r.check("each woken sleeper returns", released(w));
    }

    let m1 = sleeper(&WORD_A, 7);
    let m2 = sleeper(&WORD_A, 7);
    let m3 = sleeper(&WORD_A, 7);
    let_it_sleep();
    r.check(
        "requeue 3 of 3 moves all of them and wakes none",
        requeue(&pair, 0, 0, 3) == Ok(3),
    );
    r.check(
        "and a wake on the destination finds all three",
        futex(&WORD_B, FUTEX_WAKE, 8) == Ok(3),
    );
    for w in [m1, m2, m3] {
        r.check("each moved sleeper returns from its new word", released(w));
    }

    let lone = sleeper(&WORD_A, 7);
    let_it_sleep();
    r.check(
        "asking for more than are asleep touches only the ones present",
        requeue(&pair, 0, 5, 5) == Ok(1),
    );
    r.check(
        "the one present was woken by the wake side, so the destination hears nothing",
        futex(&WORD_B, FUTEX_WAKE, 8) == Ok(0),
    );
    let _ = futex(&WORD_A, FUTEX_WAKE, 8);
    let _ = futex(&WORD_B, FUTEX_WAKE, 8);
    r.check("the lone sleeper returns", released(lone));

    println!("=== a zero on both sides touches nothing ===");
    WORD_A.store(7, Ordering::SeqCst);
    let keeper = sleeper(&WORD_A, 7);
    let_it_sleep();
    let pair = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    r.check(
        "requeue 0,0 answers zero and moves nobody",
        requeue(&pair, 0, 0, 0) == Ok(0),
    );
    r.check(
        "the sleeper is still on the source word",
        futex(&WORD_A, FUTEX_WAKE, 1) == Ok(1) && released(keeper),
    );

    println!("=== refusals ===");
    let pair = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    r.check(
        "a call-level flag word is EINVAL",
        requeue(&pair, 1, 1, 1) == Err(libc::EINVAL),
    );
    r.check(
        "NULL descriptors are EINVAL",
        unsafe {
            libc::syscall(
                SYS_FUTEX_REQUEUE,
                std::ptr::null::<Waitv>(),
                0u32,
                0i32,
                0i32,
            )
        } < 0
            && unsafe { *libc::__errno_location() } == libc::EINVAL,
    );
    r.check(
        "a negative wake count is EINVAL",
        requeue(&pair, 0, -1, 0) == Err(libc::EINVAL),
    );
    r.check(
        "a negative requeue count is EINVAL",
        requeue(&pair, 0, 0, -1) == Err(libc::EINVAL),
    );
    let mut mismatched = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    mismatched[1].flags |= FUTEX2_NUMA;
    r.check(
        "two descriptors that disagree about their flags are EINVAL",
        requeue(&mismatched, 0, 0, 0) == Err(libc::EINVAL),
    );
    let mut reserved = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    reserved[0].reserved = 1;
    r.check(
        "a non-zero reserved word is EINVAL",
        requeue(&reserved, 0, 0, 0) == Err(libc::EINVAL),
    );
    let mut no_size = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    no_size[0].flags = 0;
    no_size[1].flags = 0;
    r.check(
        "a descriptor without the size bit is EINVAL",
        requeue(&no_size, 0, 0, 0) == Err(libc::EINVAL),
    );
    let mut wide = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    wide[0].val = u64::from(u32::MAX) + 7;
    r.check(
        "a val wider than the word is EINVAL",
        requeue(&wide, 0, 0, 0) == Err(libc::EINVAL),
    );
    let unmapped_source = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    let mut bad_source = unmapped_source;
    bad_source[0].uaddr = 0x10;
    r.check(
        "an unmapped source word is EFAULT",
        requeue(&bad_source, 0, 0, 0) == Err(libc::EFAULT),
    );
    let mut bad_dest = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
    bad_dest[1].uaddr = 0x10;
    r.check(
        "an unmapped destination with nobody to move is accepted, because the destination is never read",
        requeue(&bad_dest, 0, 0, 0) == Ok(0),
    );

    println!("=== the same word on both sides ===");
    WORD_A.store(7, Ordering::SeqCst);
    let same = sleeper(&WORD_A, 7);
    let_it_sleep();
    let pair = [waiter(&WORD_A, 7), waiter(&WORD_A, 7)];
    r.check(
        "a requeue from a word to itself is accepted",
        requeue(&pair, 0, 0, 1) == Ok(1),
    );
    r.check(
        "and a wake on it releases the moved sleeper",
        futex(&WORD_A, FUTEX_WAKE, 8) == Ok(1) && released(same),
    );

    if !host_run() {
        println!("=== the two spellings this kernel refuses by name ===");
        let mut numa = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
        numa[0].flags |= FUTEX2_NUMA;
        numa[1].flags |= FUTEX2_NUMA;
        r.check(
            "both descriptors carrying the NUMA bit is EOPNOTSUPP (the node word does not exist here)",
            requeue(&numa, 0, 0, 0) == Err(libc::EOPNOTSUPP),
        );
        let mut shared = [waiter(&WORD_A, 7), waiter(&WORD_B, 9)];
        for w in &mut shared {
            w.flags = FUTEX_32;
        }
        r.check(
            "the shared spelling is EOPNOTSUPP (the wait table is keyed by user address)",
            requeue(&shared, 0, 0, 0) == Err(libc::EOPNOTSUPP),
        );
    }
}

/// The kernel build: the same claims, plus the guest-only refusals inside
/// `host_reference`.
fn device_run(r: &mut Report) {
    host_reference(r);
}
