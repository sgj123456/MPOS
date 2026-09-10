//! Kernel console. Architecture-agnostic formatting over `arch::serial`.

use core::fmt::{self, Write};

pub struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::arch::serial::write_bytes(s.as_bytes());
        Ok(())
    }
}

pub fn init() {
    crate::arch::serial::init();
}

/// Write raw bytes on the console, as one uninterruptible chunk.
///
/// The syscall `write` handler uses this: it has bytes rather than format
/// arguments, and the same line-at-a-time exclusion as [`print_args`] so a
/// preempted task cannot interleave into the middle of one.
pub fn write_bytes(bytes: &[u8]) {
    critical_section::with(|_| crate::arch::serial::write_bytes(bytes));
}

#[doc(hidden)]
pub fn print_args(args: fmt::Arguments<'_>) {
    // A whole line at a time. Tasks preempt each other anywhere, including
    // halfway through a format argument, and two of them writing to one UART
    // produce output neither meant.
    //
    // This holds interrupts off for the length of a line, which is the cheap
    // version of a lock rather than the right one: it is fine while the only
    // writer is QEMU's 16550, but a real UART at 115200 baud takes ~90us a byte
    // and a kernel that blanks the clock for that long will lose ticks. It
    // becomes a lock that yields when the tty arrives.
    critical_section::with(|_| {
        let _ = Console.write_fmt(args);
    });
}
