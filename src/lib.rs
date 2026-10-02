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
//!
//! # Which half a module belongs in
//!
//! The gate below is a judgement made per module, so the criterion it is made
//! with is written down here (`PLAN.md` §11.2 Step F):
//!
//! > A module is host-compiled when its logic touches no hardware, no port I/O,
//! > no virtio ring and no real clock — and then *its dependencies must be
//! > host-compiled too, transitively*.
//!
//! The second half is the one that gets forgotten, and the compiler cannot
//! notice when it is, because the failure is silent in both directions: a
//! `#[cfg(test)]` module written behind one of these gates is dead code that
//! reads as a passing test, and a `crate::` reference to a gated module from a
//! host-compiled one is a build error only on the day somebody builds for the
//! host. `xtask`'s `gates::dead_host_tests`, run by `cargo xtask test`, walks
//! this file's `mod` declarations and fails on the first shape.

#![no_std]
#![cfg_attr(
    all(target_os = "none", target_arch = "x86_64"),
    feature(abi_x86_interrupt)
)]

extern crate alloc;

/// The AHCI port's own answers, as bits rather than as bus cycles: what a
/// status word means, and when waiting on one has to stop. Its own leaf crate -
/// the decode is arithmetic on three `u32`s and the machine half
/// (`drivers::ahci`, which reads the registers and moves the sectors) can only
/// be tested on a machine. `pci` beside `drivers::pci` is the same split, and
/// `mpos-pci` is the same kind of crate.
pub use mpos_ahci as ahci;
#[cfg(target_os = "none")]
pub mod arch;
/// Binary formats: how the kernel decides what runs a file. Its own leaf crate
/// (reading a `#!` line is pure); re-exported so `crate::binfmt` still resolves.
pub use mpos_binfmt as binfmt;
pub mod blk;
pub mod boot;
#[cfg(target_os = "none")]
pub mod console;
/// The process credential model: the four capability sets, the dumpable flag
/// and `KEEPCAPS`. Its own pure leaf crate; the storage lives on the thread
/// group in `sched`, and the consumers (`execve`'s re-derivation and the
/// ptrace/`process_vm` gate) call in through this path.
pub use mpos_cred as cred;
/// The civil calendar: day counts to dates and back. Its own pure leaf crate;
/// the `arch` half that writes it to hardware stays behind the target gate.
pub use mpos_date as date;
#[cfg(target_os = "none")]
pub mod drivers;
/// The errno table: the numbers every layer answers "no" with. Its own leaf
/// crate, re-exported here so `crate::err` keeps resolving for every caller,
/// while the crate itself cannot name the kernel and so cannot reach back up.
pub use mpos_err as err;
/// evdev's record window and its clients: which reader sees which event, and
/// what a grab does to the answer. Not target-gated - the driver that fills the
/// window needs a virtio ring and a clock, but the policy over it is arithmetic,
/// and arithmetic is worth a host test.
pub mod evdev;
/// The GPU's host-testable logic: the DRM blobs, cursor composition,
/// sync-object state machine, the `DRM_VIRTGPU_*` answers, and the virtio-gpu
/// wire ABI. Each is pure and tested on the host; the machine bodies stay in
/// `drivers`.
pub mod gpu;
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
/// them with. Its own leaf crate: the model and every spelling are pure; only
/// the port I/O that *finds* the devices is machine-dependent, in `drivers`.
pub use mpos_pci as pci;
/// The kernel's random source, which `/dev/urandom` and `getrandom(2)` both
/// read. Not target-gated: it runs on the host so its tests can prove the one
/// property that matters - that two reads are not the same bytes.
pub mod rand;
pub mod sched;
/// `SECCOMP_MODE_FILTER`'s program: the classic-BPF interpreter, its validation
/// and the action ordering. Its own leaf crate (nothing in it knows what a task
/// is); the task table and the syscall gate decide who has one and when it runs.
pub use mpos_seccomp as seccomp;
pub mod sync;
pub mod syscall;
/// The terminal emulator: the cell grid a byte stream is painted into, the
/// cursor it moves, the blitter that turns cells into pixels and the keystroke
/// map. Its own leaf crate - a terminal emulator is not a filesystem fact
/// (`PLAN.md` §11.1.4) - so `console`, `drivers/video` and `vfs::tty` come here
/// for a grid rather than reaching into `vfs` for one.
pub use mpos_term as term;
/// The userland-boundary primitives - validate a caller's pointer, and turn a
/// `VfsError` into the errno userland reads. They live at the crate root so a
/// layer *below* `syscall` (net, a driver) can reach them without naming the
/// ABI layer for a page walk; `syscall` re-exports them for its own handlers.
pub mod uaccess;
/// Device events: what the kernel tells userland when a device
/// appears, and the netlink socket they travel over. Not target-gated:
/// nothing here needs hardware, and the wire format is worth testing
/// on the host.
pub mod uevent;
/// uinput's interface: the requests and bitmaps a userspace program declares an
/// input device with. Not target-gated, for the same reason as `evdev` - the
/// arithmetic is worth a host test, and the device that uses it is bare metal.
pub mod uinput;
/// The UTS namespace: the two names `uname(2)` reports, as the object a task
/// holds rather than as a pair of machine-wide strings. Its own leaf crate
/// (*ownership* over two `String`s); the task table that decides whose
/// namespace is whose is bare metal.
pub use mpos_uts as uts;
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
