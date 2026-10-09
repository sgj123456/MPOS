//! The `sched` suite: context switch, round-robin, preemption, sleep, idle, and
//! priority-based core stealing.
//!
//! A child of `sched` and compiled only into a test build, but it uses the
//! module's *public* API rather than its private state - the honest answer here,
//! because everything these cases observe (`task_count`, `permanent_tasks`,
//! `exit_code`) has a reader outside the suite anyway. A child module would buy
//! nothing but coupling (`docs/NOTES.md` §15.21).
//!
//! The cases assert on [`RECORD`] - which task ran, in the order they ran - rather
//! than on the log, because interleaving is exactly what has to be proven: a boot
//! log that reads correctly is not evidence that two tasks took turns.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::case_check;
use crate::hal;
use crate::kunit::{Case, Phase, Suite};
use crate::mm::{PAGE_SIZE, frame_stats, heap_stats};
use crate::println;

use super::{
    MAX_TASKS, Priority, TaskId, discard_exit_code, exit_code, permanent_tasks, sleep_ticks, spawn,
    task_count, yield_now,
};

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "sched",
    phase: Phase::Scheduler,
    cases: &[
        Case {
            name: "alternating",
            run: alternating,
        },
        Case {
            name: "preempted",
            run: preempted,
        },
        Case {
            name: "sleeping",
            run: sleeping,
        },
        Case {
            name: "idling",
            run: idling,
        },
        Case {
            name: "priority_wins",
            run: priority_wins,
        },
        Case {
            name: "reaping",
            run: reaping,
        },
        Case {
            name: "stacks_returned",
            run: stacks_returned,
        },
    ],
};

/// Which task ran, in the order they ran.
static RECORD: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static RECORD_LEN: AtomicUsize = AtomicUsize::new(0);

/// Note that `who` ran. Called from inside the tasks these cases spawn.
///
/// This is the one place in the suite that still panics, and the reason is that
/// it runs in a *task*: a failure there has no case to return to. It is a
/// scaffold rather than a check - the record is sized so that it cannot fill
/// within a case's five or ten slices - and a machine that overran it is one whose
/// scheduler ran a task a hundred times more often than it was asked to.
fn record(who: u8) {
    critical_section::with(|_| {
        let index = RECORD_LEN.fetch_add(1, Ordering::Relaxed);
        assert!(index < RECORD.len(), "the run record is full");
        RECORD[index].store(who, Ordering::Relaxed);
    });
}

/// What [`record`] has collected since the last [`reset_record`].
fn recorded() -> Vec<u8> {
    RECORD
        .iter()
        .take(RECORD_LEN.load(Ordering::Relaxed))
        .map(|entry| entry.load(Ordering::Relaxed))
        .collect()
}

fn reset_record() {
    RECORD_LEN.store(0, Ordering::Relaxed);
}

/// Wait until every task this suite started has finished and been reaped.
///
/// `pub` rather than `pub(crate)` because the two suites that run after this one -
/// `vfs`'s and the syscall gate's - are in the *wiring layer's* checks directory
/// until they move, and a `pub(crate)` item does not cross that boundary. It
/// belongs here regardless: the helper is about the scheduler's task table, which
/// is this module's.
pub fn drain() {
    while task_count() > permanent_tasks() {
        discard_codes();
        sleep_ticks(1);
    }
}

/// Wait until `id` has left an exit code, by polling rather than sleeping.
pub fn wait_for_code(id: TaskId) {
    while exit_code(id).is_none() {
        sleep_ticks(1);
    }
}

/// Take every exit code nobody is going to ask for, so the slots they hold can be
/// freed.
fn discard_codes() {
    for slot in 2..MAX_TASKS - 1 {
        discard_exit_code(TaskId(slot));
    }
}

/// Two tasks at the same priority take turns.
fn alternating(_info: &crate::boot::BootInfo) -> Result<(), String> {
    reset_record();
    critical_section::with(|_| {
        for who in [0u8, 1] {
            spawn(
                move || {
                    for slice in 0..5 {
                        record(who);
                        println!("task {who} runs slice {slice}");
                        yield_now();
                    }
                },
                Priority::NORMAL,
            );
        }
    });
    drain();

    let got = recorded();
    let repeats = got.windows(2).filter(|pair| pair[0] == pair[1]).count();
    case_check!(
        repeats <= 2,
        "two tasks at the same priority did not take turns: {got:?}"
    );
    println!("two tasks alternated five slices each");
    Ok(())
}

