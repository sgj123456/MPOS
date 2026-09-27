//! mpos kernel core.
//!
//! The crate is split so that everything which does not touch machine state
//! lives in this library and can be unit-tested on the host:
//!
//!     cargo test --lib --target x86_64-unknown-linux-gnu
//!
//! Machine-dependent code (`arch`, the console, the boot handoff) is compiled
//! only for `target_os = "none"`, which is what keeps the host build free of
//! port I/O, descriptor tables and firmware calls.

#![no_std]
#![cfg_attr(
    all(target_os = "none", target_arch = "x86_64"),
    feature(abi_x86_interrupt)
)]

extern crate alloc;

#[cfg(target_os = "none")]
pub mod arch;
/// Binary formats: how the kernel decides what runs a file. Host-compiled,
/// because reading a `#!` line is pure and its edge cases deserve tests.
pub mod binfmt;
pub mod blk;
pub mod boot;
#[cfg(target_os = "none")]
pub mod console;
/// The civil calendar: day counts to dates and back. Not target-gated - the
/// arithmetic is pure, and the `arch` half that writes it to hardware is
/// compiled where its tests are not, so the sums live here where a host
/// build can argue with them.
pub mod date;
#[cfg(target_os = "none")]
pub mod drivers;
/// evdev's record window and its clients: which reader sees which event, and
/// what a grab does to the answer. Not target-gated - the driver that fills the
/// window needs a virtio ring and a clock, but the policy over it is arithmetic,
/// and arithmetic is worth a host test.
pub mod evdev;
/// Kernel logging: `tracing` events, formatted onto the serial console.
/// Ungated so the level names and the command-line parsing stay testable on
/// the host; only the subscriber that writes to COM1 is machine-specific.
pub mod logging;
pub mod mm;
/// The network stack, and the socket syscalls over it. Host builds skip it:
/// its tests are the conformance programs that run on the machine.
#[cfg(target_os = "none")]
pub mod net;
/// The PCI devices the machine has, and the files `/sys` and `/proc` describe
/// them with. Not target-gated: the model and every spelling are pure, and the
/// formats pciutils parses deserve tests a host can run. Only the port I/O
/// that *finds* the devices is machine-dependent, and that lives in `drivers`.
pub mod pci;
/// The kernel's random source, which `/dev/urandom` and `getrandom(2)` both
/// read. Not target-gated: it runs on the host so its tests can prove the one
/// property that matters - that two reads are not the same bytes.
pub mod rand;
pub mod sched;
/// `SECCOMP_MODE_FILTER`'s program: the classic-BPF interpreter, its validation
/// and the action ordering. Ungated because it is arithmetic over a byte block -
/// nothing in it knows what a task is - so a host build can run every filter a
/// test cares to write; the task table and the syscall gate are what decide who
/// has one and when it runs.
pub mod seccomp;
pub mod sync;
pub mod syscall;
/// Device events: what the kernel tells userland when a device
/// appears, and the netlink socket they travel over. Not target-gated:
/// nothing here needs hardware, and the wire format is worth testing
/// on the host.
pub mod uevent;
/// uinput's interface: the requests and bitmaps a userspace program declares an
/// input device with. Not target-gated, for the same reason as `evdev` - the
/// arithmetic is worth a host test, and the device that uses it is bare metal.
pub mod uinput;
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
