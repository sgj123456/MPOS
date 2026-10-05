//! The console's transmit ring: what a machine's UART drains behind itself.
//!
//! Both machines' console sinks have an interrupting transmitter, and the trick
//! is the same on each: the bytes go into a queue, the transmitter is asked for
//! its own "I have room" interrupt, and the handler moves them while whoever
//! wrote goes back to being a task the scheduler can preempt. The alternative -
//! the writer polling the transmitter's status bit with interrupts masked - was
//! measured on the first machine: a few kilobytes of output inside one critical
//! section blanked the clock long enough to starve the receiver, so what read as
//! a slow console was a machine that had stopped scheduling. The reason it is
//! *here*, above either machine, is that the alternative to sharing it is two
//! copies of that reasoning with one line of registers between them, and the
//! second copy is where this kind of bug survives a port (`PLAN.md` §13).
//!
//! [`Transmitter`] is the whole of what differs. Everything a caller above the
//! seam asks - how much room there is, who is waiting for some, whether the
//! writer may park, whether the queue may be used at all - is decided once
//! below, and answered through `hal::SerialRing`. A machine that has no
//! interrupting transmitter is not this; it is simply not a `SerialRing`, and
//! `crate::console` takes the polled path instead.
//!
//! # Why the critical section covers only the copy
//!
//! Every want here is either a copy into the queue or a copy out of it, and the
//! section ends before anybody waits for hardware. That is the point of the
//! device rather than a detail of its locking: the section is a *masked
//! interrupt* window, and the width of that window is what the ring exists to
//! bound. [`pump`] holds one across a burst of writes to the transmitter, which
//! is bounded by [`BURST`] rather than by how much the host is willing to drain
//! - so a terminal that stops reading costs this machine one burst, not the
//! clock.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;

use crate::sched::policy::TaskId;

/// What the ring needs of the hardware, and all it needs.
///
/// Three answers, and none of them a *decision*: the machine says whether its
/// transmitter will take a byte, takes one, and arms or disarms the interrupt
/// that says it will take another. Whether to arm it, and what happens while a
/// writer waits for room, are this module's (`PLAN.md` §13.1: a port implements
/// a capability, it does not re-derive one).
pub trait Transmitter {
    /// Whether the transmitter will take another byte right now.
    fn tx_ready() -> bool;
    /// Write one byte, after [`tx_ready`](Self::tx_ready) has said it is empty.
    /// It may not wait for the hardware.
    fn tx_put(byte: u8);
    /// Ask for - or stop asking for - the transmitter's own "I have room"
    /// interrupt.
    ///
    /// Called only when there is something to send (on every accepted
    /// submission) and again as soon as the queue empties, so an idle
    /// transmitter does not interrupt for ever. On a machine whose transmitter
    /// and receiver share one line this is one mask and one enable bit of the
    /// same register, which is exactly why the receiver's side of that register
    /// is the machine's business and not this module's.
    fn tx_interrupt(on: bool);
}

/// How many bytes the ring holds.
///
/// Sized for a screenful of the heaviest thing a console carries - a
/// colour-sequence-per-character rendering of a source file - so a printing task
/// parks on the ring rather than on the wire. Measured on the first machine:
/// below roughly a screenful, a writer offering one line at a time still had to
/// wait for the UART between them, and the point of the queue was gone.
const CAPACITY: usize = 16 * 1024;

/// How many bytes one visit from the transmitter moves before leaving the rest
/// to the next one.
///
/// The hardware says when it has had enough by clearing its own status bit, so
/// this only caps a handler against a transmitter that keeps claiming to be
/// ready - the difference between one bounded burst and the machine's time.
const BURST: usize = 256;

struct Ring {
    /// The bytes waiting to go out, oldest first.
    bytes: VecDeque<u8>,
    /// The tasks sleeping until there is room in them again.
    waiters: Vec<TaskId>,
    /// Bytes this ring has handed to a transmitter, since boot.
    moved: usize,
    /// Visits from the transmitter that moved at least one byte. A line shared
    /// with the receiver counts here too, which is why this says *visits that
    /// moved something* rather than "transmit interrupts".
    visits: u64,
    /// Bytes accepted from writers, since boot.
    queued: usize,
}

impl Ring {
    /// The empty queue, in `const` form so it can be the static's initializer.
    const fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            waiters: Vec::new(),
            moved: 0,
            visits: 0,
            queued: 0,
        }
    }
}