/// A task that never yields still loses the core.
fn preempted(_info: &crate::boot::BootInfo) -> Result<(), String> {
    reset_record();
    const SPIN_TICKS: u64 = 10;

    spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::NORMAL,
    );
    spawn(
        || {
            for _ in 0..3 {
                record(1);
                yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    case_check!(
        recorded() == vec![1, 1, 1, 0],
        "a task that never yielded held the core to itself: {:?}",
        recorded()
    );
    println!("a task that never yielded was preempted for {SPIN_TICKS} ticks");
    Ok(())
}

/// What the sleeping task measured about its own sleep.
///
/// A task cannot return a value to the case that spawned it, so the number it
/// observed comes back through a static. It is the *whole* of the task's claim -
/// how long the sleep actually took - and asserting it in the case rather than
/// inside the task is what makes the failure name the number instead of panicking
/// the machine with it.
static SLEPT_TICKS: AtomicUsize = AtomicUsize::new(0);

/// A sleeping task is off the ready queue until the clock puts it back.
fn sleeping(_info: &crate::boot::BootInfo) -> Result<(), String> {
    reset_record();
    const SLEEP_TICKS: u64 = 10;

    spawn(
        || {
            record(0);
            let start = hal::ticks();
            sleep_ticks(SLEEP_TICKS);
            SLEPT_TICKS.store((hal::ticks() - start) as usize, Ordering::Relaxed);
            record(0);
        },
        Priority::NORMAL,
    );
    sleep_ticks(1);
    spawn(
        || {
            for _ in 0..3 {
                record(1);
                yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    let got = recorded();
    case_check!(
        got == vec![0, 1, 1, 1, 0],
        "a sleeping task kept running: {got:?}"
    );
    let slept = SLEPT_TICKS.load(Ordering::Relaxed) as u64;
    case_check!(
        (SLEEP_TICKS..SLEEP_TICKS + 3).contains(&slept),
        "asked for {SLEEP_TICKS} ticks of sleep and got {slept}"
    );
    println!("a task slept {slept} ticks while the other ran");
    Ok(())
}

/// The scheduler's idle path still wakes on the clock.
fn idling(_info: &crate::boot::BootInfo) -> Result<(), String> {
    reset_record();
    spawn(|| record(0), Priority::NORMAL);
    drain();

    record(1);
    sleep_ticks(5);
    record(1);

    let got = recorded();
    case_check!(
        got == vec![0, 1, 1],
        "the boot task did not come back from a sleep with nothing else runnable: {got:?}"
    );
    println!("the scheduler idled with nothing to run and the clock woke it");
    Ok(())
}

/// A higher band takes the core from a task that is already running.
fn priority_wins(_info: &crate::boot::BootInfo) -> Result<(), String> {
    reset_record();
    const SPIN_TICKS: u64 = 5;
    const WAKE_AFTER: u64 = 2;

    spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::LOW,
    );
    spawn(
        || {
            sleep_ticks(WAKE_AFTER);
            for _ in 0..2 {
                record(1);
                yield_now();
            }
        },
        Priority::HIGH,
    );
    drain();

    let got = recorded();
    case_check!(
        got == vec![1, 1, 0],
        "a HIGH task did not outrank a LOW one that held the core: {got:?}"
    );
    println!("a HIGH task took the core from a spinning LOW one");
    Ok(())
}

/// Every task the cases above started has been reaped.
///
/// An absolute claim rather than a delta, which is why it can be a case of its
/// own: "the alive count is the permanent count" needs no baseline, and the five
/// scenarios above each drained.
fn reaping(_info: &crate::boot::BootInfo) -> Result<(), String> {
    case_check!(
        task_count() == permanent_tasks(),
        "a task that exited was never reaped: {} alive, {} permanent",
        task_count(),
        permanent_tasks()
    );
    println!("every task reaped");
    Ok(())
}

/// A task's stack comes back to the frame allocator when the task is reaped.
///
/// Measured over a window this case owns - spawn, wait, look - rather than across
/// the whole family, and the reason is the shape of the claim: the original
/// compared the frames consumed by all five scenarios against the heap's growth
/// over the same span, so split into a case of its own it would compare a baseline
/// against whatever the state happened to be by the time the last scenario ended.
/// The claim itself is the same one: the *only* frames this window consumes are
/// the heap's own growth, so every stack came back.
fn stacks_returned(_info: &crate::boot::BootInfo) -> Result<(), String> {
    let before = (frame_stats().free, heap_stats().mapped);
    for _ in 0..4 {
        spawn(|| {}, Priority::NORMAL);
    }
    drain();
    let after = (frame_stats().free, heap_stats().mapped);
    let heap_growth = (after.1 - before.1) / PAGE_SIZE;
    case_check!(
        before.0 - after.0 == heap_growth,
        "a task stack was not returned to the frame allocator: {} frames consumed, \
         {} of them the heap's own growth",
        before.0 - after.0,
        heap_growth
    );
    println!("every stack returned to the frame allocator");
    Ok(())
}
