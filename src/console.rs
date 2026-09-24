//! Kernel console — architecture-agnostic formatting over `arch::serial`.
//!
//! This module is a **kernel-wide service**, not a driver or a VFS component.
//! It wraps the raw serial driver with interrupt-safe output and provides the
//! `println!` macro. Callers include the kernel itself (`println!`), the TTY
//! line discipline (echo output), and the syscall `write` handler.

use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::{self, Write};

use critical_section::Mutex;

pub struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::drivers::serial::write_bytes(s.as_bytes());
        Ok(())
    }
}

/// The line being formatted, gathered before any of it reaches the wire.
///
/// Only used once the heap exists and the console queues into the transmit
/// ring - see [`print_args`]. Gathering is what lets a line be handed over as
/// one submission instead of being streamed under a held critical section.
struct Sink(Vec<u8>);

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

pub fn init() {
    crate::drivers::serial::init();
}

/// The kernel log ring: what `/dev/kmsg` writers file. The serial console
/// carries the kernel's own voice; this carries userland's filed notes,
/// which Linux keeps *off* the console - an init that echoes its banner
/// into `/dev/kmsg` and prints it to stdout would otherwise be on the
/// screen twice, and every boot after this one has that init.
static DMESG: Mutex<RefCell<alloc::collections::VecDeque<u8>>> =
    Mutex::new(RefCell::new(alloc::collections::VecDeque::new()));
const DMESG_CAP: usize = 16 * 1024;

/// File bytes into the kernel log ring, oldest entries dropped first.
pub fn log_bytes(bytes: &[u8]) {
    critical_section::with(|cs| {
        let mut log = DMESG.borrow(cs).borrow_mut();
        for &b in bytes {
            if log.len() == DMESG_CAP {
                log.pop_front();
            }
            log.push_back(b);
        }
    });
}

/// Snapshot the kernel log ring, as a `dmesg` reader would see it.
pub fn dmesg_snapshot() -> alloc::vec::Vec<u8> {
    critical_section::with(|cs| DMESG.borrow(cs).borrow().iter().copied().collect())
}

/// Take up to `len` bytes off the front of the kernel log ring - the
/// destructive read `syslog(2)`'s `SYSLOG_ACTION_READ` serves, and the one
/// a `klogd` parks in. What is taken is gone: reading the log is news, and
/// news is news once.
pub fn dmesg_take(len: usize) -> alloc::vec::Vec<u8> {
    critical_section::with(|cs| {
        let mut log = DMESG.borrow(cs).borrow_mut();
        let n = log.len().min(len);
        (0..n)
            .map(|_| log.pop_front().expect("length checked"))
            .collect()
    })
}

/// Empty the kernel log ring - `syslog(2)`'s `SYSLOG_ACTION_CLEAR`, and the
/// tail of its read-and-clear. The bytes are dropped rather than archived:
/// a `dmesg` after a clear answers as nothing having been said.
pub fn dmesg_clear() {
    critical_section::with(|cs| DMESG.borrow(cs).borrow_mut().clear());
}

/// How many bytes the kernel log ring holds - `syslog(2)`'s
/// `SYSLOG_ACTION_SIZE_BUFFER` answer, and the number a `dmesg` sizes its
/// own buffer by before it asks for the log.
pub fn dmesg_capacity() -> usize {
    DMESG_CAP
}

/// Write raw bytes on the console, parking on the transmit ring when it fills.
///
/// Once the transmitter interrupts - [`crate::drivers::serial::enable_tx`],
/// where the IDT and the PIC exist - the bytes are queued and a full ring
/// *waits*: the task sleeps until the hardware takes enough for the rest.
///
/// Before that it is the polled write, in chunks with the clock live between
/// them. The 16550 spins on the transmitter-empty bit per byte, so a few
/// kilobytes of output written inside one critical section blanked interrupts
/// long enough to starve the receiver and lose ticks - which is not merely
/// slow, it is the machine looking dead: no clock means no scheduling, and no
/// serial interrupt means the keyboard is not even read. The bug that shape
/// took was a `bat` over a minified stylesheet, whose colour sequences reach
/// the console in tens of thousands of writes.
///
/// The polled form is still what a caller that cannot park gets: an interrupt
/// handler, or the panic path on the way out. It is bounded by the chunk, not
/// by the wait, so a host that stops draining still costs only one line's
/// worth of masked interrupts rather than the machine.
pub fn write_bytes(bytes: &[u8]) {
    if !crate::drivers::serial::interrupt_driven() || !may_park() {
        const CHUNK: usize = 64;
        for piece in bytes.chunks(CHUNK) {
            critical_section::with(|_| crate::drivers::serial::write_bytes(piece));
        }
        return;
    }
    let mut at = 0;
    while at < bytes.len() {
        at += crate::drivers::serial::tx_submit(&bytes[at..]);
        if at == bytes.len() {
            break;
        }
        // Re-armed before *every* park, not once above. `pump_tx` hands the
        // room out by taking the WHOLE wait list and waking it, so a wake also
        // removes this task from the list. A write larger than one drain burst
        // - a `bat` flood over the ring - therefore parks again, and unless it
        // rejoined, that second sleep would be announced to nobody and the
        // console would hang mid-write. Being on the list before the check
        // still closes the first gap: room that appears between the failed
        // submit and the sleep is seen, because `block_until` re-checks under
        // the same critical section that would take us off it.
        crate::drivers::serial::tx_wait_now();
        crate::sched::block_until(|| crate::drivers::serial::tx_space() > 0);
    }
    crate::drivers::serial::tx_stop_waiting();
}

/// Whether this context may sleep: the scheduler is running, and interrupts
/// are enabled to let the wake through. An interrupt handler fails the second
/// test, and the boot code before `sched::init` fails the first.
fn may_park() -> bool {
    crate::sched::started() && crate::arch::interrupts_enabled()
}

#[doc(hidden)]
pub fn print_args(args: fmt::Arguments<'_>) {
    // A whole line at a time. Tasks preempt each other anywhere, including
    // halfway through a format argument, and two of them writing to one UART
    // produce output neither meant.
    //
    // Before the console queues into the ring - which is armed at the end of
    // the handoff, after the heap - there is nothing to park on and, more to
    // the point, nothing to allocate *from*: the first lines this prints are
    // the memory map and the frame allocator's own tally, and a `Vec` here
    // would ask for a heap that does not exist yet. So that half of the boot
    // keeps the old shape, streaming the line under the critical section.
    if !crate::drivers::serial::interrupt_driven() {
        critical_section::with(|_| {
            let _ = Console.write_fmt(args);
        });
        return;
    }
    // Gathered, then handed over once: the line stays contiguous without the
    // section spanning a wait for the hardware, which is what used to blank
    // the clock for as long as the host took to drain the line.
    let mut sink = Sink(Vec::new());
    let _ = sink.write_fmt(args);
    write_bytes(&sink.0);
}
