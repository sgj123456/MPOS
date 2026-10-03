//! QEMU test-exit device.
//!
//! `-device isa-debug-exit,iobase=0xf4` makes QEMU terminate with status
//! `(value << 1) | 1` when the guest writes `value` to that port. This lets the
//! kernel end a test run deterministically instead of being SIGKILLed by a
//! watchdog, and gives the harness a real exit status: 0x10 -> 33 on success,
//! 0x11 -> 35 on panic.

use x86_64::instructions::port::Port;

const EXIT_PORT: u16 = 0xf4;

pub const EXIT_SUCCESS: u32 = 0x10;
pub const EXIT_FAILURE: u32 = 0x11;

pub fn exit(code: u32) -> ! {
    unsafe {
        let mut port: Port<u32> = Port::new(EXIT_PORT);
        port.write(code);
    }
    // Unreachable on QEMU; on real hardware fall through to a halt.
    crate::hal::halt()
}
