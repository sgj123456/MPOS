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

/// The AHCI port's own answers, as bits rather than as bus cycles: what a
/// status word means, and when waiting on one has to stop. Ungated because the
/// decode is arithmetic on three `u32`s and the machine half -
/// `drivers::ahci`, which reads the registers and moves the sectors - can only
/// be tested on a machine. `pci` beside `drivers::pci` is the same split.
pub mod ahci;
#[cfg(target_os = "none")]
pub mod arch;
/// Binary formats: how the kernel decides what runs a file. Host-compiled,
/// because reading a `#!` line is pure and its edge cases deserve tests.
pub mod binfmt;
pub mod blk;
pub mod boot;
#[cfg(target_os = "none")]
pub mod console;
/// The process credential model: the four capability sets, the dumpable flag
/// and `KEEPCAPS`. Ungated and pure so the policy `capget`/`capset`, four
/// `prctl` options and `ptrace_may_access` read can be checked on the host -
/// the storage lives on the thread group in `sched`, and the two consumers
/// (`execve`'s re-derivation and the ptrace/`process_vm` gate) call in here.
pub mod cred;
/// The civil calendar: day counts to dates and back. Not target-gated - the
/// arithmetic is pure, and the `arch` half that writes it to hardware is
/// compiled where its tests are not, so the sums live here where a host
/// build can argue with them.
pub mod date;
#[cfg(target_os = "none")]
pub mod drivers;
/// The two DRM blobs a KMS driver publishes as bytes: an EDID, and the
/// `IN_FORMATS` list of which format-and-modifier pairs a plane scans out.
/// Ungated because both are pure encoders and the bytes are what a reader
/// parses - `drivers/drm.rs` holds the card's state and talks to the GPU, so
/// nothing inside it can be checked off the machine, and an EDID one field out
/// of place is a display no compositor can name.
pub mod drm_blob;
/// The DRM cursor's composition: an `ARGB8888` image blended over the `BGRA`
/// scanout at the pointer, in the one place a host test can reach it. The
/// driver next to it is never compiled off the machine, and this is the only
/// arithmetic on the presentation path - a 64x64 blend clipped at four edges,
/// where being wrong at one edge and right at three is the normal way it fails.
pub mod drm_cursor;
/// The DRM sync objects' state machine: what a `SYNCOBJ_*` request answers, in
/// the one place a host test can reach it. The driver next to it is never
/// compiled off the machine, and this is the family where the authorities
/// disagree - the upstream sources read a sync object with no fence as already
/// signaled and the real card refuses it (`PLAN.md` §7.4) - so the decisions are
/// written down here with a test per rule instead of being buried in a `match`
/// only a boot can exercise.
pub mod drm_sync;
/// The `DRM_VIRTGPU_*` family's answers on a card with no 3D behind it. Ungated
/// like its three siblings, and for the same reason: what a caller hears for
/// each request is a table of decisions about numbers, measured on a real
/// Linux, and a table is worth a host test rather than a boot. The requests that
/// need a resource to exist are absent here on purpose - their honest answer
/// needs the device, and the module says so.
pub mod drm_virtgpu;
/// The errno table: the numbers every layer answers "no" with. Sits at the
/// crate root precisely so that refusing in Linux's language never requires
/// naming the `syscall` layer from below it.
pub mod err;
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
/// holds rather than as a pair of machine-wide strings. Ungated because the
/// interesting half is *ownership* - a copy starts from the original's names
/// and then diverges - which is arithmetic over two `String`s and worth a host
/// test; the task table that decides whose namespace is whose is bare metal.
pub mod uts;
pub mod vfs;
/// The virtio-gpu protocol: the control commands and their shapes, as the wire
/// spells them. Not target-gated, and it has to be: the driver beside it is
/// never compiled off the machine, and what that driver and a real QEMU must
/// agree on is exactly the kind of fact - a command number, a field offset, a
/// struct size - that an agreement with itself cannot check. `drivers/drm.rs`
/// records what happens when a number is pinned only against the caller that
/// sends it.
pub mod virtgpu_abi;

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
