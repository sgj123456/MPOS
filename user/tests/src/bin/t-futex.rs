//! t-futex - the waiting primitive threads are built on.
//!
//! The man page has three things to say and this test pins all three: a wait
//! whose word no longer holds the expected value answers EAGAIN instead of
//! sleeping, a wake with nobody waiting answers zero, and - the part that
//! matters - a task that sleeps on a word is woken by another task that
//! changes it and wakes the address.
//!
//! The sleeping half needs a second thread, because that is the only way a
//! futex is ever used: the thread waits inside a *loop* that re-reads the word
//! after every return, which is exactly what a condition variable does and
//! what makes the handshake below immune to the order the two threads happen
//! to run in. Pinning "the wake counted 1" instead would pin a race.
//!
//! # The migration
//!
//! Four of the five futex calls this program makes are `rustix`'s:
//! `rustix::thread::futex` is a maintained binding of the call - `wait`,
//! `wake`, `wait_bitset` and `wake_bitset` - and its `Flags` carries
//! `FUTEX_PRIVATE_FLAG`, its `Errno::AGAIN` *is* the EAGAIN the man page names,
//! and its `val3` argument is a `NonZeroU32`, which is Linux's own "an empty
//! bitset is not a wake anyone could answer" written into the type.
//!
//! That last point is what sends exactly one claim back through `libc`, and it
//! is clause (c) of the crate header's rule - a wrapper whose *shape* cannot
//! express the claim. The claim is that `FUTEX_WAIT_BITSET` with `val3 == 0` is
//! `EINVAL`, and `NonZeroU32` has no value for zero: the number is reachable
//! only through `libc::syscall(libc::SYS_futex, ..)`, with the operation word
//! from `libc::FUTEX_WAIT_BITSET` and the number from `libc::SYS_futex` - 202 on
//! the first machine, 98 on the second. This is the same shape `t-lseek`'s bogus
//! `whence` and `t-fadvise`'s unnamed advice went through.
//!
//! The handshake's word is a `static AtomicU32` rather than the no-std version's
//! `static mut WORD`, because that is what `rustix`'s futex calls take (a
//! `&AtomicU32`, so that two threads sharing one word is not a data race), and
//! the second thread is `std::thread::spawn` rather than the no-std suite's
//! hand-mapped stack and `global_asm` stub: creating the thread is the library's
//! job and what the kernel owes it - a word both threads address alike, and a
//! sleep a wake can end - is what the handshake exercises. `t-clone` records the
//! same retirement from the other side.

use mpos_user_tests::Report;
use rustix::io::Errno;
use rustix::process::getpid;
use rustix::thread::futex;
use rustix::thread::gettid;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

/// The futex word: WAIT compares against it, WAKE wakes its waiters. The
/// predicate the two threads agree on is "it is no longer zero".
static WORD: AtomicU32 = AtomicU32::new(0);

/// What the waiting thread found when its wait returned: 0 not yet woken, -1
/// something other than a wake, 1 once it has reached the point of waiting at
/// all, and 2 once it has come back from a wait the caller ended.
static WAITED: AtomicI32 = AtomicI32::new(0);

/// What the thread answered for its own id and its process's.
static THREAD_TID: AtomicI32 = AtomicI32::new(0);
static THREAD_PID: AtomicI32 = AtomicI32::new(0);

/// How long the caller spins before giving up on a thread that should have
/// woken. A test that waits for ever is worse than a test that fails.
const SPIN_LIMIT: usize = 200_000;

fn spin_until(predicate: impl Fn() -> bool) -> bool {
    let mut spins = 0;
    while !predicate() {
        if spins >= SPIN_LIMIT {
            return false;
        }
        spins += 1;
        std::thread::yield_now();
    }
    true
}

/// The door returns `-1` and leaves the refusal in `errno`; the claim is about
/// what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// `FUTEX_WAIT_BITSET` with the empty bitset, which `rustix`'s `NonZeroU32`
/// cannot name - see the module comment. The address is the same word the typed
/// calls use, so the claim is about the operation and not about the word.
fn wait_bitset_with_empty_bitset() -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            &WORD as *const AtomicU32,
            libc::FUTEX_WAIT_BITSET as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        ) as isize
    }
}

