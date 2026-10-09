//! The PL011 UART, as QEMU's `virt` machine wires it.
//!
//! This is the aarch64 machine's first and only console until stage 4, and it
//! is deliberately the *machine's* rather than the kernel's: `mpos::console` is
//! the x86 mux (a framebuffer, six virtual consoles and the `term` grid behind
//! them), and none of that exists here yet. Stage 4 puts the real console above
//! this; until then `arch/aarch64` owns the wire, which `NOTES.md` §9.1 lists as
//! a named gap rather than hiding behind a stub.
//!
//! Where the address comes from. `0x0900_0000` is where QEMU's `virt` machine
//! places the first PL011, and where the firmware's device tree says it is - the
//! tree is what `check_devices` holds that number to. The port is *reached*
//! through the kernel half's device window rather than at that physical address,
//! which is stage 5b's arrangement and [`BASE`]'s comment.
//!
//! What arrived since, and why the polled write stayed: this machine now has a
//! transmitter for [`crate::hal::tx_ring`] and an interrupt handler, and both
//! sit *above* the byte sink below rather than replacing it. A panic, an early
//! boot line and an interrupt context all write through [`write_byte`], because
//! all three have to reach the wire without a ring, a task or a vector table to
//! help them.
//!
//! Why there is no lock. Writes here wait for the transmit FIFO to have room

use core::fmt;

use super::paging;

/// The physical address of the first PL011 on QEMU's `virt` machine: what the
/// device tree names, and what the tree check compares against.
pub const PHYS: usize = 0x0900_0000;

/// The port as this kernel reaches it: the kernel half's device window, at this
/// port's offset into it.
///
/// The window's own comment says why the registers are not mapped where they
/// physically are. Spelled as the window's base plus the offset rather than as a
/// second literal, so that a window that moves takes this with it rather than
/// leaving a driver reading unmapped memory.
pub const BASE: usize = paging::DEVICE_VIRT + (PHYS - paging::DEVICE_PHYS);

/// Data register: what is written goes out, what is read comes in.
const DR: usize = 0x00;
/// Flag register: bit 5 is `TXFF`, "the transmit FIFO is full".
const FR: usize = 0x18;
/// Integer part of the baud divisor.
const IBRD: usize = 0x24;
/// Fractional part, in 1/64ths.
const FBRD: usize = 0x28;
/// Line control: word length, FIFO enable, parity, stop bits.
const LCR_H: usize = 0x2C;
/// Control: the enable bits.
const CR: usize = 0x30;
/// Interrupt mask set/clear: one bit per interrupt source, and unlike the
/// 16550's `IER` it is a register of its own rather than a second function of
/// the data port.
const IMSC: usize = 0x38;
/// Masked interrupt status: what the UART is asking for now, its raw causes
/// filtered by [`IMSC`]. Reading it is how one visit from the line tells which
/// of the two directions it is for.
const MIS: usize = 0x40;
/// Interrupt clear: write 1s to ack.
const ICR: usize = 0x44;
/// Every source `ICR` carries, on a UART that has them all.
const ICR_ALL: u32 = 0x7ff;
/// `ICR` bit 5, the transmit event - the one source [`enable_interrupts`] must
/// *not* clear. See it for the measurement.
const ICR_TXIC: u32 = 1 << 5;

/// `MIS` bit 4: receive interrupt, after [`IMSC`]'s mask.
const MIS_RX: u32 = 1 << 4;
/// `MIS` bit 5: transmit interrupt, likewise.
const MIS_TX: u32 = 1 << 5;

/// `TXFF` - the transmit FIFO is full, so a byte written now would be lost.
const FR_TXFF: u32 = 1 << 5;
/// `RXFE` - the receive FIFO is empty, so there is nothing to read yet.
const FR_RXFE: u32 = 1 << 4;
/// `LBE` - loopback enable: the transmit line is wired back into the receiver
/// and the wire output is disabled. This is how RX is exercised without a
/// terminal, which the boot self-test uses (see [`loopback_echoes`]).
const CR_LBE: u32 = 1 << 7;

/// `IMSC` bit 4: hand every received byte to an interrupt rather than waiting
/// for someone to poll the FIFO.
const IMSC_RXIM: u32 = 1 << 4;
/// `IMSC` bit 5: interrupt whenever the transmit FIFO has room. Armed only
/// while the ring holds something, or an idle UART asserts the line for ever -
/// the same invariant the 16550's `IER` needs, on a register that multiplexes
/// one line instead of two.
const IMSC_TXIM: u32 = 1 << 5;

fn reg(offset: usize) -> *mut u32 {
    (BASE + offset) as *mut u32
}

fn read(offset: usize) -> u32 {
    // SAFETY: every offset above is a register of the PL011 at `BASE`, and the
    // whole 4 KiB page is device memory that is mapped as soon as the machine
    // starts - the kernel does not need to map it before the first use.
    unsafe { core::ptr::read_volatile(reg(offset)) }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as `read`.
    unsafe { core::ptr::write_volatile(reg(offset), value) };
}

