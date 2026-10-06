//! QEMU test-exit device: this machine's [`hal::TestExit`].
//!
//! `-device isa-debug-exit,iobase=0xf4` makes QEMU terminate with status
//! `(value << 1) | 1` when the guest writes `value` to that port. This lets the
//! kernel end a test run deterministically instead of being SIGKILLed by a
//! watchdog, and gives the harness a real exit status: 0x10 -> 33 on success,
//! 0x11 -> 35 on panic.
//!
//! The port, and the fact that this machine *has* such a port, are the whole of
//! what lives here. The codes are the harness's contract and live in `hal`
//! beside the capability (`hal::TEST_EXIT_OK` / `hal::TEST_EXIT_PANIC`); the
//! callers - `reboot(2)`'s stop path and the panic handler - name
//! `hal::exit_qemu` and not this module, so a machine with no debug-exit device
//! is a build without the capability rather than a second body of code
//! (`PLAN.md` §14.3).

use x86_64::instructions::port::Port;

const EXIT_PORT: u16 = 0xf4;

impl crate::hal::TestExit for crate::hal::Current {
    fn exit_qemu(code: u32) -> ! {
        unsafe {
            let mut port: Port<u32> = Port::new(EXIT_PORT);
            port.write(code);
        }
        // Unreachable on QEMU; on real hardware, or an emulator started without
        // the device, fall through to a halt.
        crate::hal::halt()
    }
}
