//! The kernel's boot flow: the stages every machine runs, written once.
//!
//! `src/main.rs` enters the kernel and hands the loader's record to
//! [`super::start`], which gives it to the machine layer's own entry
//! (`mpos::arch::boot`, whose one `#[cfg]` seam picks this build's port) together
//! with these three stages. So what a machine contributes to the *kernel's* boot
//! is not a second `start` and not a second sequence: it is what the machine layer
//! already owns - the entry, the descriptor tables, the page tables, the wire and
//! the description of its devices - and everything above that is here.
//!
//! Three stages, and the split is the boot's own shape rather than a machine's:
//!
//! * [`console_up`] - the console service comes up, and the command line's
//!   `console=` list decides where the log lands from here on;
//! * [`devices`] - the bus is enumerated and every device class the machine has is
//!   found, opened and registered with the subsystem that owns it
//!   (`drivers::virtio::probe`), which is one pass on both machines because the
//!   question is asked of every transport rather than of a preferred one;
//! * [`init`] - the kernel's own boot: the bootstrap root, the archive, the
//!   display's task, the wire, the stack over the adapter, the seed, and the
//!   handoff to `/init`. Never returns.
//!
//! # What the kernel mounts, and what it does not
//!
//! One thing: the ramfs the archive is unpacked into, which *is* Linux's rootfs -
//! the kernel's own bootstrap root, and the only tree the kernel owns. Everything
//! else is the early userland's, because only its owner can unmount it again:
//! `/proc`, `/sys`, `/dev` and the real root are mounted by the initramfs's own
//! `/init`, and so is the host share (`assets/initramfs/core/init`,
//! `docs/NOTES.md` §15.6). A kernel that mounts a device tree of its own leaves
//! the owner of the system with a mount it cannot find in its own table - the
//! failure measured when the kernel built `/dev` before the block layer existed,
//! and the reason the two machines had drifted apart here: one built `/dev` and
//! mounted the share and the disk in the kernel, the other left all three to the
//! script. Now both leave them to the script, which is what a distribution's
//! early userland is for.
//!
//! # Why the flow lives in the wiring layer
//!
//! Every line below names `console`/`drivers`/`vfs`, which sit above the machine
//! layer, so `mpos-arch` cannot hold this module at all (`AGENTS.md` §3,
//! downward-only). The wiring layer sits above both and is the layer allowed to
//! name the machine (`xtask/src/depgraph.rs`'s `ARCH_EXEMPT`), which is why the
//! five ordered steps beside it (`src/board/steps.rs`) are here too.

use alloc::string::String;
use alloc::vec::Vec;

use mpos::boot::BootInfo;
use mpos::boot::console_list::ConsoleList;
use mpos::sched;
// The console line the handoff queues for the early userland's own read, which a
// test build's boot is the only one that owes (see the handoff below).
#[cfg(feature = "selftest")]
use mpos::vfs;

#[cfg(feature = "selftest")]
use super::kunit;
use super::report;
use super::steps;

/// The boot word that arms §10.5's disk-counter protocol (§10.5, `abi`'s
/// `DISKSTAT_OSRELEASE_MAGIC` carries the other half of the pair). Named here
/// rather than spelled at its one use so the launch side (`xtask`'s sweep) and
/// this one have a single string to agree on - it is a contract between two
/// readers of the same command line.
pub const DISKSTAT_WORD: &str = "mpos.diskstat";

/// Bring the console service up, and say what was booted.
///
/// The one step between "the machine layer's own writer carries every line" and
/// "`mpos::println!` does", and the reason it is one function rather than a call
/// in each machine body: the two bodies had already drifted apart here. The first
/// read the `console=` list off the boot command line and the second did not,
/// although the second's command line carries `console=ttyAMA0` and
/// [`ConsoleList`]'s host tests already name that word as this machine's own
/// spelling of the wire - so the second machine's log followed a default list
/// instead of the list it was booted with. Which consoles a boot writes to is a
/// fact about the *launch*, not about the machine, and it is read from the same
/// record on both.
///
/// The list is applied before the banner, because the banner is the first thing
/// that must land on the consoles the launch named rather than on the wire the
/// machine happened to bring up first.
pub fn console_up(info: &BootInfo) {
    // Bring the service's sink up - idempotent over the wire the machine layer
    // already poked - and point it at the consoles this boot named.
    mpos::console::init();
    mpos::console::set_console_list(ConsoleList::from_cmdline(info.cmdline, |unknown| {
        mpos::println!("console={unknown}: no such console here, ignored");
    }));
    report::banner(info);
}

