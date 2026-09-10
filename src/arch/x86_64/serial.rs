//! COM1: legacy 16550 on I/O ports 0x3f8..0x3ff.
//!
//! Output is a plain write. Input is an interrupt: the receiver's data-ready
//! line is IRQ4, and [`drain_into_tty`] is how the handler empties the FIFO
//! and hands what it finds to the console's line discipline.
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

const COM1_BASE_PORT: u16 = 0x3f8;
/// The interrupt enable register. Writing here is the one thing the wrapper
/// crate has no method for, and its `Config` can only set it at init time -
/// which is before the IDT exists and therefore too early.
const COM1_IER: u16 = COM1_BASE_PORT + 1;
/// IER bit 0: raise an interrupt whenever a byte is ready to be read.
const IER_DATA_READY: u8 = 1 << 0;
/// The receive buffer, at offset 0. Reading it takes one byte out of the FIFO.
const COM1_DATA: u16 = COM1_BASE_PORT;
/// The line status register, at offset 5.
const COM1_LSR: u16 = COM1_BASE_PORT + 5;

/// A freshly built UART, re-created per call. The type is a thin wrapper over
/// the port and the config, and the port is the real state, so this costs
/// nothing worth keeping a shared instance for.
fn uart() -> Uart16550<uart_16550::backend::PioBackend> {
    // SAFETY: COM1 is the kernel's alone, and it stays valid for the life of
    // the kernel.
    unsafe { Uart16550::new_port(COM1_BASE_PORT) }.expect("COM1 base port invalid")
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

pub fn init() {
    // The one place `init` is called. Re-running it to re-assert a setting
    // would also re-write the interrupt enable register from the config, which
    // knows nothing about the receive interrupt - so every console write would
    // silently turn the keyboard off again.
    uart()
        .init(console_config())
        .expect("COM1 rejected the console line config");
}

pub fn write_bytes(bytes: &[u8]) {
    // A fresh wrapper over the same port, with no init: the UART's registers
    // are the state, and they are already set up.
    uart().send_bytes_exact(bytes);
}

/// Ask the UART to interrupt on every byte that arrives, and unmask IRQ4.
///
/// Deliberately not part of [`init`]: the handler feeds the tty, and until the
/// scripted boot checks are done there is nothing that should be typing.
pub fn enable_rx() {
    let mut ier: Port<u8> = Port::new(COM1_IER);
    // SAFETY: COM1 is the kernel's alone, and the register is a byte. Read
    // first so a bit another path set is not cleared.
    let current = unsafe { ier.read() };
    unsafe { ier.write(current | IER_DATA_READY) };
    super::pic::set_masked(4, false);
}

/// Every byte the receiver is holding goes into the tty line discipline.
///
/// Called from the IRQ4 handler, inside a critical section the console may
/// already be holding: nothing here locks, and nothing here prints.
pub fn drain_into_tty() {
    let tty = crate::vfs::tty::global();
    let mut data: Port<u8> = Port::new(COM1_DATA);
    let mut lsr: Port<u8> = Port::new(COM1_LSR);
    // A bounded loop: a receiver stuck asserting "data ready" - broken
    // hardware, or a byte stream with no end - must not hang the handler.
    for _ in 0..MAX_DRAIN {
        // SAFETY: both registers are COM1's, and both reads have a side
        // effect the hardware defines: LSR clears the error flags, DATA takes
        // one byte out of the FIFO.
        let status = unsafe { lsr.read() };
        if status & LSR::DATA_READY.bits() == 0 {
            break;
        }
        let byte = unsafe { data.read() };
        tty.receive_byte(byte);
    }
}

/// How many bytes one interrupt will move before giving the rest back to the
/// next one. The FIFO holds 16, so this is never reached with working
/// hardware; it exists so the handler cannot spin forever if it does.
const MAX_DRAIN: usize = 64;
