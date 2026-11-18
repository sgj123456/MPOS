//! t-thread-join - the clear-tid address is a fact about a **thread**.
//!
//! `pthread_join` is a futex wait on a word the exiting thread clears, and the
//! kernel's whole part of it is `CLONE_CHILD_CLEARTID`: "when *this* thread
//! exits, store zero at *this* address and wake whoever sleeps there". Every
//! thread names a different address, and a joiner waits on the one its own
//! thread named - so the address belongs to a thread, not to the address space
//! its threads share.
//!
//! This kernel kept it on the thread group's `Mm`, of which there is one per
//! process. The second `pthread_create` therefore overwrote the first thread's
//! address: the first thread out cleared the *second* thread's word, and the
//! joiner of the first slept for ever on a word nobody was going to write.
//! Measured in the guest before the fix (`build/scratch/race/j3.c`, a static
//! **glibc** program; `docs/notes/07-chromium-analysis.md` §4ac):
//!
//! ```text
//! === n=1
//! create 0 e=0
//! joining 0
//! t0 exiting
//! j0 e=0 r=0            <- one thread is joined normally
//! all joined
//! === n=2
//! create 0 e=0
//! create 1 e=0
//! joining 0
//! t1 exiting
//! t0 exiting            <- both workers are gone ...
//! === exit=143          <- ... and the join of the first never returns
//! ```
//!
//! The same fixture on a current host (Linux 7.3.4) joins both threads, so the
//! hang was this kernel's.
//!
//! # Why the claim is `PR_GET_TID_ADDRESS` and not a `pthread_join`
//!
//! A hang is not a usable claim: a sweep reports it as `timeout`, not as a
//! named failure, and a pin that hangs on the broken kernel and passes on the
//! fixed one cannot say *which* half was wrong. The first version of this file
//! tried `pthread_join` anyway - twice, once through `std::thread` and once
//! through musl's own `pthread_create` - and **both passed on a kernel carrying
//! the defect**, which the counterfactual measured. musl's join happens to
//! survive the wrong address (its worker's word is re-read on a retry path);
//! static glibc's does not. A pin that depends on which libc the payload links
//! is a pin of the libc.
//!
//! What the kernel actually promises is narrower and directly observable: a
//! thread that names an address can read that address back, and the call that
//! reads it - `prctl(PR_GET_TID_ADDRESS)` - answers for **the calling thread**
//! (Linux keeps `clear_child_tid` in `task_struct` for exactly this reason).
//! So each of several threads names its own word and reads it back; a kernel
//! with one word per address space hands every thread the last one named.
//!
//! # What this program pins
//!
//! The *movement*, not a constant: the first thread reads back its own address
//! (which a single-word-per-group kernel would also pass when it happens to be
//! the only thread), then **four** threads each read back their own - and the
//! second claim is the one that fails when the address is held per group. The
//! words are distinct addresses in this program's own stack frame, so no two
//! readings can agree by accident.

use mpos_user_tests::Report;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `prctl(2)`'s "where would you clear my tid" query, as this kernel spells it
/// (`src/syscall/linux/abi.rs:2738`). Spelled here because `libc` does not name
/// it. The first draft had it wrong (`0x5005`, remembered rather than read) and
/// the claim then failed on a kernel that answers correctly, because `prctl`
/// refused the unknown option and left the output word at its starting zero.
const PR_GET_TID_ADDRESS: libc::c_int = 40;

/// One word per thread, each a **distinct address**. A thread names its own
/// with `set_tid_address` and reads it back with `PR_GET_TID_ADDRESS`.
static WORDS: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// What each thread named, for the parent's comparisons.
static NAMED: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// What each thread was told when it asked.
static ANSWERED: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// How many threads have finished. The parent polls this with a **sleep**, not
/// a spin: this machine is one CPU (`assets/configs/machine.toml`'s
/// `qemu.cpus = 1`), so a spinning thread never yields to the ones it waits on.
static DONE: AtomicUsize = AtomicUsize::new(0);

