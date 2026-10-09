//! The steps every port's `init` runs, and the ordering contracts in them.
//!
//! Each machine's boot body (`board/x86_64.rs`, `board/aarch64.rs`) ends in an
//! `init` that hands the machine to `/init`, and those two `init`s are *not* one
//! function with a `#[cfg]` in it - they are two different sequences. Where they
//! differ is real: one enumerates a PCI bus and names `virtio-blk`, `ahci`, a 9p
//! share and a DRM card, the other opens virtio-MMIO windows and switches root
//! from the kernel; one runs the M0..M8 checks, the other a different set, in an
//! order its own milestones taught it. Neither machine should have to accept the
//! other's answer, and `mpos-arch` can name none of it.
//!
//! What they do *not* differ in is five steps, and every one of them carries an
//! ordering contract that was, before this module, written down twice:
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
//!
//! Where each step goes is *not* the same on both machines, and that is why this
//! is a set of steps rather than one `run()`: x86_64 arms the console after the
//! spawn (nothing reads the wire until the shell is up) while aarch64 arms it
//! before its own `check_serial_tx`, which is a check *of* the armed path;
//! x86_64 unpacks inside its filesystem bring-up while aarch64 unpacks after its
//! devices and switches root after that. Fusing those into one function would
//! mean handing it a list of hooks to reorder - the two sequences, wearing one
//! hat.

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
/// The ramfs is the bootstrap root, not the root the system runs from - a disk
/// root arrives later, either switched to by a machine that does it in the
/// kernel or mounted by the initramfs's own `/init` script. What is fixed here is
/// the *order*: the tty table exists before the standard file table is built over
/// it (the console tty is what the table's entries are made from), and `/` exists
/// before any check resolves a path through it.
pub fn mount_ramfs_root() {
    vfs::file::init_standard(vfs::tty::init());
    // The instance, not a borrow of it: the root slot has to be able to answer
    // which filesystem serves a node (`Vfs::mount_root` says what that costs).
    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(root);
}

/// Unpack the boot archive into the root.
///
/// Shared because the bytes are the same bytes and the tree they land in is the
/// same tree - only where the machine got them differs, and that is its own
/// problem (`vfs::tar` is handed a slice either way).
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
/// Where a machine calls it is its own business: from here the console stops
/// polling its receiver and starts queueing into a ring the interrupt drains, so
/// a machine whose checks *of* that path run late arms it late, and one whose
/// checks want it armed arms it early.
pub fn arm_console() {
    vfs::vcmux::init();
    arch::enable_console_interrupts();
}

/// Hand the machine's network adapter to the one stack, and start its service.
///
/// Both machines find their adapter their own way - x86_64 enumerates the PCI bus
/// for it, aarch64 picks an MMIO window out of its device tree - and both end
/// here, because everything above the device is one copy already: the same
/// `net::init` builds eth0 and smoltcp's loopback over whatever `NetDevice` it is
/// handed, and the same `spawn_service` starts the task that keeps a connection
/// moving while its owner is blocked. Calling them from two bodies is how the two
/// machines would come to claim different network setups for the same stack, so
/// they are called from here instead - and the line below is the *stack's*
/// configuration rather than a machine's, because QEMU's user-mode network
/// answers the same address on either.
///
/// The device is passed in rather than looked up here: which bus carries it is the
/// machine's fact, and a machine with no adapter at all says so itself (its
/// absence is a line, not a failure - the socket calls answer `ENETDOWN`).
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

/// Mount the host's 9p share at `/mnt/share`, if the machine found one.
///
/// Both machines find their device their own way - the first enumerates the PCI
/// bus, the second picks an MMIO window out of its device tree - and both record
/// it in the one `fs9p` registry, so the mount itself is the *filesystem's* and
/// not a machine's: the same `p9::mount`, the same `/mnt/share`, whichever bus
/// answered. A machine with no share - or one whose tag the host never exported -
/// skips the step with a line, which is a quieter boot rather than a failed one.
///
/// The step was the first machine's private function until the second machine
/// grew the device (`NOTES.md` §13.29), and moving it here is what made the second
/// machine's share possible at all: it was the *probe* that was missing there,
/// not the mount, and a mount written twice would have hidden that.
pub fn mount_share() {
    let Some(dev) = mpos::drivers::virtio::fs9p::devices().into_iter().next() else {
        mpos::println!("virtio-9p: no share on the bus");
        return;
    };
    let tag = dev.mount_tag();
    let fs = match mpos::vfs::p9::mount(tag) {
        Ok(fs) => fs,
        Err(e) => {
            mpos::println!("virtio-9p: \"{tag}\" refused to open: {e:?}");
            return;
        }
    };
    let root = vfs::resolve("/").expect("no root filesystem");
    // `/mnt` is made if the boot has not already: *which* root is active when the
    // share is mounted differs by machine (the first mounts it on the bootstrap
    // ramfs during device bring-up, the second on the disk it has just switched
    // to), so the directory is created here rather than assumed - a mountpoint
    // one machine happened to make is not a contract.
    if root.lookup("mnt").is_err()
        && let Err(e) = root.create("mnt", mpos::vfs::NodeKind::Dir)
    {
        mpos::println!("virtio-9p: /mnt could not be made: {e:?}");
        return;
    }
    let mnt = match root.lookup("mnt") {
        Ok(inode) => inode,
        Err(e) => {
            mpos::println!("virtio-9p: /mnt does not resolve: {e:?}");
            return;
        }
    };
    if mnt.lookup("share").is_err()
        && let Err(e) = mnt.create("share", mpos::vfs::NodeKind::Dir)
    {
        mpos::println!("virtio-9p: /mnt/share could not be made: {e:?}");
        return;
    }
    // The share is named by its *tag* rather than left as "none": the tag is
    // what a second mount of the same export needs, and the second machine's
    // init reads it out of `/proc/mounts` to serve the share inside the root it
    // is about to switch to (`initramfs-aarch64/init`, and the payload's
    // `t-9p` reads the boot's share through that name).
    //
    // The filesystem is held by `Arc` - its inodes hold it by `Weak`, so a
    // share that outlives its mount is answered rather than crashed - and the
    // mount API takes that same instance, so the record keeps the share alive
    // for as long as the name does and can answer which filesystem serves it.
    match mpos::vfs::mount::mount_at("/mnt/share", fs.clone(), "9p", tag, None) {
        Ok(()) => mpos::println!("virtio-9p: \"{tag}\" mounted at /mnt/share"),
        Err(e) => mpos::println!("virtio-9p: \"{tag}\" did not mount: {e:?}"),
    }
}

