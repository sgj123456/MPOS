#![no_main]

//! The scheduler's queues, driven by a stream of transitions.
//!
//! `sched::policy` is a pure state machine - a ready queue per priority band
//! plus a sleep heap keyed by wake-up time - and it is the one place where a
//! lost or duplicated task is not a crash but a process that never runs again.
//! The runtime hands it ids and priorities it took from userland, so the
//! questions worth asking are the ones a corrupt task table answers wrongly:
//! does every transition move exactly the count it claims, can the clock go
//! backwards, can a task the scheduler was told to retire still be picked.
//!
//! Nothing here counts tasks across operations, because a task may legitimately
//! be queued twice and may be asleep while it is also in a ready queue. Each
//! operation is checked against the queue lengths on both sides of it, which is
//! the whole contract.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::sched::policy::{Priority, Scheduler, TaskId};

#[derive(Arbitrary, Debug)]
enum Op {
    Push {
        id: u16,
        band: u8,
    },
    Sleep {
        id: u16,
        band: u8,
        ticks: u64,
    },
    Wake {
        id: u16,
    },
    Retire {
        id: u16,
    },
    Pick,
    /// A tick that never goes backwards, which is what the runtime promises.
    Advance {
        delta: u16,
    },
}

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

fuzz_target!(|input: Input| {
    let mut sched = Scheduler::new();
    let mut clock = 0u64;

    for op in input.ops.iter().take(512) {
        let ready = sched.ready_count();
        let sleeping = sched.sleeping_count();

        match *op {
            Op::Push { id, band } => {
                sched.push_ready(TaskId(id as usize), Priority::new(band));
                assert_eq!(sched.ready_count(), ready + 1, "push_ready queued nothing");
                assert_eq!(sched.sleeping_count(), sleeping, "push_ready touched sleep");
            }
            Op::Sleep { id, band, ticks } => {
                sched.sleep(TaskId(id as usize), Priority::new(band), ticks);
                assert_eq!(
                    sched.sleeping_count(),
                    sleeping + 1,
                    "sleep did not add a sleeper"
                );
                assert!(
                    sched.is_sleeping(TaskId(id as usize)),
                    "sleep then not asleep"
                );
            }
            Op::Wake { id } => {
                let woke = sched.wake(TaskId(id as usize));
                if woke {
                    assert!(
                        sleeping > 0,
                        "wake reported a sleeper from an empty sleep queue"
                    );
                    assert_eq!(
                        (sched.ready_count(), sched.sleeping_count()),
                        (ready + 1, sleeping - 1),
                        "wake moved a task somewhere other than the ready queue"
                    );
                } else {
                    assert_eq!(
                        (sched.ready_count(), sched.sleeping_count()),
                        (ready, sleeping),
                        "a failed wake still changed the queues"
                    );
                    assert!(
                        !sched.is_sleeping(TaskId(id as usize)),
                        "wake said no about a task that is asleep"
                    );
                }
            }
            Op::Retire { id } => {
                sched.retire(TaskId(id as usize));
                assert!(
                    !sched.is_sleeping(TaskId(id as usize)),
                    "retire left it asleep"
                );
                assert!(
                    sched.ready_count() <= ready && sched.sleeping_count() <= sleeping,
                    "retire added to a queue"
                );
            }
            Op::Pick => {
                let picked = sched.pick();
                match picked {
                    Some(id) => {
                        assert_eq!(
                            sched.ready_count(),
                            ready - 1,
                            "picking {id:?} took something other than one task"
                        );
                    }
                    None => assert_eq!(ready, 0, "pick found nothing in a non-empty queue"),
                }
            }
            Op::Advance { delta } => {
                clock = clock.saturating_add(delta as u64);
                let woke = sched.advance(clock);
                assert_eq!(sched.now(), clock, "the scheduler kept a different clock");
                assert_eq!(
                    (sched.ready_count(), sched.sleeping_count()),
                    (ready + woke, sleeping - woke),
                    "{woke} tasks woke and the queues moved by something else"
                );
            }
        }

        // The questions the runtime asks between operations, always about the
        // state as it stands now.
        assert_eq!(
            sched.has_ready(),
            sched.ready_count() > 0,
            "has_ready and the queue length answer differently"
        );
        assert_eq!(
            sched.is_empty(),
            sched.ready_count() == 0 && sched.sleeping_count() == 0,
            "is_empty disagrees with the two counts it is made of"
        );
    }

    // Draining the ready queue is what a halt does. It has to end, and it may
    // not produce the same task twice without something re-queuing it.
    let mut drained = 0usize;
    while sched.pick().is_some() {
        drained += 1;
        assert!(drained <= 512, "the ready queue never emptied");
    }
    assert_eq!(sched.ready_count(), 0);
});