/// Ask for the console's interrupt - the shared wire both directions hang off.
///
/// Called once, from the wiring layer, once there is somewhere for a typed byte
/// to go ([`handle_interrupt`] hands one straight to the console tty) and once
/// the queue the transmitter drains has somewhere to be filled from. Earlier is
/// available and wrong: a receiver that interrupted before the tty existed would
/// take a panic inside an interrupt handler, which prints nowhere.
///
/// Only the receiver is armed. The transmitter's bit is the queue's to set and
/// clear per burst (`tx_ring::Transmitter::tx_interrupt`), which is the only way
/// an idle line can stop asserting.
pub fn enable_interrupts() {
    // Every source *except* the transmitter's. That exception is the reason this
    // is not `0x7ff`: `TXIC` clears a latched transmit event, and a
    // level-triggered transmitter that has lost its latch stays silent until the
    // next byte leaves it - which is precisely the event the queue is waiting for,
    // so nothing ever writes one. Measured as a boot that hung about half the
    // time at the first queued line, silent, with 559 bytes in the ring and the
    // UART asked for nothing: clearing the bit took away the queue's only chance
    // to be drained. The receiver's side is cleared, because a stale byte from
    // before the kernel was ready would otherwise be handed to a tty that was.
    write(ICR, ICR_ALL & !ICR_TXIC);
    imsc_set(IMSC_RXIM, true);
    // The group and the priority are the controller's business and were chosen
    // when it was configured; the line itself is what has to let through.
    crate::hal::unmask(crate::hal::Line::Serial);
}

/// Set or clear one bit of the interrupt mask, leaving the rest as they are.
///
/// Read-modify-write rather than a plain store because this register carries
/// *both* directions: a store that armed the transmitter would silently disarm
/// the receiver, and a keypress would be missed by a machine that had just
/// written one line.
fn imsc_set(bit: u32, on: bool) {
    let current = read(IMSC);
    let next = if on { current | bit } else { current & !bit };
    write(IMSC, next);
}

/// Bring the UART up: 8 bits, no parity, one stop bit, FIFOs on, 115200 baud
/// from a 24 MHz reference - the clock QEMU's `virt` gives it.
///
/// The order is the one the TRM gives: the control register off while the line
/// control is changed, so the UART cannot latch a half-configured frame.
pub fn init() {
    write(CR, 0);
    // Clear every interrupt: nothing here handles one, and a stale `RIS` bit
    // would be a puzzle for whoever enables interrupts at stage 1c.
    write(ICR, 0x7ff);
    // 24 MHz / (16 * 115200) = 13.0208...; the fractional register holds the
    // 0.0208 in 1/64ths, which rounds to 1.
    write(IBRD, 13);
    write(FBRD, 1);
    // WLEN = 3 (8 bits) << 5, FEN (FIFOs on) << 4.
    write(LCR_H, (3 << 5) | (1 << 4));
    // UARTEN | TXE | RXE.
    write(CR, 1 | (1 << 8) | (1 << 9));
}

