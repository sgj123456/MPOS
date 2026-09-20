//! Kernel console — architecture-agnostic formatting over `arch::serial`.
//!
//! This module is a **kernel-wide service**, not a driver or a VFS component.
//! It wraps the raw serial driver with interrupt-safe output and provides the
//! `println!` macro. Callers include the kernel itself (`println!`), the TTY
//! line discipline (echo output), and the syscall `write` handler.

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

/// Write raw bytes on the console, as one uninterruptible chunk.
///
/// The syscall `write` handler uses this: it has bytes rather than format
/// arguments, and the same line-at-a-time exclusion as [`print_args`] so a
/// preempted task cannot interleave into the middle of one.
pub fn write_bytes(bytes: &[u8]) {
    critical_section::with(|_| crate::drivers::serial::write_bytes(bytes));
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
