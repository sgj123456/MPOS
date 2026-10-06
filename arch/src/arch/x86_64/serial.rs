//! The machine's console UART: COM1, `ttyS0`.
//!
//! COM1 is the one wire to the world - the kernel console, where every
//! `tracing` event and `println!` goes, and the wire the console multiplexer
//! carries the virtual consoles' traffic over. There is no second serial
//! terminal: the shells run on the pty-backed `/dev/tty1`..`tty6`, and
//! `vcmux` moves their bytes over this port.
//!
//! Output is a plain write when the writer cannot queue (before the interrupt
//! is armed, from a handler, on the panic path). Input is an interrupt: the
//! receiver's data-ready line is IRQ4, and [`drain_into_tty`] is how the handler
//! empties the FIFO and hands what it finds to the multiplexer.
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
//! The transmitter's half is not this module's any more. The queue the bytes
//! wait in, who sleeps for room in it, and when to stop asking the UART for an
//! interrupt are one answer rather than two, in `crate::hal::tx_ring`, and
//! this file implements [`Transmitter`] over the port - which is the whole of
//! what distinguishes one console UART from the other.
//!
//! The interrupt stays masked until [`enable_rx`] asks for it, so nothing
//! arrives while the kernel is still finding its feet.

use uart_16550::spec::registers::{FifoTriggerLevel, LSR};
use uart_16550::{Config, Uart16550};
use x86_64::instructions::port::Port;

use crate::hal::tx_ring;

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
            crate::hal::kernel::route_console_input(byte);
        }
    }
}

/// COM1 as [`crate::hal::tx_ring`]'s transmitter: the three answers that
/// ring asks of any UART, and nothing about who waits for it or why.
///
/// Because `Uart` is a `Copy` wrapper over a port number rather than over the
/// port's state, there is nothing for an instance to carry - [`COM1`] is a
/// `const` and each op rebuilds the thin wrapper the crate needs, which is why
/// the trait's methods take no `self`.
impl crate::hal::tx_ring::Transmitter for Uart {
    fn tx_ready() -> bool {
        // Deliberately not `Self::tx_ready(&COM1)`: the ring's trait is the
        // answer for *the* console transmitter of whichever machine this is, and
        // on this one there is exactly one - the same reason [`write_bytes`] has
        // no port argument.
        COM1.tx_ready()
    }

    fn tx_put(byte: u8) {
        COM1.tx_put(byte);
    }

    fn tx_interrupt(on: bool) {
        COM1.ier_set(IER_TX_READY, on);
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
    tx_ring::enabled()
}

/// Start queueing: the IDT and the PIC are up, so the transmitter can be told
/// to interrupt for what the ring holds. Called once, where the receiver is
/// armed - before that, [`write_bytes`] is the only way a byte gets out.
pub fn enable_tx() {
    tx_ring::enable();
}

/// How many bytes the ring has room for.
pub fn tx_space() -> usize {
    tx_ring::space()
}

/// Queue what fits for the transmitter to send, and ask for the interrupt that
/// does it. Returns how many bytes were taken; the rest is the caller's to
/// offer again once room appears.
///
/// The reasoning this used to carry - the bounded critical section, and why it
/// must not cover a wait for the hardware - lives with the queue now
/// (`tx_ring::submit`): it is the same reasoning either machine's console needs,
/// and this is the 16550's half of it.
pub fn tx_submit(bytes: &[u8]) -> usize {
    tx_ring::submit::<Uart>(bytes)
}

/// The transmitter's interrupt: hand the ring to the hardware, and tell whoever
/// was waiting for room that some has appeared.
///
/// Called from the serial handler alongside [`drain_into_tty`], because a 16550
/// at IRQ4 has one line for both directions: the visit that took a keystroke is
/// also the visit that must move the queue.
pub fn pump_tx() {
    tx_ring::pump::<Uart>();
}

/// Add the running task to the ring's wait list, before it parks on room.
pub fn tx_wait_now() {
    tx_ring::wait_now();
}

/// Take the running task back off the list - the wait ended however it ended.
pub fn tx_stop_waiting() {
    tx_ring::stop_waiting();
}