/// Walk the machine's PCI bus, answering how many functions answered.
///
/// Both machines enumerate it before their own transports, and for the same
/// reason: a PCI device is the one device no firmware description carries on
/// either machine, so the bus is asked directly and what it answers is published
/// for `/sys` and `/proc` to describe the machine from. Which registers the walk
/// goes through - port I/O or a memory-mapped ECAM window - is the machine's own
/// answer (`drivers::pci`), and the tree it builds is one model on both.
pub fn enumerate_bus(info: &BootInfo) -> usize {
    let count = mpos::drivers::pci::init(info);
    mpos::println!("pci: {count} functions enumerated");
    count
}

/// Find and register this machine's devices.
///
/// Three steps and no mounting. The bus is enumerated because a PCI device is the
/// one device no firmware description carries; every virtio class is then asked for
/// over whichever transport answers (`drivers::virtio::probe`), which registers
/// each device with the subsystem that owns it; and the SATA controller behind the
/// bus is brought up the same way - all of it the machine's *devices*, none of it a
/// tree the kernel assembles.
pub fn devices(info: &BootInfo) {
    enumerate_bus(info);
    mpos::drivers::virtio::probe::all(info);
    attach_sata();
}

/// The SATA disk, if the machine's bus carried a controller.
///
/// A second disk and not a second boot path: the boot disk is the virtio block
/// device `probe` registers as `vda`, and what a SATA controller adds is a device
/// under the name the SCSI tree gives it (`sda`). The name, the major and the tree
/// position are the *device's* facts rather than the transport's - the same ones
/// the other machine's disk gets - which is why they are said here, once, instead
/// of at each machine's bring-up.
fn attach_sata() {
    let Some(dev) = mpos::drivers::ahci::init() else {
        return;
    };
    let sectors = dev.capacity_sectors();
    mpos::println!(
        "ahci: sda, {} sectors ({} MiB)",
        sectors,
        sectors * 512 / (1024 * 1024)
    );
    mpos::blk::add_disk(
        "sda",
        mpos::uevent::SCSI_DISK_MAJOR,
        "/devices/ahci/block",
        dev,
    );
}