static RING: Mutex<RefCell<Ring>> = Mutex::new(RefCell::new(Ring::new()));

/// Whether the transmitter's interrupt may be used at all.
///
/// Cleared until [`enable`], which is what the boot calls once the interrupt
/// controller can actually deliver one: before that the only honest way to put a
/// byte on the wire is the machine's own polled write, and a queue nothing
/// drains is bytes that quietly never arrive.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Let the console queue its bytes rather than wait for the wire.
///
/// Called once, from the wiring layer where the receiver is armed too - after
/// the vector table, the controller and the heap all exist. Until it runs,
/// [`crate::console`] takes the polled path.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether a writer may queue rather than poll - [`Transmitter`]'s question, so
/// that neither machine keeps its own copy of this answer.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// How many bytes the queue holds.
pub fn capacity() -> usize {
    CAPACITY
}

/// How many of them are free right now.
pub fn space() -> usize {
    critical_section::with(|cs| CAPACITY - RING.borrow(cs).borrow().bytes.len())
}

/// Take as much of `bytes` as there is room for and ask for the interrupt that
/// sends it, answering how many were taken - the rest is the caller's to offer
/// again once there is room.
///
/// The critical section covers the copy into the queue and nothing else. That
/// is the whole of what separates this from writing each byte with interrupts
/// masked: no amount of host slowness can widen the window, only the length of
/// one `extend`.
pub fn submit<T: Transmitter>(bytes: &[u8]) -> usize {
    let n = critical_section::with(|cs| {
        let mut ring = RING.borrow(cs).borrow_mut();
        let n = (CAPACITY - ring.bytes.len()).min(bytes.len());
        ring.bytes.extend(bytes[..n].iter().copied());
        ring.queued += n;
        n
    });
    if n > 0 {
        T::tx_interrupt(true);
    }
    n
}

/// The transmitter's half: hand the queue to the hardware, and wake whoever was
/// waiting for room.
///
/// Bounded by [`BURST`], so a stream the host will not drain costs this visit
/// one burst rather than the rest of the machine's time; what did not fit stays
/// queued for the next one. Emptying the queue *disarms* the transmitter's
/// interrupt, which is what stops an idle line interrupting for ever - and stops
/// it here rather than in each gate's return path, so a machine whose
/// transmitter shares a line with its receiver cannot be left asserting one.
pub fn pump<T: Transmitter>() {
    let waiters = critical_section::with(|cs| {
        let mut ring = RING.borrow(cs).borrow_mut();
        let mut moved = 0usize;
        while moved < BURST && T::tx_ready() {
            let Some(byte) = ring.bytes.pop_front() else {
                break;
            };
            T::tx_put(byte);
            moved += 1;
        }
        if ring.bytes.is_empty() {
            // Nothing left to send: ask no more.
            T::tx_interrupt(false);
        }
        if moved == 0 {
            return None;
        }
        ring.moved += moved;
        ring.visits += 1;
        Some(core::mem::take(&mut ring.waiters))
    });
    // Outside the section: `wake` may move a task onto the run queue, and there
    // is no reason to hold interrupts masked across somebody else's schedule.
    if let Some(waiters) = waiters {
        for id in waiters {
            crate::sched::wake(id);
        }
    }
}

/// Put the running task on the room-in-the-queue wait list, before it parks.
pub fn wait_now() {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut waiters = RING.borrow(cs).borrow_mut();
        if !waiters.waiters.contains(&me) {
            waiters.waiters.push(me);
        }
    });
}

/// Take the running task back off it - however the wait ended.
pub fn stop_waiting() {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        RING.borrow(cs).borrow_mut().waiters.retain(|w| *w != me);
    });
}

/// What the queue has moved since boot, for the bring-up check that proves the
/// ring - and not the polled write - is carrying the console.
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    /// Bytes accepted from writers.
    pub queued: usize,
    /// Bytes handed to a transmitter by an interrupt.
    pub moved: usize,
    /// Visits that moved something.
    pub visits: u64,
}

/// See [`Stats`].
pub fn stats() -> Stats {
    critical_section::with(|cs| {
        let ring = RING.borrow(cs).borrow();
        Stats {
            queued: ring.queued,
            moved: ring.moved,
            visits: ring.visits,
        }
    })
}
