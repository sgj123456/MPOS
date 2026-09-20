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
//! The interrupt stays masked until [`enable_rx`] asks for it, so nothing
//! arrives while the kernel is still finding its feet.

use uart_16550::spec::registers::{FifoTriggerLevel, LSR};
use uart_16550::{Config, Uart16550};
use x86_64::instructions::port::Port;

/// IER bit 0: raise an interrupt whenever a byte is ready to be read.
const IER_DATA_READY: u8 = 1 << 0;

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
        let mut ier = self.ier();
        // SAFETY: the register is a byte. Read first so a bit another path set
        // is not cleared.
        let current = unsafe { ier.read() };
        unsafe { ier.write(current | IER_DATA_READY) };
        crate::arch::pic::set_masked(4, false);
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
