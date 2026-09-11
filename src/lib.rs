//! mpos kernel core.
//!
//! The crate is split so that everything which does not touch machine state
//! lives in this library and can be unit-tested on the host:
//!
//!     cargo test --lib --target x86_64-unknown-linux-gnu
//!
//! Machine-dependent code (`arch`, the console, the Limine translation) is
//! compiled only for `target_os = "none"`, which is what keeps the host build
//! free of port I/O, descriptor tables and bootloader protocol.

#![no_std]
#![cfg_attr(
    all(target_os = "none", target_arch = "x86_64"),
    feature(abi_x86_interrupt)
)]

extern crate alloc;

#[cfg(target_os = "none")]
pub mod arch;
pub mod blk;
pub mod boot;
#[cfg(target_os = "none")]
pub mod console;
#[cfg(target_os = "none")]
pub mod drivers;
pub mod mm;
pub mod sched;
pub mod shell;
pub mod sync;
pub mod syscall;
pub mod vfs;

#[cfg(target_os = "none")]
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::print_args(format_args!($($arg)*)));
}

#[cfg(target_os = "none")]
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