/// The kernel's own boot: prove the kernel works, then hand the machine to
/// `/init` and stand aside. It ends inside [`hand_to_init`], which parks the boot
/// task rather than returning - a system that stops running itself is not a system.
///
/// Handed to the machine layer as its `init` callback, so it runs on the kernel's
/// own page tables, with the record already turned into `BootInfo`, the console up
/// and the machine's devices registered.
pub fn init(info: &BootInfo) -> ! {
    // Logging comes up as soon as the heap does, because the subscriber it
    // installs allocates and the level it prints at is read from the command
    // line. Everything after this point is worth being able to turn on or off.
    mpos::logging::init();

    // §10.5's disk-counter protocol, armed here and nowhere else: a boot
    // carrying the word below spends its whole life answering `uname`'s
    // `release` with the magic word (`abi::DISKSTAT_OSRELEASE_MAGIC`) and its
    // `version` with the live per-disk counters, which is what makes
    // `t-diskbench` a measurement rather than a guess. This is not a *test*
    // opening: it does not decide whether the kernel runs checks - that stays
    // the build's to say (§15.19, and `xtask`'s own gate still refuses
    // `mpos.selftest` by name) - it renames two ABI fields for one launch, and
    // only `cargo xtask test` ever puts the word on a line; the session image
    // never carries it, and nothing a program can do turns it on. The flag
    // rides the boot task's UTS namespace, so every task inherits it and a
    // `unshare(CLONE_NEWUTS)` copy keeps reporting real numbers - the payload
    // is rendered from the live counters at read time.
    if info.cmdline.split_whitespace().any(|w| w == DISKSTAT_WORD) {
        sched::arm_diskstat_boot_namespace();
        mpos::println!("diskstat: uname release/version carry the per-disk io counters");
    }

    // The heap, used once: an allocation that is written, read back and dropped
    // is the cheapest proof that the allocator the whole kernel now depends on
    // actually hands out memory. The numbers beside it are what a boot that is
    // short of memory shows first.
    init_mm();
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Heap, info);

    // The machine's interrupts, descriptor tables and page tables are already up -
    // `mpos::arch::boot` installed them before calling here, on both machines - so
    // what asks about them is the machine layer's own evidence, a test build's
    // cases beside this file (`src/board/suites.rs`).
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Arch, info);

    steps::start_scheduler();
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Scheduler, info);

    // The bootstrap root: a ramfs, which is the whole of what this kernel mounts.
    // The archive the loader handed over is unpacked into it, and from then on
    // `/init` is the program that owns the machine's trees - the pseudo-
    // filesystems, the real root and the host share included.
    steps::mount_ramfs_root();
    match info.initramfs {
        Some(archive) => steps::unpack(archive),
        // A kernel with no archive and no root of its own has nothing to hand a
        // machine to. This kernel mounts no disk root and no device tree - the
        // early userland owns both - so a boot that carries neither an archive nor
        // a `/init` inside one has no userland at all, and stopping with the reason
        // is the honest answer rather than a panic from the handoff's path walk.
        None => {
            mpos::println!(
                "root: the boot carries no initramfs, and this kernel mounts no root of its own"
            );
            mpos::hal::halt()
        }
    }
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Vfs, info);

    // The display, and the one device whose *owner* is a task: it is opened here,
    // where there is a scheduler to start the render task on, rather than with the
    // probe's other classes. The card's own node (`/dev/fb0`) is published by the
    // device; what the task does with the screen follows the console list, and a
    // session may claim the card for itself.
    if let Some((device, view)) = mpos::drivers::virtio::probe::gpu_card(info) {
        let (width, height) = view.dims();
        mpos::println!(
            "virtio-gpu: {width}x{height}, {} KiB at /dev/fb0",
            view.fb_len() / 1024
        );
        mpos::drivers::video::render::spawn(device, view);
    }
    // Fired whether or not a card was found: a machine with no display reaches
    // the same step, and its suite is the one that says so.
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Display, info);

    // The console's own interrupts, before the handoff: the mux has to be up
    // first, so a byte the receiver delivers belongs to the active virtual console
    // rather than to the console tty (`steps::arm_console` carries that contract),
    // and the writer has to be queueing into the ring the interrupt drains before
    // a program on the other end of the wire reads anything.
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::ConsoleInput, info);
    steps::arm_console();
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::ConsoleTx, info);

    // The stack over the adapter the probe published, and the one permanent task
    // that keeps the polled devices moving (the input rings, the vsock family, the
    // display's own events). The stack needs the scheduler the step above started;
    // a machine with no adapter says so and its socket calls answer `ENETDOWN`.
    match mpos::drivers::virtio::net::device() {
        Some(dev) => steps::bring_up_net(dev),
        None => mpos::println!("net: no adapter; sockets answer ENETDOWN"),
    }
    mpos::drivers::virtio::spawn_service();

    // What a dying process's descriptors own, before anything can die.
    steps::register_exit_hooks();

    // The machine's own entropy device, folded into the seed before the generator
    // stamps one. `rand::fill` would seed lazily on first use; saying it here is
    // the boot saying *when*, so that the seed is one fact about this machine
    // rather than a side effect of whichever caller arrived first.
    mpos::rand::set_device_entropy(mpos::drivers::virtio::entropy);
    mpos::rand::init();

    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Ipc, info);
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Done, info);

    mpos::println!("All ready!");

    // A line queued on the console *before* the handoff, so the init's own read
    // returns real input instead of blocking for ever. The early userland's script
    // reads one line and echoes it back when the boot carries `mpos.selftest`
    // (`assets/initramfs/core/init`), and that echo is what makes "a line reached a
    // program at EL0 through the console, read and written back through the
    // dispatch" a claim with evidence behind it rather than a hope. A boot whose
    // init does not read leaves the line in the tty's buffer, where nothing else
    // looks for it.
    //
    // A *test build*'s and nothing else's: injecting a line is the one thing a real
    // handoff must never do, because a distribution's init reads the same tty
    // through getty, where the line would arrive as a spurious username at the
    // login prompt.
    #[cfg(feature = "selftest")]
    for &byte in b"echo-test\n" {
        vfs::tty::global().receive_byte(byte);
    }

    // A check whose absence is deliberate, and whose absence the gate enforces:
    // this is where "the command line asks for a self-test and this kernel is the
    // ordinary build - say so instead of queueing nothing and letting the early
    // userland wait for ever" would go, and `xtask`'s own gate refuses it by name
    // ("the boot has no test opening", `NOTES.md` §15.19): whether a kernel
    // carries tests is the *build's* to say, not a word's and not a run-time
    // check's. That is also why `boot::cmdline_has` has no caller - it is the one
    // spelling of a question nothing in the boot is allowed to ask.
    //
    // Which leaves the pairing this check would have caught to be got right where
    // it can be got right: at the build, by the loader carrying the kernel its
    // caller just built rather than whichever variant last stood at the target
    // path (`xtask/src/utils.rs::build_loader`). Measured wrong once, before that
    // was fixed: the second machine booted the ordinary kernel into a self-test
    // boot, printed every line above this one, and then waited for ever - no test
    // output, and a cause in two files this one is not among.

    hand_to_init(info)
}

