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
//!
//! # Two machines, and which stages have un-gated what
//!
//! The gate above is not only "host or machine" any more. `target_os = "none"`
//! is true for aarch64 as well as x86_64, so every module the second machine
//! cannot yet compile needs `target_arch = "x86_64"` spelled out too - and the
//! compiler will not say so until someone builds for aarch64, which is exactly
//! why the list is written down here rather than discovered (`PLAN.md` §9.1).
//!
//! A port is not one diff: the machine layer is stage 1, and the subsystems
//! above it un-gate one stage at a time, each landing with the trait or the
//! door that lets it compile. The ledger, as it stands:
//!
//! - **compiled for every target the build has** - `arch` (both bodies),
//!   `boot`, `mm` (its pure half and the heap; the *dynamic* MMIO window -
//!   `mm::map_mmio` - is x86_64's until stage 6, because it goes through
//!   `hal::Paging`; the fixed window aarch64's devices are reached through is
//!   `arch::aarch64::paging::DEVICE_VIRT`, and it is not this module's), `logging` (the level vocabulary; the subscriber writes to the console, now present on both machines), `rand`, `sync`, `uevent`, and every `crates/*`
//!   leaf. `hal` joined them at stage 6f (both machines implement the baseline
//!   concerns; x86's optional `Msi`/`SerialRing`/`FpuTrap`/`Segments` are gated),
//!   and `console` at stage 4a (the serial sink is reached through `hal::Console`,
//!   and the framebuffer + interrupt-TX-ring + scheduler-parking arms are
//!   `#[cfg]`-inert on aarch64, whose console is the polled PL011).
//! - **not on aarch64 yet**, and the two ways a module can be that:
//!   - *machine-only modules that were already `target_os = "none"`* move to
//!     `all(target_os = "none", target_arch = "x86_64")`. `drivers` (stage 5) and
//!     `net` (stage 6) are these.
//!   - *host-compiled modules* - `sched` (3), `vfs` (4), `syscall` (6) and
//!     `uaccess` (6) - keep the host and x86_64 bare metal and exclude only the
//!     aarch64 build, `any(not(target_os = "none"), target_arch = "x86_64")`.
//!     They are the ones the host unit suite pins, and gating them to
//!     `target_arch = "x86_64"` would silently delete that coverage: the module
//!     would compile, the tests just would not exist.
//!
//! The stage number in brackets is where each un-gates; nothing above claims a
//! green it does not have - a module listed as x86_64-only is one the aarch64
//! build does not compile *yet*, not one it compiles differently.

#![no_std]
#![cfg_attr(
    all(target_os = "none", target_arch = "x86_64"),
    feature(abi_x86_interrupt)
)]

extern crate alloc;

/// The arch-neutral Linux ABI facts: the `clone` flag bits, the default umask
/// and the resource-limit model. Its own leaf crate so `sched` reads them on a
/// downward edge, not up into `syscall`; `syscall::linux::{nr,abi}` re-export
/// these instead of redefining them, so there is one table.
pub use mpos_abi as abi;
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
/// The block layer seam: the `BlockDev` trait a filesystem reads its storage
/// through, the registry that names every disk the kernel has, and the GPT/MBR
/// decode that finds a filesystem on one. Its own leaf crate - the trait and the
/// parser are pure and host-tested, so the crate boundary is what makes the
/// downward-only rule (AGENTS.md §4 rule 5) a compile error for them rather than
/// a convention.
pub use mpos_blk as blk;
pub mod boot;
/// The log ring, serial, framebuffer switch and the six virtual consoles.
/// Now compiled for both machines (stage 4a): the sink is reached through
/// `crate::hal::Console` - the x86_64 16550/`SerialRing` and framebuffer paths and
/// the scheduler-parking arm are `#[cfg]`-inert on aarch64, whose console is the
/// polled PL011. A boot-observed aarch64 console marker waits on relocating the
/// post-machine checks into `main` (`PLAN.md` §9.1's stage 4b gap).
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
/// Device drivers. The module is compiled for both machines now, but almost
/// every submodule is x86_64's (port I/O, `uart_16550`, PCI, AHCI, DRM): they
/// carry their own `#[cfg(target_arch = "x86_64")]` gates inside
/// `src/drivers/mod.rs`, so on aarch64 only the machine-neutral virtio `Hal`
/// and the new MMIO transport compile. This is `PLAN.md` §9.1's drivers-stage
/// prep - the same shape stage 6A used for `syscall`.
#[cfg(all(
    target_os = "none",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub mod drivers;
