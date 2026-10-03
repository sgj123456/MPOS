//! The machine's console UART: COM1, `ttyS0`.
//!
//! COM1 is the one wire to the world - the kernel console, where every
//! `tracing` event and `println!` goes, and the wire the console multiplexer
//! carries the virtual consoles' traffic over. There is no second serial
//! terminal: the shells run on the pty-backed `/dev/tty1`..`tty6`, and
//! `vcmux` moves their bytes over this port.
//!
//! Output is a plain write. Input is an interrupt: the receiver's data-ready
//! line is IRQ4, and [`drain_into_tty`] is how the handler empties the FIFO
//! and hands what it finds to the multiplexer.
//!
//! # Why this module does not lock
//!
//! Everywhere else in this kernel, shared state is a `critical_section::Mutex`
//! around a `RefCell`. Here it cannot be. The console writes through
//! [`write_bytes`] with the kernel's one critical section held for the length
//! of a line, and an interrupt that arrives in the middle of that line is
//! taken *inside* the section. A mutex the handler then tried to take would
//! find itself already held by the code it interrupted - a `RefCell` borrow
//! that panics, or worse, a spin that never ends.
//!
//! So the receiver keeps no lock. The UART is the state: its registers are
//! read straight from the port, and the only thing the handler touches on the
//! way out is the tty, which is written through `critical_section` and is
//! safe to enter from anywhere because interrupts are already off by then.
//!
//! The transmit ring below does take a lock, and it is safe for the same
//! reason read the other way: the borrow covers only the copy into the ring or
//! out of it, never the wait for the hardware. That distinction is the whole
//! point of the ring - see [`pump_tx`] and [`tx_submit`].
//!
//! The interrupt stays masked until [`enable_rx`] asks for it, so nothing
//! arrives while the kernel is still finding its feet.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex;
use uart_16550::spec::registers::{FifoTriggerLevel, LSR};
use uart_16550::{Config, Uart16550};
use x86_64::instructions::port::Port;

use crate::sched::policy::TaskId;

/// IER bit 0: raise an interrupt whenever a byte is ready to be read.
const IER_DATA_READY: u8 = 1 << 0;
/// IER bit 1: raise an interrupt whenever the transmitter is ready for one
/// more byte. Enabled only while the ring holds something, so an idle console
/// does not interrupt forever.
const IER_TX_READY: u8 = 1 << 1;

/// LSR bit 5: the transmitter holding register is empty.
const LSR_TX_READY: u8 = 1 << 5;

/// How many bytes one interrupt will move before giving the rest back to the
/// next one. The FIFO holds 16, so this is never reached with working
/// hardware; it exists so the handler cannot spin forever if it does.
const MAX_DRAIN: usize = 64;
/// The same bound on the way out. The hardware says when it has had enough by
/// clearing its own status bit, so this only caps the handler against a
/// transmitter that keeps claiming to be ready.
const TX_BURST: usize = 256;

/// How many bytes the transmit ring holds. Sized for a screenful of the
/// heaviest thing the console carries - a `bat`-painted line of colour
/// sequences - so a printing task parks on the ring rather than on the wire.
const TX_RING: usize = 16 * 1024;

/// The bytes waiting to go out, and who is waiting for room in them.
static TX: Mutex<RefCell<VecDeque<u8>>> = Mutex::new(RefCell::new(VecDeque::new()));
static TX_WAITERS: Mutex<RefCell<Vec<TaskId>>> = Mutex::new(RefCell::new(Vec::new()));
/// Whether the ring is what the console writes through. Cleared until
/// [`enable_tx`], because before the IDT and the PIC exist the only way to
/// put a byte on the wire is to wait for the hardware right here.
static TX_IRQ: AtomicBool = AtomicBool::new(false);

/// One UART: the base I/O port, 0x3f8 for COM1.
#[derive(Clone, Copy)]
struct Uart {
    base: u16,
}

const COM1: Uart = Uart { base: 0x3f8 };