/// Mount the boot disk at `/mnt`, if the machine registered one.
///
/// The kernel's own mount of its disk - the half every machine that boots from
/// a block device needs *before* there is a userspace to mount it: the self-test
/// payload reads seed files through this tree, and on the second machine the
/// disk root is what the initramfs script switches into. What is deliberately
/// not here is any root switch: pivoting `/` onto this mount is the early
/// userland's job on both machines (`NOTES.md` §15.6), because a kernel that
/// assembles the real root first leaves pieces the owner of the system cannot
/// unmount.
///
/// The mount carries the *name and number* the devfs publishes for the same
/// disk (`/dev/vda`, major 254 minor 0 for a whole virtio disk), so a later
/// `mount(2)` of that node by the initramfs script re-reads the filesystem over
/// the same instance rather than opening the device twice - and the root the
/// script promotes keeps the device number Linux reports as `/`'s `st_dev`.
pub fn mount_disk_root() {
    let Some(disk) = mpos::blk::disks().into_iter().next() else {
        mpos::println!("disk: no device was registered with the block layer");
        return;
    };
    // The root is a *partition*, and which one is the partition table's answer
    // rather than this function's: `find_filesystem` finds the one ext2/ext4
    // partition on the disk, and hands the whole device back for a disk that
    // carries a filesystem with no table around it. So a bare image and a real
    // GPT disk mount through the same line, and neither machine's bring-up has
    // to know which shape its boot medium is (`NOTES.md` §15.18).
    //
    // Not `find_rootfs`: that one names partition 3, which is where the root
    // sits in a three-partition layout (ESP, boot, rootfs). A two-partition
    // disk - ESP and root, which is what the second machine boots - has its
    // root in partition 2, and asking for a third would find nothing.
    let Some(partition) = mpos::blk::part::find_filesystem(disk.dev.clone()) else {
        mpos::println!("disk: {} carries no ext2/ext4 partition", disk.name);
        return;
    };
    let fs = match mpos::vfs::ext4::Ext4Fs::mount(partition.clone()) {
        Ok(fs) => fs,
        Err(e) => {
            mpos::println!("disk: {} did not mount as ext4: {e:?}", disk.name);
            return;
        }
    };
    let fs = alloc::sync::Arc::new(fs) as alloc::sync::Arc<dyn mpos::vfs::FileSystem>;
    let dev = mpos::uevent::makedev(disk.major, 0);
    // `/mnt` is made if the boot has not already - the same rule `mount_share`
    // states for the directory under its mountpoint. The ramfs root answers
    // `lookup`/`create` directly; nothing resolves through the mount table yet,
    // which is exactly why the name is taken from the root inode rather than
    // walked.
    let root = match vfs::resolve("/") {
        Ok(inode) => inode,
        Err(e) => {
            mpos::println!("disk: the bootstrap root does not resolve: {e:?}");
            return;
        }
    };
    if root.lookup("mnt").is_err()
        && let Err(e) = root.create("mnt", mpos::vfs::NodeKind::Dir)
    {
        mpos::println!("disk: /mnt could not be made: {e:?}");
        return;
    }
    // Attached through the VFS's own path-taking form, so the record names the
    // mount `/mnt`, types it `ext4` and sources it `/dev/vda` - the three words
    // `/proc/mounts` and `statfs` answer with, and the reason the script's
    // later `mount --move /mnt /sysroot` moves *this* mount rather than failing
    // to find one.
    match mpos::vfs::mount::mount_at(
        "/mnt",
        fs,
        "ext4",
        &alloc::format!("/dev/{}", disk.name),
        Some(dev),
    ) {
        Ok(()) => mpos::println!("disk: /dev/{} mounted at /mnt", disk.name),
        Err(e) => mpos::println!("disk: /mnt did not mount: {e:?}"),
    }
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