/// The errno table: the numbers every layer answers "no" with. Its own leaf
/// crate, re-exported here so `crate::err` keeps resolving for every caller,
/// while the crate itself cannot name the kernel and so cannot reach back up.
pub use mpos_err as err;
/// The hardware abstraction layer: the capability traits a machine implements
/// and the forwards everything above asks it through (`src/hal/mod.rs`). Machine
/// only, like `arch` itself - it exists so that nothing outside `arch`/`hal`
/// names an architecture.
///
/// x86_64-only for now, and by construction rather than by choice: the forwards
/// at the bottom of `hal/mod.rs` name all six traits, so the module can only be
/// compiled for a machine that implements all six. The aarch64 impls land with
/// their stages - `Cpu`/`Interrupts` at 1c, `Contexts` at 2 (and its `UserFrame`
/// at 6a), and `Paging` split: 1f gave it the table-building half (`install`,
/// `translate`), 6a a user space that can be built, mapped, switched to and
/// walked, and 6b that space's *operations* - `unmap`, `protect`,
/// `walk_user_pages`, the copy-on-write half and the `Drop` built out of them,
/// written to `AddressSpaceOps`' shapes so that the impl is delegation when
/// `hal` reaches this machine. Both machines now compile it: x86_64 implements the
/// six baseline concerns plus the three optional capabilities its hardware has
/// (`Msi`/`FpuTrap`/`Segments`), and aarch64 - which has no MSI controller, no lazy
/// FPU trap and no segment selectors - implements only the six, which is what the
/// split into optional capability traits exists to allow. The aarch64 impls landed
/// with the stages that built their bodies: the `Paging` running-process methods and
/// the PL031 `WallClock` (`PLAN.md` §9.1).
#[cfg(target_os = "none")]
pub mod hal;
/// evdev's record window and its clients: which reader sees which event, and
/// what a grab does to the answer. Its own leaf crate - the driver that fills
/// the window needs a virtio ring and a clock, but the policy over it is
/// arithmetic, and arithmetic is worth a host test. Its waiter numbers are
/// opaque there (`evdev::WaiterId`); the driver that can name a task converts.
pub use mpos_evdev as evdev;
/// The device tree reader: the flattened blob a firmware hands over on a
/// machine that describes itself, walked for the nodes and properties a driver
/// asks about. Its own leaf crate, because every question it answers is
/// arithmetic on a `&[u8]` and arithmetic is worth a host test; the machine
/// half - which compatible string means the serial port - stays in `arch`.
pub use mpos_fdt as fdt;
/// The GPU's host-testable logic: the DRM blobs, cursor composition,
/// sync-object state machine, the `DRM_VIRTGPU_*` answers, and the virtio-gpu
/// wire ABI. Each is pure and tested on the host; the machine bodies stay in
/// `drivers`. Its own leaf crate now - the driver that reads it is machine-only,
/// so nothing checked from *there* can be checked at all, and the one thing that
/// was not pure (who a sync object wakes) arrives as an injected
/// `gpu::sync::Waker`.
pub use mpos_gpu as gpu;
/// Kernel logging: `tracing` events, formatted onto the serial console.
/// Ungated so the level names and the command-line parsing stay testable on
/// the host; only the subscriber that writes to COM1 is machine-specific.
pub mod logging;
pub mod mm;
/// The network stack, and the socket syscalls over it. Host builds skip it:
/// its tests are the conformance programs that run on the machine.
///
/// Ungated on both machines since §13.4's device seam landed and the second one
/// got an adapter. `net` holds `Arc<dyn drivers::virtio::NetDevice>` and names no
/// transport; the machine that owns the card opens it - `main/x86_64.rs` over the
/// PCI bus, `main/aarch64.rs` over the MMIO window its `check_net` found and
/// published - and hands it to `net::init`, so the stack above the seam is the
/// same code on both and a machine with no card hands over nothing (the honest
/// `ENETDOWN`, not a device that pretends to answer). The one reference still
/// x86_64's is `collect_net_sockets`, the four `/proc/net` rows, gated with
/// procfs's *own* condition: its only caller is procfs, which the second machine
/// has not been given yet (`PLAN.md` §13's port-progress ledger).
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
/// The task table, priority bands, sleep/block, signals and futex. Un-gated on
/// aarch64 by stage 3, which is when its one machine reference - the TLS `MSR`
/// write in `sched/runtime.rs` - moves behind `hal`/`arch`.
///
/// The predicate is "the host, or x86_64 bare metal", not "x86_64" alone:
/// `sched` is host-compiled (it has no port I/O of its own - its machine
/// references sit behind `target_os = "none"` gates inside it), and the host
/// unit suite is the pin for its policy. Gating it to `target_arch = "x86_64"`
/// would drop that host coverage, which is exactly the failure this port's
/// stage list has to avoid. aarch64 joins at stage 5: its x86-only machine
/// references - the lazy-FPU trap and the ptrace/stop register snapshot - are
/// `#[cfg]`-gated inside (that machine has neither the `#NM` trap nor ptrace
/// yet), so the scheduler's arch-neutral policy compiles and runs where the
/// aarch64 `svc` gate and signal-frame ABI (stage 6/7) will drive them.
#[cfg(any(
    not(target_os = "none"),
    target_arch = "x86_64",
    target_arch = "aarch64"
))]
pub mod sched;
/// `SECCOMP_MODE_FILTER`'s program: the classic-BPF interpreter, its validation
/// and the action ordering. Its own leaf crate (nothing in it knows what a task
/// is); the task table and the syscall gate decide who has one and when it runs.
pub use mpos_seccomp as seccomp;
pub mod sync;
/// The Linux ABI: numbers, `abi.rs` facts, dispatch and handlers. x86_64's
/// register file and numbering until stage 6, which is where the aarch64
/// numbers (`openat` is 56, not 257) and the `svc #0` gate land.
///
/// Host-compiled like `sched`, so the exclusion is the bare-metal aarch64 build
/// and nothing else - the ABI facts and the errno mappings it carries are
/// pinned by host tests.
#[cfg(any(
    not(target_os = "none"),
    target_arch = "x86_64",
    target_arch = "aarch64"
))]
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
///
/// x86_64's until stage 6: both halves are the ABI's - `err_of` names `VfsError`
/// and the pointer check asks the page tables - and both arrive with the rest of
/// the aarch64 ABI. Host-compiled, though, because `vfs`'s own host tests call
/// the pointer bound with `from_user = false`.
#[cfg(any(
    not(target_os = "none"),
    target_arch = "x86_64",
    target_arch = "aarch64"
))]
pub mod uaccess;
/// Device events: what the kernel tells userland when a device
/// appears, and the netlink socket they travel over. Not target-gated:
/// nothing here needs hardware, and the wire format is worth testing
/// on the host.
pub mod uevent;
/// uinput's interface: the requests and bitmaps a userspace program declares an
/// input device with. Its own leaf crate, and it depends on the evdev crate for
/// one constant - the size of a `struct input_event`, the unit both families
/// read and write. The device that uses it is bare metal.
pub use mpos_uinput as uinput;
/// The UTS namespace: the two names `uname(2)` reports, as the object a task
/// holds rather than as a pair of machine-wide strings. Its own leaf crate
/// (*ownership* over two `String`s); the task table that decides whose
/// namespace is whose is bare metal.
pub use mpos_uts as uts;
/// The filesystem layer: mounts, the fd table, every filesystem, pipes and the
/// device doors. Un-gated on aarch64 by stage 4; until then it names `drivers`
/// through `procfs`/`sysfs`/`p9`, which stage 5 brings to this machine.
///
/// Host-compiled - the ramfs, the ext4 cache and the fd table are the largest
/// block of host-pinned logic in the kernel - so only the bare-metal aarch64
/// build is excluded here.
#[cfg(any(
    not(target_os = "none"),
    target_arch = "x86_64",
    target_arch = "aarch64"
))]
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