impl Uart {
    /// A freshly built wrapper, re-created per call. The type is a thin wrapper
    /// over the port and the config, and the port is the real state, so this
    /// costs nothing worth keeping a shared instance for.
    fn wrapper(&self) -> Uart16550<uart_16550::backend::PioBackend> {
        // SAFETY: the kernel owns the port, and it stays valid for the life
        // of the kernel.
        unsafe { Uart16550::new_port(self.base) }.expect("UART base port invalid")
    }

    /// The interrupt enable register. Writing here is the one thing the wrapper
    /// crate has no method for, and its `Config` can only set it at init time -
    /// which is before the IDT exists and therefore too early.
    fn ier(&self) -> Port<u8> {
        Port::new(self.base + 1)
    }

    /// The receive buffer, at offset 0. Reading it takes one byte out of the
    /// FIFO.
    fn data(&self) -> Port<u8> {
        Port::new(self.base)
    }

    /// The line status register, at offset 5.
    fn lsr(&self) -> Port<u8> {
        Port::new(self.base + 5)
    }

    fn init(&self) {
        self.wrapper()
            .init(console_config())
            .expect("the UART rejected its line config");
    }

    /// Put bytes on the wire by waiting for the hardware to take each one.
    ///
    /// The polled path: correct everywhere, cheap only when the host keeps up.
    /// It is what the console uses before [`enable_tx`] and what it falls back
    /// to when the caller cannot park - from an interrupt handler, or during
    /// the panic path.
    fn write(&self, bytes: &[u8]) {
        // A fresh wrapper over the same port, with no init: the UART's registers
        // are the state, and they are already set up.
        self.wrapper().send_bytes_exact(bytes);
    }

    /// Ask the UART to interrupt on every byte that arrives, and unmask its
    /// line.
    ///
    /// Deliberately not part of [`init`](Self::init): the handler feeds the
    /// terminal, and until the scripted boot checks are done there is nothing
    /// that should be typing.
    fn enable_rx(&self) {
        self.ier_set(IER_DATA_READY, true);
        crate::hal::unmask(crate::hal::Line::Serial);
    }

    /// Set or clear one bit of the interrupt enable register.
    fn ier_set(&self, bit: u8, on: bool) {
        let mut ier = self.ier();
        // SAFETY: the register is a byte. Read first so a bit another path set
        // is not cleared.
        let current = unsafe { ier.read() };
        let next = if on { current | bit } else { current & !bit };
        unsafe { ier.write(next) };
    }

    /// Whether the transmitter will take another byte right now.
    fn tx_ready(&self) -> bool {
        let mut lsr = self.lsr();
        // SAFETY: the register belongs to this port. Unlike the receiver's
        // data register, reading it has no side effect to reason about.
        let status = unsafe { lsr.read() };
        status & LSR_TX_READY != 0
    }

    /// Put one byte in the transmitter holding register. Only ever called
    /// when [`tx_ready`](Self::tx_ready) said it was empty, so it cannot wait.
    fn tx_put(&self, byte: u8) {
        let mut data = self.data();
        // SAFETY: the register is a byte of the port this module owns.
        unsafe { data.write(byte) };
    }

    /// Every byte the receiver is holding goes to the console's input path -
    /// the mux's when it is up, the console tty's before that.
    ///
    /// Called from the interrupt handler, inside a critical section the console
    /// may already be holding: nothing here locks, and nothing here prints.
    fn drain(&self) {
        let mut data = self.data();
        let mut lsr = self.lsr();
        // A bounded loop: a receiver stuck asserting "data ready" - broken
        // hardware, or a byte stream with no end - must not hang the handler.
        for _ in 0..MAX_DRAIN {
            // SAFETY: both registers belong to this port, and both reads have a
            // side effect the hardware defines: LSR clears the error flags, DATA
            // takes one byte out of the FIFO.
            let status = unsafe { lsr.read() };
            if status & LSR::DATA_READY.bits() == 0 {
                break;
            }
            let byte = unsafe { data.read() };
            crate::vfs::vcmux::route_input(byte);
        }
    }
}

