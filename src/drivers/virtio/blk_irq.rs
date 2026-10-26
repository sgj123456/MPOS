//! The block completion mailbox: what a device interrupt answers, and the
//! door that keeps one transfer in flight per device.
//!
//! Both virtio-blk drivers — the PCI one the first machine boots from and the
//! MMIO one the second machine roots from — submit their requests
//! non-blocking and **park their task** until the device's completion
//! interrupt answers. Parking is what the whole shape is for: the scheduler
//! runs other work for the device's own latency, where every earlier shape
//! held the CPU (masked, then unmasked) and gave it nothing. It is also what
//! the block layer above had to grow into: the ext4 driver used to hold the
//! cache's `RefCell` borrow across the device call, and the first cut that
//! parked died as `RefCell already borrowed` the moment a second task walked
//! into it (measured, 2026-10-25); the cache now claims a slot, parks holding
//! nothing, and completes it — `vfs::ext4::cache`'s own header carries that
//! shape.
//!
//! **One transfer at a time per device.** The mailbox is one flag and one
//! waiter, and nothing above asks for two in flight (§10.5 dropped
//! multi-request pipelining on that measurement), so a second task that wants
//! the device while a transfer is parked waits on [`wait_for_device`] instead
//! of submitting — a submission under a parked one would clear the flag the
//! first is sleeping on and retire against the wrong token. The wait list's
//! bound is the task table's own (a task registers once per wait), so its
//! allocation volume is not a user-steerable quantity.
//!
//! The handler ([`complete_irq`]) runs in interrupt context and does none
//! of: sleeping, allocating, switching tasks — it acknowledges (the driver's
//! own register), sets the flag, and wakes the parked submitter, whose wait
//! re-checks its condition inside `schedule`'s critical section, so a wake
//! that arrives before the sleep cannot be lost.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use core::cell::RefCell;
use critical_section::Mutex;

use crate::sched::TaskId;

/// The device answered. Set by the interrupt handler, cleared by whoever
/// submits the next request.
static COMPLETED: AtomicBool = AtomicBool::new(false);

/// Whether a completion interrupt is wired at all. A machine whose transport
/// carries no line (the first machine's PCI device before its MSI-X was
/// armed, a tree that names no line for the second's window) keeps the
/// synchronous call, which is the shape this kernel shipped with and the
/// honest one for a device that cannot interrupt.
static ARMED: AtomicBool = AtomicBool::new(false);

/// When the last completion interrupt was taken, for the abandoned-claim grace
/// period in [`holder_exited`].
static IRQ_AT_NS: AtomicU64 = AtomicU64::new(0);

/// How many completion interrupts this boot has taken. The pin
/// (`drivers::kunit`'s case) reads it across a device call: a read that moved
/// `count` and left it unmoved was completed by polling, not by the wire.
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether a transfer is in flight. Claimed by the submitter inside the
/// submission's critical section, released by the retirer after the request
/// is retired — the whole serialization of the device's queue.
static BUSY: AtomicBool = AtomicBool::new(false);

/// The parked submitter, or `usize::MAX` when none. Set with the claim, before
/// the first look at [`COMPLETED`], so the handler either finds it (and wakes
/// it) or has already set `COMPLETED` (and the submitter never sleeps).
///
/// The sentinel is **not** 0, because slot 0 is a real task - the boot context
/// the scheduler adopts (`BOOT_SLOT`), which is the task the boot's own
/// self-test suites run on. A `waiter != 0` wake gate skipped task 0's
/// completion wake, parking the boot task on a transfer whose interrupt had
/// already fired (`docs/notes/` §7 R.9).
///
/// This is the *wake* address and nothing else: the completion handler swaps it
/// out the moment it has woken the submitter. It is therefore **not** a record
/// of who holds the device - that is [`HOLDER`], and confusing the two is the
/// bug this file shipped (`docs/notes/` §7 R.9).
static WAITER: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The task holding the device's one-in-flight claim, or 0 when it is free.
///
/// Set by [`begin_request`] and cleared **only** by [`finish_request`] - never
/// by the completion handler, which knows a request finished but not that its
/// submitter has retired it. That is what makes a claim's death observable
/// without guessing: [`holder_exited`] asks whether *this* slot is still a
/// live task, instead of inferring death from a timeout.
///
/// **The bug this exists to end, measured.** With the holder read off `WAITER`,
/// the handler cleared it on the interrupt, and a claimant arriving inside the
/// next 100 ms could not tell "the holder is about to retire this" from "the
/// holder died", so it *assumed* death and took the claim. Two tasks then held
/// the same device and submitted tokens 0 and 1 back to back:
///
/// ```text
/// mpos: blk submit: me=6 token=Some(0)
/// mpos: blk submit: me=8 token=Some(1)
/// mpos: blk stall:  me=8 token=1 ring=Some(0) busy=true waiter=8
/// ```
///
/// `virtio-drivers`' `pop_used` is strictly FIFO, so token 0 - completed and
/// owed a pop by `me=6` - pinned the used ring's head, and `me=8` waited for a
/// token it could never reach while holding the mailbox itself. One boot wedged
/// nineteen udev workers behind a single ext4 fill claim.
static HOLDER: AtomicUsize = AtomicUsize::new(0);

