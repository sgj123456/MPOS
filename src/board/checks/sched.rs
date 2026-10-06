//! `M4`: the scheduler - context switch, round-robin, preemption, sleep, priority.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use alloc::vec;
use alloc::vec::Vec;

use mpos::hal;
use mpos::mm::{self, PAGE_SIZE};
use mpos::println;
use mpos::sched::{self, Priority};

/// Which task ran, in the order they ran.
///
/// The checks below assert on this rather than on the log, because interleaving
/// is precisely the thing that has to be proven: a boot log that looks right is
/// not evidence that two tasks took turns.
static RECORD: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static RECORD_LEN: AtomicUsize = AtomicUsize::new(0);

fn record(who: u8) {
    critical_section::with(|_| {
        let index = RECORD_LEN.fetch_add(1, Ordering::Relaxed);
        assert!(index < RECORD.len(), "the run record is full");
        RECORD[index].store(who, Ordering::Relaxed);
    });
}

pub(super) fn recorded() -> Vec<u8> {
    RECORD
        .iter()
        .take(RECORD_LEN.load(Ordering::Relaxed))
        .map(|entry| entry.load(Ordering::Relaxed))
        .collect()
}

fn reset_record() {
    RECORD_LEN.store(0, Ordering::Relaxed);
}

/// Wait until every task this milestone started has finished and been reaped.
pub(super) fn drain() {
    while sched::task_count() > sched::permanent_tasks() {
        discard_codes();
        sched::sleep_ticks(1);
    }
}

/// Wait until `id` has left an exit code, by polling rather than sleeping.
pub(super) fn wait_for_code(id: sched::TaskId) {
    while sched::exit_code(id).is_none() {
        sched::sleep_ticks(1);
    }
}

/// Take every exit code nobody is going to ask for, so the slots they hold
/// can be freed.
fn discard_codes() {
    for slot in 2..sched::MAX_TASKS - 1 {
        sched::discard_exit_code(sched::TaskId(slot));
    }
}

/// Two tasks at the same priority take turns.
fn alternating() {
    reset_record();
    critical_section::with(|_| {
        for who in [0u8, 1] {
            sched::spawn(
                move || {
                    for slice in 0..5 {
                        record(who);
                        println!("task {who} runs slice {slice}");
                        sched::yield_now();
                    }
                },
                Priority::NORMAL,
            );
        }
    });
    drain();

    let got = recorded();
    let repeats = got.windows(2).filter(|pair| pair[0] == pair[1]).count();
    assert!(
        repeats <= 2,
        "two tasks at the same priority did not take turns: {got:?}"
    );
    println!("two tasks alternated five slices each");
}

/// A task that never yields still loses the core.
fn preempted() {
    reset_record();
    const SPIN_TICKS: u64 = 10;

    sched::spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::NORMAL,
    );
    sched::spawn(
        || {
            for _ in 0..3 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![1, 1, 1, 0],
        "a task that never yielded held the core to itself"
    );
    println!("a task that never yielded was preempted for {SPIN_TICKS} ticks");
}

/// A sleeping task is off the ready queue until the clock puts it back.
fn sleeping() {
    reset_record();
    const SLEEP_TICKS: u64 = 10;

    sched::spawn(
        || {
            record(0);
            let start = hal::ticks();
            sched::sleep_ticks(SLEEP_TICKS);
            let slept = hal::ticks() - start;
            assert!(
                (SLEEP_TICKS..SLEEP_TICKS + 3).contains(&slept),
                "asked for {SLEEP_TICKS} ticks and got {slept}"
            );
            record(0);
        },
        Priority::NORMAL,
    );
    sched::sleep_ticks(1);
    sched::spawn(
        || {
            for _ in 0..3 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![0, 1, 1, 1, 0],
        "a sleeping task kept running"
    );
    println!("a task slept {SLEEP_TICKS} ticks while the other ran");
}

/// The scheduler's idle path still wakes on the clock.
fn idling() {
    reset_record();
    sched::spawn(|| record(0), Priority::NORMAL);
    drain();

    record(1);
    sched::sleep_ticks(5);
    record(1);

    assert_eq!(
        recorded(),
        vec![0, 1, 1],
        "the boot task did not come back from a sleep with nothing else runnable"
    );
    println!("the scheduler idled with nothing to run and the clock woke it");
}

/// A higher band takes the core from a task that is already running.
fn priority_wins() {
    reset_record();
    const SPIN_TICKS: u64 = 5;
    const WAKE_AFTER: u64 = 2;

    sched::spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::LOW,
    );
    sched::spawn(
        || {
            sched::sleep_ticks(WAKE_AFTER);
            for _ in 0..2 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::HIGH,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![1, 1, 0],
        "a HIGH task did not outrank a LOW one that held the core"
    );
    println!("a HIGH task took the core from a spinning LOW one");
}

/// Prove the scheduler: context switch, round-robin, preemption, sleep, idle,
/// and priority-based core stealing.
pub(crate) fn run() {
    let before = (mm::frame_stats().free, mm::heap_stats().mapped);
    alternating();
    preempted();
    sleeping();
    idling();
    priority_wins();

    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "a task that exited was never reaped"
    );

    let after = (mm::frame_stats().free, mm::heap_stats().mapped);
    let heap_growth = (after.1 - before.1) / PAGE_SIZE;
    assert_eq!(
        before.0 - after.0,
        heap_growth,
        "a task stack was not returned to the frame allocator"
    );
    println!("every task reaped, every stack returned");
    println!("M4_OK");
}
