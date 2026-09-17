//! Which task runs next.
//!
//! This is the half of the scheduler that has no idea what a register is: a set
//! of ready queues and a sleep queue, driven by a tick count the caller owns.
//! Everything here runs on the host under `cargo test`, which is the only reason
//! the policy is a separate module at all.
//!
//! The rule is strict priority between bands and round-robin within one. Strict
//! priority means a CRITICAL task starves a LOW one; that is deliberate for a
//! kernel whose CRITICAL band is the idle-adjacent bookkeeping and whose LOW
//! band is background work. Once M4 has been on real workloads for a while an
//! aging pass can be added here without touching any assembly.

use alloc::collections::{BinaryHeap, VecDeque};
use core::cmp::{Ordering, Reverse};

/// How many priority bands exist. A `Priority` is an index into them, so this
/// number is the whole of the policy's tunable surface.
pub const BANDS: usize = 4;

const _: () = assert!(BANDS > 0, "a scheduler with no bands cannot queue anything");

/// Identifies a task. The runtime hands these out and they are never reused
/// while a task is alive, so the policy can hold one without holding a lock on
/// the task table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub usize);

/// Which ready queue a task sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

impl Priority {
    pub const LOW: Priority = Priority(0);
    pub const NORMAL: Priority = Priority(1);
    pub const HIGH: Priority = Priority(2);
    pub const CRITICAL: Priority = Priority(3);

    /// Clamped rather than rejected: a priority outside the bands has no queue to
    /// go in, and the alternatives are a panic or silently dropping a task.
    pub const fn new(band: u8) -> Priority {
        if band as usize >= BANDS {
            Priority((BANDS - 1) as u8)
        } else {
            Priority(band)
        }
    }

    pub const fn band(self) -> usize {
        self.0 as usize
    }
}

impl Default for Priority {
    fn default() -> Priority {
        Priority::NORMAL
    }
}

/// A queued sleeper.
///
/// The ordering *is* the wake rule, so it is written out rather than derived:
/// the task that wakes first is the smallest, meaning earliest deadline, then
/// highest priority, then lowest id. The heap holds these wrapped in `Reverse`
/// because `BinaryHeap` is a max-heap. The last two rules only matter when
/// deadlines tie, and they exist to make wake order deterministic instead of a
/// function of heap internals.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sleeper {
    deadline: u64,
    priority: Priority,
    id: TaskId,
}