/// Tasks waiting for the device to go free, in arrival order. Touched only
/// by task context under the critical section — the handler never reads it.
/// The list's bound is the task table's own: a task registers once per wait,
/// and the table is a fixed-size array the kernel already bounds, so this
/// list cannot outgrow it whatever a caller does.
static QUEUE: Mutex<RefCell<alloc::vec::Vec<usize>>> =
    Mutex::new(RefCell::new(alloc::vec::Vec::new()));

/// The line's gate: whether the machine's transports carry a completion
/// interrupt. Driven by each driver's bring-up after its wiring succeeds.
pub fn arm() {
    ARMED.store(true, Ordering::Release);
}

/// Whether the completion interrupt is wired.
pub fn armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// How many completion interrupts this boot has taken.
pub fn irq_count() -> u64 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// Whether the claim's **holder** has exited: a claim whose owner is gone can
/// never be retired by it, so it is abandoned rather than held (measured:
/// `blk-waiter=slot 6` with slot 6 absent from the task table - `docs/notes/` §7
/// R.9).
///
/// The question is asked of [`HOLDER`] and is a plain liveness read - no timeout,
/// no inference from an interrupt's age. The earlier version asked it of `WAITER`
/// and, finding it cleared by the handler, fell back to "an interrupt landed more
/// than 100 ms ago, so the owner is not coming back". That is a guess, and it was
/// wrong exactly where it mattered: a living owner between its interrupt and its
/// own `finish_request` was pronounced dead, a second task was let in, and the
/// used ring wedged (the measurement is [`HOLDER`]'s).
fn holder_exited() -> bool {
    let holder = HOLDER.load(Ordering::Acquire);
    holder != 0 && !crate::sched::task_alive(TaskId(holder))
}

/// Whether a transfer is in flight **with a living holder**.
pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire) && !holder_exited()
}

/// The task holding the device, or 0 when free - for the stall report and the
/// heartbeat, which need the holder and the wake address as separate facts.
pub fn holder_slot() -> usize {
    HOLDER.load(Ordering::Acquire)
}

/// Claim the device for one transfer and clear the mailbox for it, or report
/// it taken. `me` is recorded as the waiter **before** the caller's first
/// look at [`completed`]: the lost-wakeup rule's own order, and the reason a
/// completion that lands between claim and park still answers.
pub fn begin_request(me: TaskId) -> bool {
    critical_section::with(|_| {
        if BUSY.load(Ordering::Acquire) && holder_exited() {
            BUSY.store(false, Ordering::Release);
            WAITER.store(usize::MAX, Ordering::Release);
            HOLDER.store(0, Ordering::Release);
            COMPLETED.store(false, Ordering::Release);
        }
        if BUSY.swap(true, Ordering::AcqRel) {
            return false;
        }
        COMPLETED.store(false, Ordering::Release);
        WAITER.store(me.0, Ordering::Release);
        // The claim's owner, recorded with it and released only by
        // `finish_request` - so "is the holder alive?" is a liveness read of a
        // task that really holds the device, not a guess about an interrupt's
        // age (`HOLDER`'s note has the measurement).
        HOLDER.store(me.0, Ordering::Release);
        crate::sched::note_blk_mailbox(true, Some(me.0));
        true
    })
}

/// Register `me` as waiting for the device to go free. The caller re-checks
/// [`busy`] after this in its own wait condition, which is what makes a
/// release between registration and sleep visible.
///
/// `false` answers "already free — take it": the caller re-runs its claim
/// instead of waiting for a wake that would never come.
pub fn wait_for_device(me: TaskId) -> bool {
    critical_section::with(|cs| {
        if !BUSY.load(Ordering::Acquire) {
            return false;
        }
        QUEUE.borrow(cs).borrow_mut().push(me.0);
        true
    })
}

/// The retirer's half: the request is retired, the device is free. Wakes the
/// queued tasks outside the critical section, the way every wake is handed.
pub fn finish_request() {
    let queued = critical_section::with(|cs| {
        BUSY.store(false, Ordering::Release);
        crate::sched::note_blk_mailbox(false, None);
        WAITER.store(usize::MAX, Ordering::Release);
        // The claim, and only here: the completion handler must not clear this,
        // because a request that finished is not a claim that was retired and
        // the difference is what keeps a slow holder from being taken for a dead
        // one (see `HOLDER`).
        HOLDER.store(0, Ordering::Release);
        core::mem::take(&mut *QUEUE.borrow(cs).borrow_mut())
    });
    for id in queued {
        crate::sched::wake(TaskId(id));
    }
}

/// Mark the request complete and wake the parked submitter.
///
/// Interrupt context: no locks beyond the scheduler's own, no allocation, no
/// device access. The waiter it wakes recorded itself inside the same
/// critical section that cleared the flag, so by the time this runs the
/// parked task is either in the sleep queue (the wake finds it) or still
/// before its first condition check (which the flag answers).
pub fn complete_irq() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    IRQ_AT_NS.store(crate::hal::since_boot_nanos(), Ordering::Relaxed);
    IRQ_AT_NS.store(crate::hal::since_boot_nanos(), Ordering::Relaxed);
    COMPLETED.store(true, Ordering::Release);
    let waiter = WAITER.swap(usize::MAX, Ordering::AcqRel);
    // The claim is still in flight (the owner retires it), but no slot holds it
    // any more - mirrored so the heartbeat does not report a stale owner.
    crate::sched::note_blk_mailbox(true, None);
    if waiter != usize::MAX {
        crate::sched::wake(TaskId(waiter));
    }
}

/// Read the completion flag.
pub fn completed() -> bool {
    COMPLETED.load(Ordering::Acquire)
}