/// Write one byte, waiting for room.
///
/// The wait is a poll of `TXFF` rather than an interrupt because there is no
/// interrupt controller yet; it is bounded by the FIFO's depth and the wire
/// speed, and every caller so far is a boot-time line.
pub fn write_byte(byte: u8) {
    while read(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    write(DR, byte as u32);
}

/// Write a string, turning each newline into a carriage return and a newline.
///
/// A terminal needs both, and a kernel that sends only `\n` prints a staircase
/// on hardware that does not translate - which is every serial terminal that is
/// not itself a Unix tty. The x86 console does the same for the same reason.
pub fn write_str(text: &str) {
    for byte in text.bytes() {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// Read one received byte, or `None` if the receive FIFO is empty right now.
///
/// Non-blocking by design: the interactive console this feeds has to poll the
/// wire between other work, and a read that parks on a byte that may never come
/// is the failure the x86 console already had to be fixed for. Turning this into
/// a blocking read with wakeups is the interrupt path (the PL011's `RIS`/`IMSC`
/// lines through the GIC), which is a later stage.
pub fn read_byte() -> Option<u8> {
    if read(FR) & FR_RXFE != 0 {
        None
    } else {
        // `DR`'s low byte is the received datum; the upper bits are a framing
        // status echo we do not surface here.
        Some((read(DR) & 0xff) as u8)
    }
}

/// Drive the UART's own loopback and check a written byte comes back on the
/// receiver, without a terminal or the wire.
///
/// `LBE` ties transmit to receive inside the PL011 and disables the wire output,
/// so this exercises the RX register path (`FR_RXFE`, `DR`) deterministically and
/// silently - the boot cannot type into a headless run, but it can prove the read
/// half of the console actually reads. Returns whether the probe byte echoed back.
pub fn loopback_echoes(probe: u8) -> bool {
    let cr = read(CR);
    write(CR, cr | CR_LBE);
    write_byte(probe);
    // A bounded poll: in loopback the echo is internal and immediate, so an empty
    // FIFO here means the RX path is genuinely broken, not merely slow.
    let mut echoed = None;
    for _ in 0..1000 {
        if let Some(byte) = read_byte() {
            echoed = Some(byte);
            break;
        }
        core::hint::spin_loop();
    }
    write(CR, cr & !CR_LBE);
    echoed == Some(probe)
}

/// Put `byte` into the receive FIFO using the UART's own loopback, without a
/// terminal, so a later [`read_byte`] drains it.
///
/// The same `LBE` trick as [`loopback_echoes`], but the byte is left in the FIFO
/// for the caller to read on the *next* pass rather than consumed here. This is
/// how the tty receive path is pinned at boot: the console has no keyboard or
/// wire byte to hand it in a headless run, but loopback can hand it one it will
/// then read back through the real RX registers.
pub fn inject_via_loopback(byte: u8) {
    let cr = read(CR);
    write(CR, cr | CR_LBE);
    write_byte(byte);
    write(CR, cr & !CR_LBE);
}

/// The PL011 as [`crate::hal::tx_ring`]'s transmitter.
///
/// Three answers, and no policy: this UART will take a byte when `TXFF` is
/// clear, takes it through `DR`, and asks for its own interrupt through one bit
/// of [`IMSC`]. Whether anybody *wants* the interrupt, and who sleeps when the
/// queue is full, is the ring's - which is what lets this machine's console
/// queue-and-park path be the same code the first machine's is rather than a
/// second copy of it written against this register.
impl crate::hal::tx_ring::Transmitter for Pl011 {
    fn tx_ready() -> bool {
        read(FR) & FR_TXFF == 0
    }

    fn tx_put(byte: u8) {
        write(DR, byte as u32);
    }

    fn tx_interrupt(on: bool) {
        imsc_set(IMSC_TXIM, on);
    }
}

/// The tag [`Transmitter`] is implemented for. A unit type rather than nothing
/// because the trait's methods take no `self`: there is one PL011 at one
/// address, and instances of it would have nothing to hold.
pub struct Pl011;

/// One visit from the UART's interrupt, for both directions at once.
///
/// The receiver's bytes go to the console first, then the transmitter's queue -
/// the order x86_64's shared IRQ4 handler uses, and for the same reason: a
/// keystroke echoed by the tty is itself bytes going out, and the visit that
/// took it should be the visit that sends them rather than leaving them for the
/// next one.
///
/// Both halves are bounded, so a line stuck asserting one direction costs this
/// interrupt its bound and not the machine's time: the receiver drains while the
/// FIFO says it has bytes (up to [`MAX_DRAIN`] of them), and the transmitter
/// hands over up to one burst of the queue's choosing.
///
/// Called from the machine's IRQ vector, with no critical section entered by
/// this function: the tty it writes into is entered under one by `vfs`, and the
/// queue takes its own around the copy it makes.
pub fn handle_interrupt() {
    // What the UART was asking for with its own mask applied. Read once, before
    // either half runs, because both halves can change it.
    let status = read(MIS);
    if status & MIS_RX != 0 {
        drain_input();
    }
    if status & MIS_TX != 0 {
        crate::hal::tx_ring::pump::<Pl011>();
    }
}

/// Every byte the receiver is holding goes to the console's input path, exactly
/// as the 16550's handler hands its FIFO to the console's mux - the same function
/// over the same kind of wire, so a keystroke means the same thing on both
/// machines ([`crate::vfs::vcmux::route_input`]).
///
/// That function is also what decides *which* terminal a byte belongs to: the
/// active virtual console once the mux is up, and the console tty itself before
/// it is. Which is why this is one call and not the tty's own `receive_byte`, and
/// why the boot's console checks - which run *before* the mux, deliberately - go
/// on reading bytes from the console tty directly.
///
/// Called from an interrupt, so it neither locks nor prints - the same contract
/// `drivers::serial::drain` carries, and for the same reason there: an interrupt
/// taken inside a critical section the console is holding is a section that run
/// is already inside.
fn drain_input() {
    for _ in 0..MAX_DRAIN {
        if read(FR) & FR_RXFE != 0 {
            break;
        }
        let byte = (read(DR) & 0xff) as u8;
        crate::hal::kernel::route_console_input(byte);
    }
}

/// How many bytes one interrupt will move, per direction. The FIFO holds 32 on
/// this UART against the 16550's 16, so this is never reached with working
/// hardware; it exists so neither half can spin forever if it is.
const MAX_DRAIN: usize = 64;

/// The `core::fmt` sink, so `write!` works against this UART.
pub struct Writer;
impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_str(text);
        Ok(())
    }
}