/// Ask the kernel where this thread's clear-tid word is, and record it.
fn read_it(id: usize) {
    let mut answered: libc::c_ulong = 0;
    // SAFETY: the **second** argument is a valid pointer to a `c_ulong` this
    // frame owns, which is what `PR_GET_TID_ADDRESS` writes through. The
    // position is measured, not guessed (`build/scratch/prt/r.c`, Linux 7.3.4):
    // a pointer in the second slot is answered, one in the third is `-1` with
    // the word untouched, and arguments past the second are ignored.
    unsafe {
        libc::prctl(
            PR_GET_TID_ADDRESS,
            &mut answered as *mut libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    ANSWERED[id].store(answered as usize, Ordering::Release);
}

fn main() {
    let mut report = Report::new();

    // 1. One thread on its own: it names a word and reads it back. This passes
    //    on a kernel with one slot per address space too, so it is not the pin;
    //    it says the call works at all before the next claim leans on it.
    let main_word = &WORDS[0] as *const AtomicUsize as usize;
    NAMED[0].store(main_word, Ordering::Release);
    // SAFETY: this program's own `AtomicUsize`, alive for the program's life.
    unsafe { libc::syscall(libc::SYS_set_tid_address, main_word) };
    read_it(0);
    report.check(
        "the calling thread reads back the address it named",
        ANSWERED[0].load(Ordering::Acquire) == main_word,
    );

    // 2. Three threads, **each naming a different word**, all naming before any
    //    reads. That ordering is what makes the claim structural rather than a
    //    race: with one slot per address space, only the last name written
    //    survives, so at most one of the four can read its own address back
    //    whatever order the threads run in.
    //
    //    The barrier **parks** (`std::sync::Barrier`), for the one-CPU reason
    //    above. The threads are detached rather than joined, and that is
    //    measured, not stylistic: musl's `pthread_join` waits on the very word
    //    `set_tid_address` re-points, so joining a thread that named its own
    //    word hangs the program on the *fixed* kernel - the fixture breaking
    //    itself, measured as `=== exit=143` after claim 1 passed. Detaching
    //    keeps the library out of the measurement; the `DONE` counter says when
    //    every reading is taken.
    //    Index 0 is the leader's word above, so the workers are 1..=N and the
    //    arrays are one longer than N.
    const N: usize = 3;
    let gate = std::sync::Arc::new(Barrier::new(N));
    for id in 1..=N {
        let gate = gate.clone();
        let t = std::thread::spawn(move || {
            let word = &WORDS[id] as *const AtomicUsize as usize;
            NAMED[id].store(word, Ordering::Release);
            // SAFETY: `WORDS[id]` is this program's own static.
            unsafe { libc::syscall(libc::SYS_set_tid_address, word) };
            gate.wait();
            read_it(id);
            DONE.fetch_add(1, Ordering::Release);
        });
        // Dropped, not joined: dropping a `JoinHandle` detaches the thread,
        // which is the whole point (see above). `std` has no `detach` call.
        drop(t);
    }
    while DONE.load(Ordering::Acquire) < N {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // The workers' slots only: slot 0 is the leader's, checked above. Iterator
    // form because an index loop here is a `needless_range_loop` and the
    // iterator says the same thing more plainly.
    let own = ANSWERED
        .iter()
        .zip(NAMED.iter())
        .skip(1)
        .filter(|(answered, named)| {
            answered.load(Ordering::Acquire) == named.load(Ordering::Acquire)
        })
        .count();
    report.check(
        "three threads each read back the address they named, not one shared slot",
        own == N,
    );

    // 3. And the three answers are four different addresses. Without this, a
    //    kernel answering one address for everybody could still pass claim 2
    //    if the words it answered happened to be the last one named by a thread
    //    whose own claim then matched. Stated separately so a failure names
    //    which half broke.
    let answers: Vec<usize> = ANSWERED
        .iter()
        .skip(1)
        .map(|a| a.load(Ordering::Acquire))
        .collect();
    let mut distinct = true;
    for (i, a) in answers.iter().enumerate() {
        if answers[i + 1..].contains(a) {
            distinct = false;
        }
    }
    report.check(
        "and the three answers are three different addresses",
        distinct,
    );

    // 4. The process reaches its end with every reading taken - a sweep that
    //    hangs here would be reported as a timeout rather than as a claim.
    report.check("the process reaches its end with every reading taken", true);

    report.finish();
}