/// The heap, exercised once and reported.
fn init_mm() {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend(b"hello from mpos".iter().copied());
    buffer.push(b'!');

    let text = String::from_utf8(buffer).expect("valid utf8");
    mpos::println!("{}", text);

    let heap = mpos::mm::heap_stats();
    mpos::println!(
        "heap: {} B live, {} B cached, {} B mapped",
        heap.live,
        heap.cached,
        heap.mapped
    );
}

/// Load `/init` and put it on pid 1, then wait for the machine to be handed over.
///
/// The path is read through the VFS, so it is whatever `/` resolves to: the ramfs
/// holding the boot archive, which is the only root this kernel establishes. What
/// the machines agree on - and what [`steps::hand_to_init`] asserts rather than
/// trusts - is that the first program the scheduler starts *is* pid 1: a boot that
/// put the machine's init anywhere else has not handed it over, and the failure
/// belongs here rather than as a subtle wrongness found by the first `getpid` a
/// program makes.
fn hand_to_init(info: &BootInfo) -> ! {
    // The spawn and the line about it happen with interrupts masked, and that
    // window is not fastidiousness: the clock preempts, so a tick landing between
    // the spawn and the print can let a short init run to completion - and pid 1
    // leaving the machine prints `INIT_EXITED` and halts it, so the verdict the
    // sweep polls for would never be written. The line goes out through the
    // machine's *polled* writer for the same reason: `println!`'s queue can park a
    // writer, and parking with interrupts off is a deadlock.
    let pid = critical_section::with(|_| {
        let pid = steps::hand_to_init();
        mpos::arch::print_args(format_args!("mpos: handing the machine to /init\n"));
        pid
    });

    // The deepest the boot stack ever gets: `hand_to_init` parsed an ELF and built
    // a process's address space and stack on top of everything above. Checking
    // here rather than after the handoff is the point - once init runs, the trap
    // that lands on an overrun stack is the one that cannot report it.
    report::check_boot_stack(info, "under hand_to_init");

    // "The machine is handed over" is init running something, which is what the
    // child under it means: init forks and the shell - or a payload's runner - is
    // that child. An init that *exits* instead has handed the machine over just as
    // much (the exit path prints its own `INIT_EXITED` and halts), so the wait ends
    // there too rather than sleeping for ever on a pid that is gone.
    while sched::is_running(pid) && !sched::has_live_child(pid) {
        sched::sleep_ticks(1);
    }
    mpos::println!("mpos: /init is up");
    report::park()
}
