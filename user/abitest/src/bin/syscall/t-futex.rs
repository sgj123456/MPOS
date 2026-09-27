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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The futex word: WAIT compares against it, WAKE wakes its waiters. The
/// predicate the two threads agree on is "it is no longer zero".
static mut WORD: i32 = 0;

/// What the waiting thread found when its wait returned: 0 woken, -1 something
/// else, and 1 once it has reached the point of waiting at all.
static mut WAITED: i32 = 0;

/// What the thread answered for its own id and its process's.
static mut THREAD_TID: i32 = 0;
static mut THREAD_PID: i32 = 0;

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
        lin::sched_yield();
    }
    true
}

/// The thread's whole life: say who it is, then wait in a loop - re-reading
/// the word after every return, the way a condition variable does, so that
/// whichever of the two threads runs first, neither can miss the other.
extern "C" fn thread_body(_arg: usize) -> ! {
    unsafe {
        THREAD_TID = lin::gettid() as i32;
        THREAD_PID = lin::getpid() as i32;
        WAITED = 1;
    }
    loop {
        if unsafe { WORD } != 0 {
            unsafe { WAITED = 2 };
            break;
        }
        let r = lin::futex(&raw mut WORD, lin::nr::FUTEX_WAIT, 0);
        if r != 0 && r != lin::EAGAIN {
            unsafe { WAITED = -1 };
            break;
        }
    }
    // Only this thread ends: its creator is still running.
    lin::exit(0)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-futex\n");
    let mut failed = false;
    let word = &raw mut WORD;

    // A wait on a word that no longer holds the expected value does not sleep:
    // the value changed under the waiter, and EAGAIN is how it is told to look
    // at the word again.
    unsafe { WORD = 1 };
    lin::check_eq(
        &mut failed,
        b"WAIT on a word that does not match is EAGAIN",
        lin::futex(word, lin::nr::FUTEX_WAIT, 0),
        lin::EAGAIN,
    );

    // A wake with nobody waiting wakes nobody - and is not an error.
    lin::check_eq(
        &mut failed,
        b"WAKE with no waiter answers 0",
        lin::futex(word, lin::nr::FUTEX_WAKE, 1),
        0,
    );

    // The real thing: a thread sleeps on the word, the caller changes it and
    // wakes the address, and the thread comes back and says so.
    let stack_top = lin::thread_stack_top();
    lin::check_true(
        &mut failed,
        b"a stack was mapped for the thread",
        stack_top > 0,
    );
    if stack_top <= 0 {
        lin::done(failed);
    }

    unsafe { WORD = 0 };
    let mut tid_slot: i32 = 0;
    let tid = lin::thread_create(stack_top as usize, thread_body, 0, &mut tid_slot);

    lin::check_true(&mut failed, b"clone(CLONE_THREAD) answered a tid", tid > 0);
    if tid > 0 {
        let thread_started = spin_until(|| unsafe { WAITED } != 0);
        lin::check_true(&mut failed, b"the thread is running", thread_started);
        // The tid is written *by the child*, when it first runs - so it is read
        // here, after the child has said it is up, and not before.
        lin::check_eq(
            &mut failed,
            b"the thread tid was parked where the caller named",
            tid_slot as isize,
            tid,
        );

        // The predicate changes first, then the wake: whichever of the two the
        // thread sees first, it cannot miss - that is what the loop is for.
        unsafe { WORD = 1 };
        lin::futex(word, lin::nr::FUTEX_WAKE, 1);

        let woke = spin_until(|| unsafe { WAITED } == 2);
        lin::check_true(
            &mut failed,
            b"the woken thread came back from its wait",
            woke,
        );
        lin::check_eq(
            &mut failed,
            b"the thread shares the creator's memory",
            unsafe { WORD } as isize,
            1,
        );
        lin::check_eq(
            &mut failed,
            b"the thread's tid is its own",
            unsafe { THREAD_TID } as isize,
            tid,
        );
        lin::check_eq(
            &mut failed,
            b"and it answers the process's pid",
            unsafe { THREAD_PID } as isize,
            lin::getpid(),
        );
    }

    // The bitset form's fifth argument *is* the bitset, and an empty one names
    // no wake that could ever end the wait: Linux answers EINVAL rather than
    // sleeping a thread nothing can wake. Pinning that here is also what keeps
    // this test from parking for ever on a call whose answer it cannot time.
    lin::check_eq(
        &mut failed,
        b"FUTEX_WAIT_BITSET with an empty bitset is EINVAL",
        lin::futex(word, lin::nr::FUTEX_WAIT_BITSET, 0),
        lin::EINVAL,
    );

    // The bitset travels in the *sixth* argument - the fifth is uaddr2's,
    // unused by every wait. A bitset wait with a real bitset and a word that
    // already differs answers EAGAIN (the word changed, nothing slept); a
    // kernel reading the bitset from uaddr2's slot sees the zero there and
    // answers EINVAL instead, which is exactly what these pin against.
    let mut bitset_word: i32 = 1;
    lin::check_eq(
        &mut failed,
        b"FUTEX_WAIT_BITSET with a real bitset and a changed word is EAGAIN",
        lin::futex6(
            &raw mut bitset_word,
            lin::nr::FUTEX_WAIT_BITSET,
            0,
            0,
            0,
            0xffff_ffff,
        ),
        lin::EAGAIN,
    );
    lin::check_eq(
        &mut failed,
        b"FUTEX_WAKE_BITSET with a real bitset wakes nobody, not EINVAL",
        lin::futex6(
            &raw mut bitset_word,
            lin::nr::FUTEX_WAKE_BITSET,
            1,
            0,
            0,
            0xffff_ffff,
        ),
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-futex: panicked\n")
}