fn main() {
    println!("t-futex");
    let mut r = Report::new();

    // A wait on a word that no longer holds the expected value does not sleep:
    // the value changed under the waiter, and EAGAIN is how it is told to look
    // at the word again.
    WORD.store(1, Ordering::SeqCst);
    r.check(
        "WAIT on a word that does not match is EAGAIN",
        matches!(
            futex::wait(&WORD, futex::Flags::empty(), 0, None),
            Err(Errno::AGAIN)
        ),
    );

    // A wake with nobody waiting wakes nobody - and is not an error.
    r.check(
        "WAKE with no waiter answers 0",
        matches!(futex::wake(&WORD, futex::Flags::empty(), 1), Ok(0)),
    );

    // The real thing: a thread sleeps on the word, the caller changes it and
    // wakes the address, and the thread comes back and says so.
    let handle = std::thread::spawn(|| {
        THREAD_TID.store(gettid().as_raw_nonzero().get(), Ordering::Relaxed);
        THREAD_PID.store(getpid().as_raw_nonzero().get(), Ordering::Relaxed);
        WAITED.store(1, Ordering::Release);
        loop {
            if WORD.load(Ordering::SeqCst) != 0 {
                WAITED.store(2, Ordering::Release);
                break;
            }
            match futex::wait(&WORD, futex::Flags::empty(), 0, None) {
                // Woken, or told to look at the word again: both mean the loop
                // re-reads it, which is what makes either order safe.
                Ok(()) | Err(Errno::AGAIN) => {}
                Err(_) => {
                    WAITED.store(-1, Ordering::Release);
                    break;
                }
            }
        }
    });

    let started = spin_until(|| WAITED.load(Ordering::Acquire) != 0);
    r.check("the thread is running", started);

    // The predicate changes first, then the wake: whichever of the two the
    // thread sees first, it cannot miss - that is what the loop is for. What the
    // wake *returns* is not checked, and deliberately: the thread may have gone
    // round the loop and seen the predicate before ever sleeping, in which case
    // the wake correctly finds nobody. Pinning "it counted 1" would pin that
    // race rather than the kernel.
    WORD.store(1, Ordering::SeqCst);
    let _ = futex::wake(&WORD, futex::Flags::empty(), 1);
    let came_back = spin_until(|| WAITED.load(Ordering::Acquire) == 2);
    r.check("the woken thread came back from its wait", came_back);
    r.check(
        "the thread shares the creator's memory",
        WORD.load(Ordering::SeqCst) == 1,
    );
    r.check(
        "the thread's tid is its own",
        THREAD_TID.load(Ordering::Relaxed) > 0
            && THREAD_TID.load(Ordering::Relaxed) != getpid().as_raw_nonzero().get(),
    );
    r.check(
        "and it answers the process's pid",
        THREAD_PID.load(Ordering::Relaxed) == getpid().as_raw_nonzero().get(),
    );
    r.check("the thread is joined", handle.join().is_ok());

    // The bitset form's fifth argument *is* the bitset, and an empty one names
    // no wake that could ever end the wait: Linux answers EINVAL rather than
    // sleeping a thread nothing can wake. Pinning that here is also what keeps
    // this test from parking for ever on a call whose answer it cannot time.
    r.check(
        "FUTEX_WAIT_BITSET with an empty bitset is EINVAL",
        refused(wait_bitset_with_empty_bitset(), libc::EINVAL),
    );

    // The bitset travels in the *sixth* argument - the fifth is uaddr2's,
    // unused by every wait. A bitset wait with a real bitset and a word that
    // already differs answers EAGAIN (the word changed, nothing slept); a
    // kernel reading the bitset from uaddr2's slot sees the zero there and
    // answers EINVAL instead, which is exactly what this pins against. The
    // `NonZeroU32` is the type saying an empty bitset is not a wait, so the
    // call the claim *can* make is the one with a real word in it.
    let bitset = NonZeroU32::new(0xffff_ffff).expect("nonzero");
    r.check(
        "FUTEX_WAIT_BITSET with a real bitset and a changed word is EAGAIN",
        matches!(
            futex::wait_bitset(&WORD, futex::Flags::empty(), 0, None, bitset),
            Err(Errno::AGAIN)
        ),
    );
    r.check(
        "FUTEX_WAKE_BITSET with a real bitset wakes nobody, not EINVAL",
        matches!(
            futex::wake_bitset(&WORD, futex::Flags::empty(), 1, bitset),
            Ok(0)
        ),
    );

    r.finish()
}
