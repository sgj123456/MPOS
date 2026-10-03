//! The PL011 UART, as QEMU's `virt` machine wires it.
//!
//! This is the aarch64 machine's first and only console until stage 4, and it
//! is deliberately the *machine's* rather than the kernel's: `mpos::console` is
//! the x86 mux (a framebuffer, six virtual consoles and the `term` grid behind
//! them), and none of that exists here yet. Stage 4 puts the real console above
//! this; until then `arch/aarch64` owns the wire, which `PLAN.md` §9.1 lists as
//! a named gap rather than hiding behind a stub.
//!
//! Where the address comes from. `0x0900_0000` is where QEMU's `virt` machine
//! places the first PL011, and where the firmware's device tree says it is - the
//! tree is what `check_devices` holds that number to. The port is *reached*
//! through the kernel half's device window rather than at that physical address,
//! which is stage 5b's arrangement and [`BASE`]'s comment.
//!
//! Why there is no lock. Writes here wait for the transmit FIFO to have room
//! and then write one byte, which is as atomic as a byte-at-a-time sink needs to
//! be, and at this stage nothing has unmasked an interrupt: there is no second
//! writer to interleave with. The lock arrives with stage 1c's clock, at the
//! same moment as the first interrupt.

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
/// Interrupt clear: write 1s to ack.
const ICR: usize = 0x44;

/// `TXFF` - the transmit FIFO is full, so a byte written now would be lost.
const FR_TXFF: u32 = 1 << 5;
/// `RXFE` - the receive FIFO is empty, so there is nothing to read yet.
const FR_RXFE: u32 = 1 << 4;
/// `LBE` - loopback enable: the transmit line is wired back into the receiver
/// and the wire output is disabled. This is how RX is exercised without a
/// terminal, which the boot self-test uses (see [`loopback_echoes`]).
const CR_LBE: u32 = 1 << 7;

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

/// The `core::fmt` sink, so `write!` works against this UART.
pub struct Writer;
impl fmt::Write for Writer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_str(text);
        Ok(())
    }
}