/// The console's line settings: the default eight bits, no parity, one stop
/// bit, with one change that matters for a person at a keyboard.
///
/// The default FIFO trigger level is fourteen characters, which is right for a
/// stream of bulk output and wrong for input: what someone types waits in the
/// FIFO until the fourteenth character, or until a receive timeout this
/// emulated UART does not raise. One hands over every character as it lands.
fn console_config() -> Config {
    Config {
        fifo_trigger_level: Some(FifoTriggerLevel::One),
        ..Config::default()
    }
}

/// Bring up COM1, the kernel console and `ttyS0`.
pub fn init() {
    // The one place COM1's `init` is called. Re-running it to re-assert a
    // setting would also re-write the interrupt enable register from the config,
    // which knows nothing about the receive interrupt - so every console write
    // would silently turn the keyboard off again.
    COM1.init();
}

/// Write to the kernel console, COM1.
pub fn write_bytes(bytes: &[u8]) {
    COM1.write(bytes);
}

/// Let COM1's receiver interrupt.
pub fn enable_rx() {
    COM1.enable_rx();
}

/// COM1's bytes into the console input path.
pub fn drain_into_tty() {
    COM1.drain();
}

/// Whether the console may queue its output rather than wait for the wire.
pub fn interrupt_driven() -> bool {
    TX_IRQ.load(Ordering::Relaxed)
}

/// Start queueing: the IDT and the PIC are up, so the transmitter can be told
/// to interrupt for what the ring holds. Called once, where the receiver is
/// armed - before that, [`write_bytes`] is the only way a byte gets out.
pub fn enable_tx() {
    TX_IRQ.store(true, Ordering::Relaxed);
}

/// How many bytes the ring has room for.
pub fn tx_space() -> usize {
    critical_section::with(|cs| TX_RING - TX.borrow(cs).borrow().len())
}

/// Queue what fits for the transmitter to send, and ask for the interrupt that
/// does it. Returns how many bytes were taken; the rest is the caller's to
/// offer again once room appears.
///
/// The critical section covers the copy into the ring only. That is the point
/// of the whole device: waiting for the hardware to empty its holding register
/// used to happen with interrupts off, so a host that stopped draining the
/// serial line stopped the clock, the keyboard and every task with them.
pub fn tx_submit(bytes: &[u8]) -> usize {
    let n = critical_section::with(|cs| {
        let mut ring = TX.borrow(cs).borrow_mut();
        let n = (TX_RING - ring.len()).min(bytes.len());
        ring.extend(bytes[..n].iter().copied());
        n
    });
    if n > 0 {
        COM1.ier_set(IER_TX_READY, true);
    }
    n
}

/// The transmitter's interrupt: hand the ring to the hardware, and tell
/// whoever was waiting for room that some has appeared.
///
/// Bounded by [`TX_BURST`], so a stream the host will not drain costs this
/// interrupt one burst rather than the rest of the machine's time. The bytes
/// that did not fit stay queued, and the next interrupt picks them up.
pub fn pump_tx() {
    let waiters = critical_section::with(|cs| {
        let mut moved = 0usize;
        {
            let mut ring = TX.borrow(cs).borrow_mut();
            while moved < TX_BURST && COM1.tx_ready() {
                let Some(byte) = ring.pop_front() else { break };
                COM1.tx_put(byte);
                moved += 1;
            }
            if ring.is_empty() {
                // Nothing left to send: ask no more, or the UART interrupts
                // forever on an idle line.
                COM1.ier_set(IER_TX_READY, false);
            }
        }
        if moved > 0 {
            Some(core::mem::take(&mut *TX_WAITERS.borrow(cs).borrow_mut()))
        } else {
            None
        }
    });
    if let Some(waiters) = waiters {
        for id in waiters {
            crate::sched::wake(id);
        }
    }
}

/// Add the running task to the ring's wait list, before it parks on room.
pub fn tx_wait_now() {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        let mut waiters = TX_WAITERS.borrow(cs).borrow_mut();
        if !waiters.contains(&me) {
            waiters.push(me);
        }
    });
}

/// Take the running task back off the list - the wait ended however it ended.
pub fn tx_stop_waiting() {
    let me = crate::sched::current();
    critical_section::with(|cs| {
        TX_WAITERS.borrow(cs).borrow_mut().retain(|w| *w != me);
    });
}
