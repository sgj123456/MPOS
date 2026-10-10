//! The five steps with an ordering contract, beside the flow that calls them.
//!
//! [`super::flow`] is one sequence on every machine - the console service, the
//! device probe, and the kernel's own boot - so this module is not a set of
//! steps two machines pick from: it is the steps the one sequence runs, each of
//! them carrying an order that has to hold and that used to be written down once
//! per machine body (which is why they are functions rather than inline calls):
//!
//! 1. the scheduler adopts the running boot context as task 0, before anything
//!    spawns or blocks ([`start_scheduler`]);
//! 2. the standard file table goes over the tty table and a ramfs becomes `/`,
//!    before any check opens a path ([`mount_ramfs_root`]);
//! 3. the boot archive is unpacked into that root ([`unpack`]);
//! 4. the console mux goes up *before* the receiver is armed, so a keystroke
//!    cannot arrive with no session to belong to ([`arm_console`]);
//! 5. `/init` is loaded and lands on pid 1 - and something must be pid 1 or the
//!    boot has not handed the machine over at all ([`hand_to_init`]).
//!
//! Two more are here for the same reason rather than for an order of their own:
//! [`bring_up_net`] is the stack over whatever adapter the probe published, and
//! [`register_exit_hooks`] is the one list of what a dying process's descriptors
//! own. Both are written once because writing them per machine is how the two
//! came to claim different network setups and different cleanup for the same
//! descriptors (`NOTES.md` §13.4).
//!
//! What is *not* here any more is the kernel's own mounting of anything but the
//! bootstrap root: `/dev`, the host share and the disk were steps of this module
//! until the mount moved to whoever owns the tree it lands on, which is the early
//! userland (`assets/initramfs/core/init`, `NOTES.md` §15.6).
//!
//! # Why the steps are here and not in the library
//!
//! [`arm_console`] is half machine wiring: `vcmux::init` is the kernel's, but
//! arming the receiver is `crate::arch::enable_console_interrupts`, and the
//! kernel's own modules may not reach for the machine layer - `xtask depgraph`
//! enforces that `only main/lib may reach the machine layer directly`
//! (`ARCHITECTURE.md` §2, and the allow-list at `xtask/src/depgraph.rs`'s
//! `ARCH_EXEMPT`). It names the wiring as the thing that "starts it
//! (`arch::boot`), **arms its console interrupts** and reads its breakpoint
//! counter" - this is that arming.
//!
//! The library's `boot` module was the first home tried and it is the wrong one
//! for a second reason: the steps below need `crate::sched` and `crate::vfs`,
//! and `vfs` already names `boot` once (it reads the command line for
//! `/proc/cmdline`), so `boot` reaching back would close the graph's ninth mutual
//! pair. Putting them in the wiring crate - which sits above both, and is the
//! layer allowed to name the machine at all - adds no edge the ratchet can see
//! and no cycle (`NOTES.md` §14.9).

use alloc::string::String;
use alloc::vec;

use mpos::{arch, sched, vfs};

/// The scheduler adopts the running context - the boot stack - as task 0 and
/// installs its loop.
///
/// Must come before any spawn, and before anything that asks `sched::started()`:
/// the syscall gate answers differently before this call than after it, which is
/// what lets the machine layer's pre-scheduler probe take traps while `/init`'s
/// own calls route into the shared dispatch table.
pub fn start_scheduler() {
    sched::init();
    mpos::println!(
        "scheduler up: {} slots, running task {:?}",
        sched::MAX_TASKS,
        sched::current()
    );
}

/// The boot task's filesystem: the standard file table over the tty table, and a
/// ramfs as `/`.
///
/// The ramfs is the bootstrap root and the *only* tree this kernel establishes -
/// it is Linux's rootfs, and the real root arrives later, mounted and promoted by
/// the initramfs's own `/init` (`assets/initramfs/core/init`). What is fixed here
/// is the *order*: the tty table exists before the standard file table is built
/// over it (the console tty is what the table's entries are made from), and `/`
/// exists before anything resolves a path through it - the archive's unpack, and
/// the `/init` the machine is handed to.
pub fn mount_ramfs_root() {
    vfs::file::init_standard(vfs::tty::init());
    // The instance, not a borrow of it: the root slot has to be able to answer
    // which filesystem serves a node (`Vfs::mount_root` says what that costs).
    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(root);
}

/// Unpack the boot archive into the root.
///
/// The bytes come from the boot record, which is the loader's business; the tree
/// they land in is the ramfs above. A boot with no archive is a boot whose whole
/// userland is on the disk its own init mounts, and the flow says so rather than
/// calling this.
pub fn unpack(archive: &[u8]) {
    let members =
        vfs::tar::unpack_into(vfs::global(), archive).expect("the boot archive was broken");
    mpos::println!("initramfs: {members} members unpacked");
}