impl Ord for Sleeper {
    fn cmp(&self, other: &Self) -> Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| other.priority.cmp(&self.priority))
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Sleeper {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Ready queues and a sleep queue. Holds no task state, so the runtime can ask
/// it for a decision while the task table is borrowed elsewhere.
pub struct Scheduler {
    ready: [VecDeque<TaskId>; BANDS],
    sleeping: BinaryHeap<Reverse<Sleeper>>,
    /// The clock as this scheduler last saw it, updated by [`Scheduler::advance`].
    now: u64,
}

impl Scheduler {
    /// Const so the runtime can put one in a `static`. Neither queue allocates
    /// until it is used.
    pub const fn new() -> Scheduler {
        Scheduler {
            ready: [const { VecDeque::new() }; BANDS],
            sleeping: BinaryHeap::new(),
            now: 0,
        }
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn ready_count(&self) -> usize {
        self.ready.iter().map(VecDeque::len).sum()
    }

    pub fn sleeping_count(&self) -> usize {
        self.sleeping.len()
    }

    /// True when some task could run this instant. The runtime halts when this
    /// is false and waits for the next tick to make it true again.
    pub fn has_ready(&self) -> bool {
        self.ready_count() > 0
    }

    /// True when there is nothing to run now and nothing the clock will bring
    /// back later. Distinct from [`has_ready`](Self::has_ready): a task asleep
    /// until tick 1000 leaves the scheduler empty of ready work but not empty.
    pub fn is_empty(&self) -> bool {
        self.ready_count() == 0 && self.sleeping.is_empty()
    }

    /// The task to run next, removed from its queue.
    ///
    /// Popping from the front and [`push_ready`](Self::push_ready) appending to
    /// the back is all the round-robin there is: a task that yields goes to the
    /// end of its own band and does not run again until everything ahead of it
    /// has had a turn.
    pub fn pick(&mut self) -> Option<TaskId> {
        (0..BANDS)
            .rev()
            .find_map(|band| self.ready[band].pop_front())
    }

    /// Put a task at the back of its band's queue.
    pub fn push_ready(&mut self, id: TaskId, priority: Priority) {
        self.ready[priority.band()].push_back(id);
    }

    /// Take a task out of every queue - ready and sleeping. It will never run
    /// again: this is how a group exit disposes of its siblings, whose slots
    /// the release pass frees while their contexts sit unreached. A task left
    /// queued after its slot went away is a pid the picker would dereference
    /// into nothing.
    pub fn retire(&mut self, id: TaskId) {
        for band in self.ready.iter_mut() {
            band.retain(|queued| *queued != id);
        }
        let mut rebuilt = BinaryHeap::new();
        while let Some(Reverse(sleeper)) = self.sleeping.pop() {
            if sleeper.id != id {
                rebuilt.push(Reverse(sleeper));
            }
        }
        self.sleeping = rebuilt;
    }

    /// Queue a task to wake after `ticks` more clock advances.
    ///
    /// `ticks` of 0 is not "wake immediately": the task still has to wait for the
    /// next [`advance`](Self::advance), which is what makes a sleep a scheduling
    /// point rather than a no-op.
    pub fn sleep(&mut self, id: TaskId, priority: Priority, ticks: u64) {
        self.sleeping.push(Reverse(Sleeper {
            // Saturating so u64::MAX is "until someone wakes me": a plain add
            // would wrap the deadline into the past.
            deadline: self.now.saturating_add(ticks),
            priority,
            id,
        }));
    }

    /// Pull a task out of the sleep queue and make it ready now.
    ///
    /// Returns whether it was asleep. Every other answer is a no-op by design:
    /// a running or ready task will look at its condition again on its own, and
    /// a task that has exited is gone. This is the "you have a message" half of
    /// blocking I/O - the condition's owner calls it after making the condition
    /// true, and a lost wake would mean a lost update in the caller's own state.
    pub fn wake(&mut self, id: TaskId) -> bool {
        // BinaryHeap cannot remove an arbitrary element, so rebuild around it.
        // The sleep queue holds at most one entry per task and stays small.
        let mut woken = None;
        let mut rebuilt = BinaryHeap::new();
        while let Some(Reverse(sleeper)) = self.sleeping.pop() {
            if woken.is_none() && sleeper.id == id {
                woken = Some(sleeper);
            } else {
                rebuilt.push(Reverse(sleeper));
            }
        }
        self.sleeping = rebuilt;
        match woken {
            Some(sleeper) => {
                self.ready[sleeper.priority.band()].push_back(id);
                true
            }
            None => false,
        }
    }

    /// Move the clock to `now` and re-queue everything whose deadline has passed.
    /// Returns how many tasks woke, so the runtime can tell whether the tick
    /// changed the set of runnable tasks at all.
    pub fn advance(&mut self, now: u64) -> usize {
        debug_assert!(
            now >= self.now,
            "the clock went backwards from {} to {now}",
            self.now
        );
        self.now = now;

        let mut woken = 0;
        while self
            .sleeping
            .peek()
            .is_some_and(|Reverse(next)| next.deadline <= now)
        {
            let Reverse(sleeper) = self.sleeping.pop().expect("peeked");
            self.ready[sleeper.priority.band()].push_back(sleeper.id);
            woken += 1;
        }
        woken
    }
}

impl Default for Scheduler {
    fn default() -> Scheduler {
        Scheduler::new()
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn task(n: usize) -> TaskId {
        TaskId(n)
    }

    #[test]
    fn round_robin_cycles_within_a_band() {
        let (a, b, c) = (task(0), task(1), task(2));
        let mut sched = Scheduler::new();
        for id in [a, b, c] {
            sched.push_ready(id, Priority::NORMAL);
        }

        // `pick` only pops; re-queueing a task that gave up the CPU is the
        // runtime's job, so do it here to model a task yielding seven times.
        let mut order = Vec::new();
        for _ in 0..7 {
            let id = sched.pick().expect("queue ran dry");
            sched.push_ready(id, Priority::NORMAL);
            order.push(id);
        }
        assert_eq!(order, vec![a, b, c, a, b, c, a]);
    }

    #[test]
    fn a_higher_band_runs_before_a_lower_one() {
        let (low, critical) = (task(0), task(1));
        let mut sched = Scheduler::new();

        // Queued last but runs first: the bands are drained top down, so arrival
        // order across bands means nothing.
        sched.push_ready(low, Priority::LOW);
        sched.push_ready(critical, Priority::CRITICAL);
        assert_eq!(sched.pick(), Some(critical));

        // And it keeps running first, which is the starvation this policy accepts.
        for _ in 0..3 {
            sched.push_ready(critical, Priority::CRITICAL);
            assert_eq!(sched.pick(), Some(critical));
        }
        assert_eq!(sched.pick(), Some(low));
    }

    #[test]
    fn a_task_wakes_on_the_tick_it_asked_for() {
        let id = task(7);
        let mut sched = Scheduler::new();
        sched.advance(10);
        sched.sleep(id, Priority::NORMAL, 5);

        sched.advance(14);
        assert!(!sched.has_ready(), "woke a tick early");
        sched.advance(15);
        assert_eq!(sched.pick(), Some(id));
    }

    #[test]
    fn wake_order_follows_deadlines() {
        let (early, middle, late) = (task(0), task(1), task(2));
        let mut sched = Scheduler::new();

        // Queued out of order on purpose: the heap, not insertion, decides.
        sched.sleep(late, Priority::NORMAL, 30);
        sched.sleep(early, Priority::NORMAL, 10);
        sched.sleep(middle, Priority::NORMAL, 20);

        assert_eq!(sched.advance(100), 3);
        let order: Vec<TaskId> = (0..3)
            .map(|_| sched.pick().expect("queue ran dry"))
            .collect();
        assert_eq!(order, vec![early, middle, late]);
    }

    #[test]
    fn a_tie_breaks_on_priority_then_id() {
        let mut sched = Scheduler::new();
        // All three wake on the same tick, so the order they are re-queued in is
        // entirely the tie-break rule.
        sched.sleep(task(5), Priority::LOW, 1);
        sched.sleep(task(3), Priority::LOW, 1);
        sched.sleep(task(9), Priority::HIGH, 1);
        sched.advance(1);

        assert_eq!(sched.pick(), Some(task(9)), "the HIGH task should lead");
        assert_eq!(sched.pick(), Some(task(3)), "then the lower id");
        assert_eq!(sched.pick(), Some(task(5)));
    }

    #[test]
    fn sleeping_zero_ticks_still_waits_for_the_clock() {
        let id = task(1);
        let mut sched = Scheduler::new();
        sched.advance(4);
        sched.sleep(id, Priority::NORMAL, 0);

        assert_eq!(sched.pick(), None, "a zero-tick sleep ran without yielding");
        assert_eq!(sched.advance(5), 1, "the next tick brings it back");
        assert_eq!(sched.pick(), Some(id));
    }

    #[test]
    fn ready_and_empty_answer_different_questions() {
        let mut sched = Scheduler::new();
        assert!(!sched.has_ready());
        assert!(sched.is_empty());

        sched.push_ready(task(0), Priority::NORMAL);
        assert!(sched.has_ready());
        assert!(!sched.is_empty());
        sched.pick();
        assert!(!sched.has_ready(), "a popped task is no longer queued");
        assert!(sched.is_empty());

        sched.sleep(task(1), Priority::NORMAL, 5);
        assert!(!sched.has_ready());
        assert!(
            !sched.is_empty(),
            "sleepers are work the clock will bring back"
        );
        sched.advance(5);
        assert!(sched.has_ready());
        sched.pick();
        assert!(sched.is_empty());
    }

    #[test]
    fn waking_a_sleeper_pulls_it_out_of_the_sleep_queue() {
        let (a, b) = (task(0), task(1));
        let mut sched = Scheduler::new();
        sched.push_ready(b, Priority::NORMAL);
        sched.sleep(a, Priority::NORMAL, 20);

        sched.advance(3);
        assert!(sched.wake(a), "the sleeper was there to wake");
        // Wake appends, so `b` keeps its place ahead of the newly ready task.
        assert_eq!(sched.pick(), Some(b));
        assert_eq!(sched.pick(), Some(a));
        // The deadline entry is gone, so the clock brings nothing back.
        assert_eq!(sched.advance(100), 0);
        // And a ready task has nothing to wake.
        assert!(!sched.wake(a));
    }

    #[test]
    fn a_woken_sleeper_rejoins_its_own_band() {
        let mut sched = Scheduler::new();
        sched.push_ready(task(0), Priority::NORMAL);
        sched.sleep(task(1), Priority::LOW, 10);

        assert!(sched.wake(task(1)));
        assert_eq!(sched.pick(), Some(task(0)), "LOW does not jump NORMAL");
        assert_eq!(sched.pick(), Some(task(1)));
    }

    #[test]
    fn waking_a_task_that_is_not_sleeping_reports_failure() {
        let mut sched = Scheduler::new();
        let a = task(0);
        // Running (picked), ready, and never-added are all already accounted for.
        sched.push_ready(a, Priority::NORMAL);
        assert!(!sched.wake(a));
        assert!(!sched.wake(a));
        let picked = sched.pick();
        assert_eq!(picked, Some(a));
        assert!(!sched.wake(a));
    }

    #[test]
    fn sleep_saturates_so_u64_max_means_until_woken() {
        let id = task(0);
        let mut sched = Scheduler::new();
        sched.advance(u64::MAX / 2);
        sched.sleep(id, Priority::NORMAL, u64::MAX);
        // The deadline saturated to u64::MAX, so every earlier tick leaves it
        // asleep and a plain add would have wrapped it into the past.
        sched.advance(u64::MAX - 1);
        assert_eq!(sched.pick(), None);
        assert!(sched.wake(id));
        assert_eq!(sched.pick(), Some(id));
    }
}
