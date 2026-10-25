//! The block completion mailbox: what a device interrupt answers.
//!
//! Both virtio-blk drivers — the PCI one the first machine boots from and the
//! MMIO one the second machine roots from — submit their requests
//! non-blocking and wait on this mailbox. What the wait must **not** do is
//! yield: the block layer's callers (the ext4 driver above all) hold
//! non-reentrant borrows across the device call, and a task that parked
//! mid-operation would let a second task walk into the same borrow —
//! measured as `RefCell already borrowed` in `vfs::ext4` the first time the
//! wait parked. So the wait is an **unmasked** poll: the completion
//! interrupt answers it (the handler acks the device and marks this
//! mailbox), the timer and every other line keep arriving while it spins,
//! and no task switch happens — the re-entrancy the block layer was built
//! on stays exactly as it was, and the *mask* §10.5 measured (the whole
//! device latency held with interrupts off) is still gone: what remains is
//! submit and retire, two bounded critical sections.
//!
//! The handler ([`complete_irq`]) runs in interrupt context and does none
//! of: sleeping, allocating, switching tasks — it acknowledges, sets the
//! flag, and leaves the interrupted core right where it was.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The device answered. Set by the interrupt handler, cleared by whoever
/// submits the next request.
static COMPLETED: AtomicBool = AtomicBool::new(false);

/// Whether a completion interrupt is wired at all. A machine whose transport
/// carries no line (the first machine's PCI device before its MSI-X was
/// armed, a tree that names no line for the second's window) keeps the
/// synchronous call, which is the shape this kernel shipped with and the
/// honest one for a device that cannot interrupt.
static ARMED: AtomicBool = AtomicBool::new(false);

/// How many completion interrupts this boot has taken. The pin
/// (`drivers::kunit`'s case) reads it across a device call: a read that
/// moved `count` and left it unmoved was completed by polling, not by the
/// wire.
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

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

/// Clear the mailbox for a new submission: the flag is the *previous*
/// request's until this runs, and every submitter runs under the device lock,
/// so one clear per submit is the whole of the arbitration.
pub fn begin_request() {
    COMPLETED.store(false, Ordering::Release);
}

/// Mark the request complete and wake the parked submitter.
///
/// Interrupt context: no locks beyond the scheduler's own, no allocation, no
/// device access. The waiter it wakes registered itself inside the same
/// critical section that checked the flag, so by the time this runs the
/// parked task is either in the sleep queue (the wake finds it) or already
/// past its check (the flag says so) — the third state the lost-wakeup rule
/// forbids cannot happen.
pub fn complete_irq() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    COMPLETED.store(true, Ordering::Release);
}

/// Read the completion flag.
pub fn completed() -> bool {
    COMPLETED.load(Ordering::Acquire)
}