/// Arm the console's own interrupts and give the wire to the console mux.
///
/// The order *inside* this function is the contract, and it is the reason it is
/// one function rather than two calls at two call sites: the mux goes up first,
/// so that from the moment the receiver can deliver a byte the byte belongs to
/// the *active virtual console* rather than to the console tty. A receiver armed
/// before the mux delivers its first keystroke to a tty whose session is not the
/// one the machine is showing (`NOTES.md` §13.16).
///
/// It is called from the one flow, before the handoff and before the stack: from
/// here the console stops polling its receiver and starts queueing into a ring an
/// interrupt drains, so the writer has to be queueing before a program on the
/// other end of the wire reads anything, and the receiver's bytes have to have a
/// session to belong to.
pub fn arm_console() {
    vfs::vcmux::init();
    arch::enable_console_interrupts();
}

/// Hand the machine's network adapter to the one stack, and start its service.
///
/// The device is the one the probe published (`drivers::virtio::probe`), whichever
/// transport it answered on, and everything above the device is one copy: the same
/// `net::init` builds eth0 and smoltcp's loopback over whatever `NetDevice` it is
/// handed, and the same `spawn_service` starts the task that keeps a connection
/// moving while its owner is blocked. The line below is the *stack's*
/// configuration rather than a machine's, because QEMU's user-mode network answers
/// the same address on either.
///
/// The device is passed in rather than looked up here: a caller with no adapter
/// says so in its own line and the socket calls answer `ENETDOWN`.
pub fn bring_up_net(dev: alloc::sync::Arc<dyn mpos::drivers::virtio::NetDevice>) {
    let mac = dev.mac();
    mpos::net::init(dev);
    mpos::net::spawn_service();
    mpos::println!(
        "net: virtio-net up at 10.0.2.15/24, gateway 10.0.2.2, MAC \
         {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
}

/// Tell the scheduler's exit path what a dying process's descriptors own.
///
/// One list for both machines, in the order the resources have to be released
/// in, because the registry runs hooks in registration order: record locks first
/// (they are keyed by `(device, inode)` and the inode is reached through a
/// descriptor), then the state keyed by the open file description - the socket,
/// the inotify watch, the evdev reader position, and the uinput device a program
/// made for itself. Each hook runs before the batch is dropped, while the
/// reference counts still say which description is really the last one.
///
/// The socket hook is the one whose absence has a measurement on the *second*
/// machine, and it is why this list is a shared step rather than a machine's
/// courtesy: with the stack live and the hook unregistered, `t-tcp`'s bind of
/// 34567 answered `EADDRINUSE` on a machine that had just booted, because a
/// socket a dying process left in the stack's set keeps its local port reserved
/// for ever (`NOTES.md` §13.4). A machine whose adapter never probed still has
/// `AF_UNIX` and `AF_VSOCK` sockets, so the hook belongs to the descriptor and
/// not to the device.
///
/// The `FD_CLOEXEC` registry is the `execve` counterpart, and it is deliberately
/// shorter: only the two whose reader position is keyed by an address the fresh
/// image reuses - the evdev node and the uinput device - are told when the
/// process *lives on*, while locks, the socket and the inotify watch have their
/// own close-time handling and stay exit-only. Same hooks, registered into the
/// second list, so `sched` never names `drivers` for the cloexec pass.
///
/// The two input hooks are gated on the capability that describes them
/// (`mpos_input_registry`: "this machine has an evdev registry - and therefore
/// somewhere for a created uinput device to land"), rather than on an
/// architecture, so a port that grows the input stack gets them by declaring the
/// capability and nothing here moves. The other three are every machine's: locks
/// and watch state are keyed by the open file description, and the socket hook's
/// absence has the measurement above.
pub fn register_exit_hooks() {
    sched::exit::register(mpos::vfs::locks::release_on_process_exit);
    sched::exit::register(mpos::net::socket::close_sockets_at_exit);
    sched::exit::register(mpos::vfs::inotify::close_at_exit);
    #[cfg(mpos_input_registry)]
    {
        sched::exit::register(mpos::drivers::virtio::input::close_at_exit);
        sched::exit::register(mpos::drivers::uinput::close_at_exit);
        sched::exit::register_cloexec(mpos::drivers::virtio::input::close_at_exit);
        sched::exit::register_cloexec(mpos::drivers::uinput::close_at_exit);
    }
}

/// Load `/init` and put it on pid 1.
///
/// The path is read through the VFS, so it is whatever `/` resolves to when this
/// is called: the bootstrap ramfs on a machine that unpacks, the disk on a
/// machine that has switched root. What the two machines agree on - and what
/// this asserts rather than trusts - is that the first program the scheduler
/// starts *is* pid 1. A boot that put the machine's init anywhere else has not
/// handed the machine over, and the failure belongs here rather than as a
/// subtle wrongness discovered by the first `getpid` a program makes.
///
/// Returns the pid (always [`sched::INIT_PID`]) so the caller can wait on it -
/// which is where the caller's own policy starts, because what "init is up"
/// means is the machine's answer: x86_64's init forks a shell and the boot waits
/// for a child to exist, aarch64's runs a program that exits and the boot waits
/// for it to leave.
pub fn hand_to_init() -> sched::TaskId {
    let image = vfs::read_file("/init").expect("the root carries /init");
    let argv = vec![String::from("/init")];
    let pid = sched::spawn_init(&image, &argv, &[]).expect("the init image loaded");
    assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1");
    pid
}
