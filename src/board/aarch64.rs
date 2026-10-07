//! The aarch64 kernel: what this machine's boot does once it is up, and what it
//! accepts as proof that it works.
//!
//! The *machine* half of the boot is `mpos_arch::arch::aarch64::boot`: the
//! loader reads the kernel off the ESP, applies its relocations against
//! `KERNEL_VIRT_BASE`, builds the page tables, exits boot services and branches
//! into the entry with the machine already running in the higher half, and from
//! there that machine's own body takes it as far as the EL1 vector table, the
//! kernel's own page tables, and the machine's devices. It calls back here at
//! three points - [`milestone`], [`devices`] and [`init`] - because everything in
//! them names `vfs`, `blk`, `net` or `drivers`, which sit above the machine layer
//! and cannot be named from `mpos-arch` (`AGENTS.md` §3, downward-only).
//!
//! There is no entry stub and no stack to set up: the one interface is the
//! register the record arrives in, exactly as on x86_64 (`RDI` there, `x0` here),
//! and [`start`]'s own prologue is the first thing to use the stack the loader
//! mapped.
//!
//! What this machine had until stage 1e was a `global_asm!` `_start` that loaded
//! `sp` from a `__stack_top` symbol the linker script cut out of the image, and
//! a `.bss` walk beneath it. Both belonged to the `-kernel` bring-up: QEMU
//! placed the ELF at a fixed physical address and jumped in with `sp = 0`, so
//! the image had to carry its own stack and could not assume its `NOBITS` tail
//! came back zeroed. The loader does both of those now, which is why x86_64's
//! entry has neither and why this one no longer does.

use core::fmt;

use super::checks;
use super::report;
use super::steps;
use mpos::boot::handoff::Handoff;
use mpos::{drivers, fdt, hal, sched, vfs};

/// The record `boot` hands the device probes: the machine as the loader
/// described it, which is where the device tree a probe walks comes from.
use mpos::boot::BootInfo;

/// Run this machine's kernel from the loader's record. Never returns.
///
/// `src/main.rs` hands this to `mpos_arch::arch::aarch64::boot` as the entry. The
/// record has to be checked rather than trusted: nothing in the machine enforces
/// that the entry register holds what this function was compiled to expect, so
/// the machine layer's translation of it asserts the magic before anything reads
/// a field - the same contract x86_64 carries, spelled in a different register.
///
/// The three functions handed down are this file's, and they are here rather than
/// in `mpos-arch` on purpose: [`milestone`] writes through `crate::console`,
/// [`devices`] opens `drivers` and mounts `vfs` filesystems, and [`init`] loads
/// the first program - all of them above the machine layer, so `arch` may call
/// the pointers but never name what they do.
///
/// # Safety
///
/// The caller is the loader's branch into `_start`, with `x0` holding the record
/// it wrote and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust.
pub unsafe fn start(handoff: *const Handoff) -> ! {
    // SAFETY: the caller's contract, restated in `arch::aarch64::boot`'s own
    // `# Safety` - the record handed over is the one the loader wrote, and the
    // machine layer's translation of it is what checks the magic.
    unsafe { mpos::arch::boot(handoff, milestone, devices, init) }
}

/// The aarch64 kernel's own boot: bring this machine's devices up, prove the
/// kernel works, then hand the machine to `/init`.
///
/// The machine layer calls it as its `init` callback, so it runs on the kernel's
/// own page tables with the record already turned into `BootInfo` and the PL011
/// up. It is *not* the same function as `board::x86_64`'s `init`, and the two
/// differ where the machines do: this one opens virtio-MMIO windows instead of
/// enumerating a PCI bus, builds `/dev` and switches root from the kernel, and
/// runs its own M7 device/disk/console checks in the order those milestones
/// taught it.
///
/// What the two do share is the five steps with ordering contracts in them - the
/// scheduler, the bootstrap root, the unpack, the console arming and the pid-1
/// handoff - and those are the sibling `steps`'s, called here in the order *this*
/// machine needs. Nothing in that module names a machine; nothing here restates
/// its contracts.
///
/// Starting the scheduler is what lets the syscall gate work: from here on
/// `sched::started()` is true, a real task is current, and `/init`'s `svc` routes
/// into the shared `syscall::dispatch` table rather than the inline refusal the
/// pre-scheduler probe still gets. `/init` is the four-instruction `exit_group(0)`
/// program `xtask` builds; running it to completion and reaping it proves the
/// whole load -> enter EL0 -> trap -> dispatch -> exit round trip on this machine.
fn init(info: &BootInfo) -> ! {
    // The archive, straight out of the record - the same read the first machine's
    // body makes, and the reason the init seam is one signature on both machines
    // now: `boot` hands this call the `BootInfo` it has held since before the
    // address-space switch, whose slices point into the direct map that switch
    // keeps (`arch/aarch64/mod.rs` argues the lifetime). Every boot of this
    // machine now carries a real initramfs - busybox, `/bin/sh` and the pivot
    // script - because the root switch is the *script's* job on both machines
    // (`PLAN.md` §15.6), not the kernel's.
    let archive: Option<&[u8]> = info.initramfs;

    // The log sink, as the first act - the same call the first machine makes at
    // the same point, over the same subscriber (`src/logging.rs`, one copy now
    // that its gate no longer claims this machine has no console or clock). The
    // console service is already up here - `milestone` printed `M4_CONSOLE_OK`
    // before this ran - so a `tracing` line from anything below reaches the wire
    // instead of being dropped on a machine with no subscriber at all.
    mpos::logging::init();

    // The milestone checks prove each subsystem in isolation, and they run under
    // the same word the first machine's do - `mpos.selftest`, which the verdict
    // and sweep boots carry and the Alpine boot does not. That gate is the point,
    // not a convenience: on this machine the checks used to *be* the boot's
    // verdict, so they ran in front of a distribution's `init`, and §13.14 is the
    // measurement of what that costs (a wall-clock check left the year 36812 and
    // killed `syslogd`). A machine that is being tested runs them; a machine that
    // is being used does not.
    let selftest = mpos::boot::cmdline_has("mpos.selftest");

    // The scheduler adopts the running context (the boot stack) as task 0, and
    // the bootstrap root comes up under it. Both are steps every machine takes
    // and both carry an ordering contract, so they are stated once in
    // `board::steps` instead of once per port.
    if selftest {
        checks::mm::run(info);
    }

    steps::start_scheduler();
    if selftest {
        checks::sched::run();
    }
    steps::mount_ramfs_root();

    // The unpack, where the first machine's sits: the ramfs becomes the whole
    // early userland before any device work, and the `/init` handed to pid 1 is
    // the *script* the archive carries - the same program shape both machines
    // hand over, shebang included (`spawn_init` resolves it against the machine's
    // tree, which is still this ramfs until the script pivots).
    let Some(archive) = archive else {
        mpos::println!(
            "M7_ROOT_FAILED: the boot carries no initramfs - this machine's root switch lives in its script"
        );
        hal::halt()
    };
    steps::unpack(archive);

    // Stage 7-dev: the device set a real init mounts as `/dev` builds on this
    // machine and its entropy node reads back. It runs here, after
    // `vfs::tty::init()` has populated the tty table (the console tty is what
    // `standard_devfs()` is built from), not as an early boot milestone - which
    // is the ordering lesson behind the `/dev` note in `PLAN.md` §9.1.
    // Stage 8-gpu: the card, opened and handed to the render task. It stands
    // before `check_dev` because that call is what publishes `/dev/fb0`, and it
    // stands in `init` rather than with the device checks because the task that
    // owns the device needs the scheduler `steps::start_scheduler` started.
    check_gpu(info);

    check_dev();

    // Stage 8-input's, stage 8-console's and stage 8-gpu's verdicts, and they
    // have to stand here rather than with the device checks: what each reads
    // back is reached through the mounted `/dev`, which only exists once
    // `check_dev` has mounted it.
    check_input_registry();
    check_console_dev();
    check_gpu_dev();
    check_sound_dev();

    // The host share, on the ramfs `/mnt` - mounted *before* the disk is hung
    // there, so the 9p mount is a child of the bootstrap root in the table and
    // not of whatever the disk's own tree happens to hold. That order is the
    // whole difference: with the disk already at `/mnt`, the step's `/mnt`
    // lookup resolves into the ext4, and mounting `share` under it asks a
    // read-only image for a directory it does not have - measured as
    // `"hostshare" did not mount: NoEntry`, and then `M8_P9_FAILED`.
    // `resolve_walk`'s longest-prefix rule keeps the earlier mount reachable as
    // `/mnt/share` after the script promotes the disk into `/`, which is what
    // lets the verdict read the same path the first machine's boot reads
    // (§15.6).
    steps::mount_share();
    check_p9_dev();

    // Stage 7's disk root, as a *mount* rather than a pivot: the ext4 the block
    // layer registered is hung at `/mnt` with the name and number `/dev/vda`
    // carries, so the self-test payload reads seed files through the mount table
    // and the initramfs script promotes the same instance into `/` without
    // re-opening the device. What the kernel does *not* do any more is switch
    // `/`: that is the script's half of the flow both machines share (§15.6) -
    // `M7_DISK_OK`/`M7_ROOT_OK` are no longer kernel markers, and the boot's
    // verdict for the switched tree is the script's own `MPOS_SWITCH_ROOT_OK`
    // line. The step itself is the shared `steps::mount_disk_root`, called
    // exactly where x86_64's `init_blk` calls it; there is no machine-specific
    // wrapper left around it.
    // It comes second so the share above stays a child of the bootstrap root.
    steps::mount_disk_root();

    // Stage 7b: the polled console *input* path, pinned before anything reads it.
    // The machine can now hand a typed byte through `hal::console_poll_input` (the
    // PL011 RX FIFO); route it into the tty the way x86_64's serial interrupt does
    // (`vcmux::route_input`) and read it back through the tty's own `read`. A
    // headless boot has no terminal, so the UART's own loopback injects the byte.
    // Doing this *before* the console's interrupt is armed means the self-test is
    // the only reader of the receiver, so nothing can take its bytes from it.
    if selftest {
        check_console_rx();
    }

    // Arm the console's interrupt, now that there is a tty for a typed byte to
    // land in and a scheduler for a writer to park on. From here the console
    // neither polls the receiver nor waits on the wire: every line after this
    // goes through the queue the PL011's own interrupt drains, exactly as the
    // first machine's do - the same `crate::console` code over the same
    // `hal::SerialRing` capability, with this one arming call in front of it.
    // `steps::arm_console` carries the mux-first ordering contract, which is
    // the same one and the same reason on both machines (`PLAN.md` §13.16).
    //
    // **Not gated on `mpos.selftest`**, and that is the point rather than a
    // detail: this is the step that puts the console mux up, and without the mux
    // nothing carries a virtual console's output to the wire. Measured: with the
    // call inside the `selftest` arm, the Alpine boot ran its whole openrc
    // runlevel - `[ ok ]` through `syslog`, `crond` and `local` - and then never
    // printed `login:`, because the getty busybox init spawned is on `/dev/tty1`,
    // a pty whose master only the pump reads. The check *of* the armed path
    // (`check_serial_tx`) stays under the test word; the arming does not.
    //
    // This machine arms it *here*, before `check_serial_tx`, because that check
    // is a check *of* the armed path: it asserts a write goes through the ring
    // the interrupt drains. x86_64 arms the same pair after its spawn instead,
    // which is why the step is shared and the placement is not.
    steps::arm_console();

    if selftest {
        check_serial_tx();
    }

    // Stage 8-net: the smoltcp stack over the MMIO adapter `check_net` opened and
    // published. The adapter is the machine's to find (a window the tree names)
    // and the stack is not: it receives the `NetDevice` seam exactly as x86_64's
    // does, so this is the same `net::init` + `spawn_service` pair the first
    // machine boots with, and nothing above `drivers` names a transport.
    //
    // **Not gated on `mpos.selftest`**, and the gate's removal is the point of
    // this cut: this is bring-up, not a check, and the first machine has always
    // run its own `init_net` unconditionally (`board/x86_64.rs`'s `init`, where
    // its PCI enumeration leads straight into it). The gate here had been read
    // as §13.14's rule - "a service that did not assemble an interface must not
    // be handed one" - but that rule is about *checks*, and this machine's
    // adapter is assembled before this line either way: `check_net` is part of
    // `devices()`, which `arch::boot` calls on every boot, in front of the
    // unpack, and the window it opens is what `net_device()` below picks up. A
    // boot that reached here has already proven a virtio-net window exists (that
    // check halts the machine otherwise), so gating the *bring-up* changed
    // nothing about what the tree carries and only decided whether the stack
    // above it was polled. Measured: with the gate in place, the Alpine boot had
    // a published adapter and a dead `net` task - `run --arch aarch64`'s GUI
    // input pump never started either, because the same task drains the input
    // devices' rings.
    init_net();
    // The input devices' pump. The registry is filled (`check_input`) and there
    // is a scheduler for the task to park in, so the thing that drains the
    // devices' rings can start - and it is the first machine's `spawn_service`,
    // whose vsock and display halves are its own devices and gated there, so the
    // second machine runs the input half of one shared task rather than one of
    // its own. Like `init_net` above it is not a check and not gated: the first
    // machine calls the same function from `init_virtio` on every boot.
    mpos::drivers::virtio::spawn_service();

    steps::register_exit_hooks();

    // Stage 6/7's two remaining families, and they stood here held out for
    // exactly one reason: with either one wired, this machine took an
    // instruction abort at EL1 on the next preemption after them - `EC 0x21`,
    // `ELR_EL1 == FAR_EL1` at a direct-mapped *heap* address (a page of the
    // console's 559-byte transmit buffer). The cause was the kernel's own
    // interrupt stub: it did not save `ELR_EL1`/`SPSR_EL1`, so a task the clock
    // preempted at EL1 resumed at whatever the last trap on the core had left in
    // them - measured by catching the abort at its vector entry, where the
    // interrupted registers read `x19 = 0x22f` (the buffer's length), `x20 =` the
    // buffer, `x30 = mpos_task_trampoline + 16`: the CPU was executing the task
    // trampoline's `blr x20` with a *copy loop's* scratch registers, because an
    // earlier `eret` had returned there. `arch/aarch64/vectors.rs` saves and
    // restores the pair now (`TRAP_EL1_IRQ_STATUS_OFFSET`), which is the EL1 twin
    // of the EL0 stub's fix and the reason these two run here again.
    //
    // They run after the unpack and the disk mount, which is the state the first
    // machine's checks run in: a bootstrap ramfs carrying the boot archive, with
    // `/dev` and the tty table already up.
    if selftest {
        checks::vfs::run();
        checks::ipc::run();
        report::check_boot_stack(info, "after the self-tests");
    }

    // Queue a line on the tty before the init starts reading, so its `read(0,...)`
    // returns real console input instead of blocking. This stands in for a person
    // typing (which `cargo xtask run --arch aarch64` provides live, via the stage-7b
    // input pump); at headless bring-up it lets boot-aarch64 prove the interactive
    // *read* path reaches a userspace program, not just the kernel.
    //
    // Gated on `mpos.selftest`, because it is an *instrument* and not something a
    // kernel does: injecting a line into the console is the one thing a real
    // handoff must never do, and a real userland init (Alpine's) reads the same
    // tty through getty - where the injected line would arrive as a spurious
    // username attempt at the login prompt. The test boots carry the word; the
    // Alpine boot does not, and gets an untouched console.
    if selftest {
        for &byte in b"echo-test\n" {
            vfs::tty::global().receive_byte(byte);
        }
    }

    // `/init` is read from the ramfs root and loaded as a task; it lands on
    // pid 1 - the same handoff the first machine makes, onto the same kind of
    // program: a `#!/bin/sh` script that mounts, switches root and execs the
    // real init.
    //
    // The spawn and this verdict's two lines happen with interrupts masked, and
    // that window is what preemption cost: `M6_INIT_OK` used to be printed after
    // the handoff with the words "it has not run yet" holding by themselves,
    // because nothing could preempt the boot task on the way. The clock preempts
    // now (`arch/aarch64/vectors.rs`), so a tick landing between the spawn and
    // the print can let a short init run to completion - and pid 1 leaving the
    // machine prints `INIT_EXITED` and halts it, so the marker the verdict polls
    // for is never written. Measured: that is exactly what the sweep's boot did
    // once `checks::sched`'s preemption fix landed. The lines go out through the
    // machine's *polled* writer for the same reason the failure reports next door
    // do: `println!`'s queue can park a writer, and parking with interrupts off
    // is a deadlock.
    let pid = critical_section::with(|_| {
        let pid = steps::hand_to_init();
        mpos::arch::print_args(format_args!("mpos: aarch64 machine handed to /init\n"));
        mpos::arch::print_args(format_args!("M6_INIT_OK\n"));
        pid
    });

    // The deepest the boot stack ever gets: `hand_to_init` parsed an ELF and built
    // a process's address space and stack on top of everything above. Checked here
    // rather than after the init runs - once it does, the trap that lands on an
    // overrun stack is the one that cannot report it.
    report::check_boot_stack(info, "under hand_to_init");

    // What "init is up" means is the machine's answer, and since §15.6 both
    // machines give it the same way: a test boot's init runs to completion and
    // leaves (`INIT_EXITED` from the exit path is the terminal proof the verdict
    // polls), while a real boot's init forks and stays - so waiting for a child
    // under it is the first machine's "the shell is up" signal, printed as its
    // `M8_OK`.
    if selftest {
        while sched::is_running(pid) {
            sched::sleep_ticks(1);
        }
    } else {
        while !sched::has_live_child(pid) {
            sched::sleep_ticks(1);
        }
        mpos::println!("M8_OK");
    }
    report::park()
}

/// The machine's own boot writer, as the wiring layer gets hold of it.
///
/// The device probes below print their markers on the wire `arch` prints its
/// own on, and for the same reason the panic path does: they run before the
/// console is carrying the machine's output, and a marker that went missing
/// because a service was not up yet is not a boot log anyone can read. It is
/// reached through the facade rather than by naming the UART, so nothing here
/// knows which machine's wire it is writing to.
struct MachineWriter;

impl fmt::Write for MachineWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        mpos::arch::print_args(format_args!("{text}"));
        Ok(())
    }
}

/// Stage 5b/7/8's device discovery: find this machine's virtio-MMIO transports
/// and open the four of them the boot drives.
///
/// Called by `arch::boot` once the kernel's own page tables are installed -
/// the windows have to be reachable through them, and which windows exist is
/// the machine's to know, so the machine layer says *when* and the wiring
/// layer says *what*. The pointer travels down exactly as [`milestone`] does:
/// `arch` calls a pointer it was handed and names none of this.
///
/// That this is the wiring layer's job rather than `arch`'s is the whole point:
/// every line below belongs to a driver, a filesystem or the block layer, and a
/// machine crate cannot reach any of them. Asking for a window through
/// `hal::map_mmio_window` is the one machine question in here, and it is the same
/// question a PCI driver asks on the first machine - which is why both machines
/// answer this hook with a function of the same shape.
fn devices(info: &BootInfo) {
    check_virtio(info);
    check_rng(info);
    // The rng device `check_rng` just installed, registered as the source
    // `rand` folds into its seed. Registered here rather than named from
    // `rand`: which device this machine has is this layer's fact, and `rand`
    // naming `crate::drivers` would close a cycle with the edge that already
    // runs the other way (`PLAN.md` §14.3).
    mpos::rand::set_device_entropy(drivers::virtio::entropy);
    check_blk(info);
    check_fs(info);
    check_net(info);
    check_input(info);
    check_console(info);
    check_sound(info);
    check_vsock(info);
    check_p9(info);
}

/// The virtio-MMIO identity registers: the four words a transport answers before
/// any driver has touched it.
///
/// They are the whole reason this machine can be asked *which* of its windows
/// carry a device. QEMU's `virt` publishes thirty-two `virtio,mmio` nodes
/// whether or not anything is attached to them, and that is measured rather than
/// assumed: the tree decompiled with and without `-device virtio-rng-device`
/// differs only in the firmware's own random seeds, byte for byte. So the tree
/// cannot say, and the transport's own registers can.
/// `MagicValue`: the four letters `virt` as one little-endian word.
const VIRTIO_MMIO_MAGIC: usize = 0x000;
/// `Version`: which register layout this transport speaks.
const VIRTIO_MMIO_VERSION: usize = 0x004;
/// `DeviceID`: zero when no device is attached to the transport.
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
/// `VendorID`: whose device it is, which the specification leaves to the
/// implementation.
const VIRTIO_MMIO_VENDOR_ID: usize = 0x00c;
/// `MagicValue`: the four ASCII letters `virt`, read as one little-endian word.
const VIRTIO_MAGIC: u32 = 0x7472_6976;
/// `Version`: 2 is the modern (1.0) transport, which is the only register layout
/// the driver crate reads. Version 1 is the legacy one, whose registers are at
/// different offsets and which has no device-feature words at all.
const VIRTIO_VERSION_MODERN: u32 = 2;
/// `DeviceID` for the entropy device (`VIRTIO_ID_RNG`). The value the
/// specification fixes for the one device this machine drives first over MMIO -
/// measured: the `virtio-rng-device` QEMU attaches to a `virt` MMIO transport
/// answers `DeviceID` 4 (vendor "QEMU"), and the driver crate's `DeviceType::Rng`
/// is the same 4, so `VirtIORng::new` accepts the window this selects.
const VIRTIO_ID_RNG: u32 = 4;
/// `DeviceID` for the block device (`VIRTIO_ID_BLOCK`), the second device driven
/// over MMIO - the disk root the drivers stage names (`PLAN.md` §9.1).
const VIRTIO_ID_BLOCK: u32 = 2;
/// `DeviceID` for the network adapter (`VIRTIO_ID_NET`), the third device driven
/// over MMIO - the `net` stage's transport proof on this machine (`PLAN.md` §9.1).
const VIRTIO_ID_NET: u32 = 1;
/// `DeviceID` for an input device (`VIRTIO_ID_INPUT`), the fourth transport the
/// aarch64 MMIO virtio path is proven to reach (`PLAN.md` §9.1).
const VIRTIO_ID_INPUT: u32 = 18;
/// `DeviceID` for a console port (`VIRTIO_ID_CONSOLE`), the fifth device class
/// this machine drives over MMIO - the host-driven second serial line the first
/// machine has as `/dev/hvc0` (`PLAN.md` §13.25).
const VIRTIO_ID_CONSOLE: u32 = 3;
/// `DeviceID` for a display adapter (`VIRTIO_ID_GPU`), the sixth device class
/// this machine drives over MMIO - the card behind `/dev/fb0` and the
/// framebuffer console (`PLAN.md` §13.26).
const VIRTIO_ID_GPU: u32 = 16;
/// `DeviceID` for a sound card (`VIRTIO_ID_SOUND`), the seventh device class this
/// machine drives over MMIO - the card behind `/dev/dsp` (`PLAN.md` §13.27).
const VIRTIO_ID_SOUND: u32 = 25;
/// `DeviceID` for a socket device (`VIRTIO_ID_VSOCK`), the eighth device class
/// this machine drives over MMIO - the wire underneath the `AF_VSOCK` family
/// (`PLAN.md` §13.28).
const VIRTIO_ID_VSOCK: u32 = 19;
/// `DeviceID` for a 9p device (`VIRTIO_ID_9P`), the ninth device class this
/// machine drives over MMIO - the host share behind `/mnt/share` (`PLAN.md`
/// §13.29).
const VIRTIO_ID_9P: u32 = 9;

/// One 32-bit read of a device's registers, through the kernel's own tables.
fn read_device(virt: usize, offset: usize) -> u32 {
    // SAFETY: `virt` came from `paging::device_virt`, which only answers for a
    // window wholly inside the range `install` mapped as Device and never
    // executable, and every `offset` below is one of four words inside the
    // 512-byte stride a virtio-mmio transport owns.
    unsafe { core::ptr::read_volatile((virt + offset) as *const u32) }
}

/// Stage 5b's evidence: the kernel's own tables reach the machine's transports,
/// and the transports answer as transports.
///
/// Stage 5a held the tree up against constants, and 5b's first half put the
/// window those devices live in into the kernel half; what neither could say is
/// whether there is really a *device* on the other side of an address. This
/// reads four registers through the window and checks the two the specification
/// fixes, so a window with nothing behind it - a tree naming a hole, a mapping
/// at the wrong physical address, an attribute that turned the reads into
/// something else entirely - ends as a named failure here rather than as a
/// driver that times out three stages from now.
///
/// The two numbers it does *not* check are checked against nothing on purpose:
/// `DeviceID` is 0 on a transport with no device behind it (which is what most
/// of a bare `virt` machine's thirty-two windows are, and is legal), and
/// `VendorID` is the implementation's to choose. Both are reported, because
/// which device answers is the fact the next stage needs.
///
/// It runs after `check_paging`, and that is the point of it: every address
/// below is in the kernel half, and the walk is what proves the page tables
/// reach the window rather than that the window exists.
fn check_virtio(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    /// News this machine cannot act on: say which claim failed and stop, as
    /// `check_devices` does and for the same reason - the marker is what
    /// `xtask boot-aarch64` polls for.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M5_VIRTIO_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let Some(bytes) = info.dtb else {
        fail!("the record carries no device tree, so the machine cannot say where its devices are");
    };
    let tree = match fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(error) => fail!("the device tree will not walk: {error:?}"),
    };

    // What the walk collects, as numbers rather than as text: the closure
    // `Fdt::each` calls cannot borrow `out`, so nothing can be reported from
    // inside it and every failure has to survive until the walk is over.
    // `check_devices` walks the same tree through the same shape.
    let mut windows = 0usize;
    let mut attached = 0usize;
    let mut first: Option<(usize, u32, u32)> = None; // address, device id, vendor id
    let mut outside: Option<(usize, usize)> = None; // address, size
    let mut wrong_magic: Option<(usize, u32, u32)> = None; // address, what it said, version
    let mut wrong_version: Option<(usize, u32)> = None; // address, what it said

    let walked = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            windows += 1;
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = hal::map_mmio_window(address, size) else {
                    outside = Some((address, size));
                    continue;
                };
                let magic = read_device(virt, VIRTIO_MMIO_MAGIC);
                let version = read_device(virt, VIRTIO_MMIO_VERSION);
                let device = read_device(virt, VIRTIO_MMIO_DEVICE_ID);
                let vendor = read_device(virt, VIRTIO_MMIO_VENDOR_ID);
                if magic != VIRTIO_MAGIC {
                    // The first offender wins, not the last: a machine with one
                    // window in the wrong place is easier to read off a boot log
                    // than the thirty-second one to be asked.
                    wrong_magic.get_or_insert((address, magic, version));
                } else if version != VIRTIO_VERSION_MODERN {
                    wrong_version.get_or_insert((address, version));
                } else if device != 0 {
                    attached += 1;
                    if first.is_none() {
                        first = Some((address, device, vendor));
                    }
                }
            }
            true
        },
    );
    if let Err(error) = walked {
        fail!("the device tree will not walk: {error:?}");
    }
    if let Some((address, size)) = outside {
        fail!(
            "the tree's virtio window at {address:#x}..{:#x} is outside the device \
             window this kernel maps, so the machine has no address for it",
            address + size,
        );
    }
    if let Some((address, magic, version)) = wrong_magic {
        fail!(
            "the transport at {address:#x} answered {magic:#x} for MagicValue (with \
             version {version}), and a virtio-mmio transport answers {VIRTIO_MAGIC:#x}"
        );
    }
    if let Some((address, version)) = wrong_version {
        fail!(
            "the transport at {address:#x} answers version {version}, and this \
             kernel's driver reads the modern (version {VIRTIO_VERSION_MODERN}) layout"
        );
    }
    if windows == 0 {
        fail!("the tree describes no virtio-mmio transport at all");
    }

    let _ = writeln!(
        out,
        "virtio: {windows} transports, every one answering virtio 1.0 through the \
         kernel half; {attached} {} a device",
        if attached == 1 { "carries" } else { "carry" },
    );
    if let Some((address, device, vendor)) = first {
        let _ = writeln!(
            out,
            "virtio: the first is at {address:#x}, device {device}, vendor {vendor:#x}"
        );
    }
    let _ = writeln!(out, "M5_VIRTIO_OK");
}

/// Stage 7's first driven device: virtio-rng over the memory-mapped transport.
///
/// `check_virtio` proved the kernel's page tables reach the machine's transports
/// and that they answer as transports; this opens the one window the tree says
/// carries an entropy device (`DeviceID` 4) as a *real virtio device* over MMIO
/// and asks it for bytes - the drivers stage's first device driven to a marker
/// (`PLAN.md` §9.1). The window is handed to the driver as a kernel-virtual
/// address, so `drivers::virtio::mmio` names neither the tree nor the boot
/// record: discovery is the machine layer's job, the driver's job is a register
/// block it is given. Success is 32 host-generated bytes back; a window that is
/// not an RNG, a device that answers short, or none at all is a named
/// `M7_RNG_FAILED` - never a zeroed buffer reported as entropy.
fn check_rng(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    /// Say which claim failed and stop; the marker is what `xtask boot-aarch64`
    /// polls for, as `check_virtio` does.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_RNG_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let Some(bytes) = info.dtb else {
        fail!("the record carries no device tree");
    };
    let tree = match fdt::Fdt::new(bytes) {
        Ok(tree) => tree,
        Err(error) => fail!("the device tree will not walk: {error:?}"),
    };

    // The entropy device's window as (kernel-virtual header, size). Collected,
    // not opened inside the closure: `Fdt::each`'s callback cannot borrow `out`.
    let mut rng: Option<(usize, usize)> = None;
    let walked = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            if rng.is_some() {
                return true;
            }
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = hal::map_mmio_window(address, size) else {
                    continue;
                };
                if read_device(virt, VIRTIO_MMIO_MAGIC) == VIRTIO_MAGIC
                    && read_device(virt, VIRTIO_MMIO_DEVICE_ID) == VIRTIO_ID_RNG
                {
                    rng = Some((virt, size));
                    break;
                }
            }
            true
        },
    );
    if let Err(error) = walked {
        fail!("the device tree will not walk: {error:?}");
    }
    let Some((virt, size)) = rng else {
        fail!("the tree describes no virtio-rng (device {VIRTIO_ID_RNG}) window");
    };

    // Install the device as the machine's entropy source *before* checking it, so
    // a successful read also leaves `mpos::rand`'s seed able to mix hardware
    // bytes (the boot calls `rand::init` next, which reads this global). A window
    // that maps but does not open as a virtio-rng is a named failure, not a silent
    // fall-through to a weaker seed.
    let Some(dev) = mpos::drivers::virtio::mmio::init_global(virt, size) else {
        fail!("the window at {virt:#x} mapped but did not open as a virtio-rng transport");
    };
    let mut buf = [0u8; 32];
    let got = dev.fill(&mut buf);
    if got != buf.len() {
        fail!(
            "the virtio-rng returned {got} of {} entropy bytes",
            buf.len()
        );
    }
    // A sum over the returned bytes so the log proves they are the device's, not
    // a buffer left zero: a request that came back empty of entropy still fills
    // nothing, and only the `got == 32` guard above would hide that.
    let sum: u32 = buf.iter().map(|&b| b as u32).sum();
    let _ = writeln!(
        out,
        "virtio-rng over MMIO installed, returned 32 bytes (sum {sum})"
    );
    let _ = writeln!(out, "M7_RNG_OK");
}

/// Every `virtio,mmio` window this machine maps whose `DeviceID` is `want`, as
/// `(kernel-virtual header, size)`. QEMU assigns the MMIO slots in an order a
/// port cannot rely on, so a caller that owns a specific image picks its window by
/// what it contains (a sector pattern, a filesystem that mounts) rather than by
/// position. `Err` if the record carries no tree, or the tree will not walk.
fn virtio_device_windows(
    info: &BootInfo,
    want: u32,
) -> Result<alloc::vec::Vec<(usize, usize)>, &'static str> {
    let bytes = info.dtb.ok_or("the record carries no device tree")?;
    let tree = fdt::Fdt::new(bytes).map_err(|_| "the device tree will not walk")?;
    let mut windows = alloc::vec::Vec::new();
    tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = hal::map_mmio_window(address, size) else {
                    continue;
                };
                if read_device(virt, VIRTIO_MMIO_MAGIC) == VIRTIO_MAGIC
                    && read_device(virt, VIRTIO_MMIO_DEVICE_ID) == want
                {
                    windows.push((virt, size));
                }
            }
            true
        },
    )
    .map_err(|_| "the device tree will not walk")?;
    Ok(windows)
}

/// Stage 7's second driven device over MMIO: virtio-blk, verified by a read.
///
/// `check_rng` proved a request device answers; a block device is the one the
/// disk root needs, and it is driven the same way - open the window as an
/// `MmioTransport`, hand it to the crate's `VirtIOBlk`. The pin is a *verified*
/// read: the raw image xtask lays down has a sector whose byte `i` is `i as u8`,
/// and with two block devices attached (this one and the ext4 one `check_fs`
/// mounts) the pattern is picked by content, not position - the window whose
/// sector 0 matches is this image. Capacity is read too (a config-space path).
fn check_blk(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    /// Say which claim failed and stop; the marker is what `xtask boot-aarch64`
    /// polls for.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_BLK_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_BLOCK) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-blk (device {VIRTIO_ID_BLOCK}) window");
    }

    // Pick the raw pattern image by content: read each block window's sector 0 and
    // keep the one that carries the `i as u8` pattern. The ext4 image's sector 0 is
    // a boot-sector of zeros, so it is not mistaken for this one.
    let mut buf = [0u8; mpos::drivers::virtio::mmio::SECTOR];
    let mut chosen: Option<(usize, usize)> = None;
    for &(virt, size) in &windows {
        if mpos::drivers::virtio::mmio::read_blocks(virt, size, &mut buf).is_some()
            && buf.iter().enumerate().all(|(i, &b)| b == i as u8)
        {
            chosen = Some((virt, size));
            break;
        }
    }
    let Some((virt, size)) = chosen else {
        fail!("no virtio-blk window returned the known pattern sector");
    };

    // Read the device's geometry too - a configuration-space path distinct from
    // the data read above. The raw image xtask lays down is 1 MiB = 2048 sectors,
    // so the capacity the device reports must agree; a transport that read a
    // sector but misreports its size would mount a filesystem over the wrong span.
    let sectors = mpos::drivers::virtio::mmio::capacity(virt, size).unwrap_or(0);
    if sectors != 2048 {
        fail!("the virtio-blk reported capacity {sectors}, not the 2048 sectors of the image");
    }
    let _ = writeln!(
        out,
        "virtio-blk over MMIO read a verified sector ({sectors} sectors)"
    );
    // The write path, not just the read: hand a distinctive sector to the device,
    // flush, and read it back through a fresh open. A round trip that does not
    // match means the write virtqueue or flush did not land the buffer - the other
    // half of the data path `read_blocks` never exercised. Sector 3 is chosen so it
    // does not disturb the pattern sector 0 the read above already validated.
    let mut wbuf = [0u8; mpos::drivers::virtio::mmio::SECTOR];
    for (i, b) in wbuf.iter_mut().enumerate() {
        *b = (i ^ 0xa5) as u8;
    }
    if mpos::drivers::virtio::mmio::write_readback(virt, size, 3, &wbuf).is_none() {
        fail!("the virtio-blk MMIO write path did not round-trip a sector");
    }
    let _ = writeln!(out, "virtio-blk over MMIO wrote and read back a sector");
    let _ = writeln!(out, "M7_BLK_OK");
}

/// Stage 7's disk-root primitive: a filesystem mounted over the MMIO block.
///
/// `check_blk` proved raw sector reads travel the memory-mapped transport; this
/// proves the whole `block driver -> filesystem` path by mounting an ext4 over one
/// of the block windows. With two block devices attached (the pattern image and an
/// ext4 image xtask lays down), the ext4 one is picked by trying each: `Ext4Fs::mount`
/// reads the superblock and group descriptors through the `BlockDev`, and only the
/// image that is actually an ext4 mounts. A filesystem that mounts is the disk root
/// the drivers stage names - reading a file through it and booting a root from it
/// are the task-context follow-on (`PLAN.md` §9.1). `M7_FS_FAILED` if none mounts.
fn check_fs(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M7_FS_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_BLOCK) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    // Reading a file through the mounted filesystem uses the `FileSystem`
    // trait surface directly (root -> lookup -> read_at); no fd table or
    // current task is involved, so it runs at boot. `Inode` needs no import:
    // a `dyn Inode`'s own methods are inherent candidates on the trait object.
    use mpos::vfs::FileSystem;
    let mut opened = false;
    for &(virt, size) in &windows {
        let Some(dev) = mpos::drivers::virtio::mmio::blockdev(virt, size) else {
            continue;
        };
        // `mount` only returns Ok for the window whose bytes are an ext4 the driver
        // can open; the raw pattern image is a clean `Err` and is skipped.
        let Ok(fs) = mpos::vfs::ext4::Ext4Fs::mount(dev.clone()) else {
            continue;
        };
        // The disk is registered with the block layer *after* the mount proved
        // the bytes, so the raw pattern image and any non-ext4 window cannot take
        // the name. That registration is what makes the device a kernel disk:
        // the devfs build publishes `/dev/vda` from `blk::disks()`, and the init
        // handoff mounts this same device through the VFS (`PLAN.md` §13.3's disk
        // root).
        //
        // Through the *driver's* registration - the same call the first machine's
        // bring-up makes - because the name, the major and the sysfs tree position
        // are one device's facts, and writing them out here is how the two copies
        // drifted: this machine used to file its disk under `/dev/vda`, so a
        // reader that followed `/sys/class/block/vda`, or matched on `DEVPATH`,
        // was handed a path that is not a device path (`PLAN.md` §15.10).
        mpos::drivers::virtio::register_block_disk(dev);
        // Read the one file the image carries, back through the block driver.
        let Ok(hello) = fs.root().lookup("hello") else {
            fail!("ext4 mounted but /hello was not found through it");
        };
        let mut buf = [0u8; 64];
        let Ok(n) = hello.read_at(0, &mut buf) else {
            fail!("reading /hello through the mounted ext4 failed");
        };
        let known: &[u8] = b"mpos reads from ext4 over virtio-MMIO\n";
        if &buf[..n] != known {
            fail!("the bytes read from /hello ({n}) did not match the file's contents");
        }
        opened = true;
        break;
    }
    if !opened {
        fail!("no virtio-blk window over MMIO mounted as ext4 and served a file");
    }
    let _ = writeln!(
        out,
        "ext4 mounted over the MMIO virtio-blk device, read /hello (36 bytes)"
    );
    let _ = writeln!(out, "M7_FS_OK");
}

/// Stage 8's net device: virtio-net over the memory-mapped transport, opened as
/// the adapter the *stack* will drive.
///
/// The net adapter is the third device driven the same way - find the window the
/// tree says carries one (`DeviceID` 1), open it as an `MmioTransport`, hand it
/// to the crate's `VirtIONet`. Two claims come out of the open, and both are about
/// the device the network stack is then given rather than about a throwaway one:
/// the 6-byte MAC is present and non-zero (a `virt` MMIO adapter always carries
/// the MAC QEMU assigned), and a frame handed to the transmit queue is accepted -
/// the rings move buffers over the mapped transport, the same class of proof the
/// block milestone made by reading a real sector back. It still claims nothing
/// about a packet reaching a network: that is the stack, and the programs that
/// drive it.
///
/// The handle is *published* rather than dropped ([`mmio::init_net_global`]), and
/// that is the point of this stage's change: an open posts the receive ring (64
/// buffers through the neutral `Hal`), so a check that opened the device, read its
/// MAC and dropped it would leave the stack to open a second one - and the ring
/// posted here is the ring the stack's first frame arrives on.
fn check_net(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_NET_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    // The tree walk is the same one the block and input checks use: every window
    // whose `DeviceID` says net, in tree order. The first that *opens* is the
    // machine's adapter - `check_fs` picks its disk by content for the same
    // reason, but a net device has no content to compare, so the open itself is
    // the selection.
    let windows = match virtio_device_windows(info, VIRTIO_ID_NET) {
        Ok(windows) => windows,
        Err(reason) => fail!("{reason}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-net (device {VIRTIO_ID_NET}) window");
    }
    let mut opened = None;
    for &(virt, size) in &windows {
        if let Some(dev) = mpos::drivers::virtio::mmio::init_net_global(virt, size) {
            opened = Some((virt, dev));
            break;
        }
    }
    let Some((virt, dev)) = opened else {
        fail!("no virtio-net window opened as a transport");
    };
    let mac = mpos::drivers::virtio::NetDevice::mac(&*dev);
    if mac.iter().all(|&b| b == 0) {
        fail!("the virtio-net answered a zero MAC, which a configured adapter never has");
    }
    let _ = writeln!(
        out,
        "virtio-net over MMIO opened, MAC {:#04x}:{:#04x}:{:#04x}:{:#04x}:{:#04x}:{:#04x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    // The data path, not just the config space: a bare minimum Ethernet frame (60
    // bytes) of zeros - the shape a real frame has, not its content - handed to the
    // transmit queue, which has to take it.
    if !mpos::drivers::virtio::NetDevice::transmit(&*dev, &[0u8; 60]) {
        fail!("the virtio-net TX queue did not accept a frame over MMIO at {virt:#x}");
    }
    let _ = writeln!(out, "virtio-net TX virtqueue accepted a frame over MMIO");
    let _ = writeln!(out, "M8_NET_OK");
}

/// The fourth MMIO device class: the aarch64 virtio transport reaches a
/// virtio-input device, and the machine keeps it.
///
/// xtask attaches a `virtio-keyboard-device` on the tree; this finds its window
/// by `DeviceID` and hands what it opens to `input::register` - so from here the
/// device is not a probe that proved a transport but the machine's keyboard.
/// `input::nodes()` names it, the devfs build below publishes it as
/// `/dev/input/event0`, the exit hooks release its readers and the service task
/// harvests its ring, all of which are the shared input stack the first machine
/// has had since §13.4 - reached here through the [`InputDevice`] contract
/// rather than through a second copy of it (`PLAN.md` §15.2).
///
/// The *marker* is deliberately not printed here. `M8_INPUT_OK` belongs to
/// [`check_input_registry`], which reads the registry back through the mounted
/// `/dev` - the pin this class carries - and this runs in `devices()`, before
/// there is a `/dev` to read anything through. A window that will not open is
/// still `M8_INPUT_FAILED` (a terminator) and stops the boot honestly, so the
/// failure is reported at the point it happens rather than one stage later.
///
/// [`InputDevice`]: mpos::drivers::virtio::InputDevice
fn check_input(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_INPUT_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_INPUT) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-input (device {VIRTIO_ID_INPUT}) window");
    }
    for &(virt, size) in &windows {
        let Some(device) = mpos::drivers::virtio::mmio::input_device(virt, size) else {
            continue;
        };
        let nodes = mpos::drivers::virtio::input::register(alloc::vec![device]);
        // The same line the first machine's `init_virtio` prints for each of its
        // bus's devices: one node per device, named for its place in the input
        // layer.
        for (index, (_node, name)) in nodes.iter().enumerate() {
            let _ = writeln!(out, "virtio-input: event{index}: {name}");
        }
        return;
    }
    fail!("no virtio-input window opened and negotiated as an MMIO transport");
}

/// The seventh MMIO device class: the machine's sound card, opened over the
/// memory-mapped transport and installed as `/dev/dsp`.
///
/// The device xtask attaches is a `virtio-sound-device` with an `audiodev`
/// behind it - the same crate driver, the same stream listing, the same OSS
/// parameter face the first machine has, reached through the [`SoundCard`]
/// contract rather than through a second copy of any of it (`PLAN.md` §13.27).
/// This finds the window by `DeviceID`, opens it and hands it to
/// `sound::install_probed`, which is where the stream is brought up.
///
/// The *marker* is [`check_sound_dev`]'s, which reads the node back through the
/// mounted `/dev`; a card that is not there, or answers and will not play, is
/// `M8_SOUND_FAILED` here.
///
/// [`SoundCard`]: mpos::drivers::virtio::SoundCard
fn check_sound(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_SOUND_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_SOUND) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-sound (device {VIRTIO_ID_SOUND}) window");
    }
    for &(virt, size) in &windows {
        let Some(card) = mpos::drivers::virtio::mmio::sound_device(virt, size) else {
            continue;
        };
        // The same line the first machine prints for its own card, so a reader
        // of either boot log reads one story.
        let (rate, channels) = mpos::drivers::virtio::SoundCard::format(&*card);
        let _ = writeln!(
            out,
            "virtio-sound: {rate} Hz, {channels} channels at /dev/dsp"
        );
        return;
    }
    fail!("no virtio-sound window opened and negotiated as an MMIO transport");
}

/// Stage 8-sound's verdict: the card [`check_sound`] installed is the machine's
/// `/dev/dsp`, and its stream really moves bytes.
///
/// Three facts. The card is installed (`sound::global()`), the node resolves
/// through the *mounted* `/dev`, and the stream takes a short burst of silence in
/// full - the data path, not just the config space, which is the one part a node
/// that merely resolves could otherwise fake. Silence on purpose: the backend
/// this machine is booted with consumes the stream and plays nothing, so no boot
/// makes a sound, and a machine whose backend did play one would still be asked
/// for silence and not for noise.
///
/// The OSS *parameter* face - `SNDCTL_DSP_SPEED` and its cousins - is
/// deliberately not exercised here: it belongs to `user/abitest`'s `t-dsp`,
/// which this machine's sweep does not carry, and a boot check that moved the
/// stream's rate would be changing the machine it is checking (§13.14). That is
/// a named gap in this class's coverage, not a claim that the ioctls work.
fn check_sound_dev() {
    let Some(card) = mpos::drivers::virtio::sound::global() else {
        mpos::println!("M8_SOUND_FAILED: no virtio-sound card was installed");
        hal::halt()
    };
    if vfs::resolve("/dev/dsp").is_err() {
        mpos::println!("M8_SOUND_FAILED: /dev/dsp does not resolve through the mounted /dev");
        hal::halt()
    }
    let burst = [0u8; 512];
    let took = mpos::drivers::virtio::SoundCard::play(&*card, &burst);
    if took != burst.len() {
        mpos::println!(
            "M8_SOUND_FAILED: the stream took {took} of {} bytes of silence",
            burst.len()
        );
        hal::halt()
    }
    let (rate, channels) = mpos::drivers::virtio::SoundCard::format(&*card);
    mpos::println!(
        "M8_SOUND_OK: /dev/dsp resolves, the MMIO card runs {rate} Hz / {channels} channels and took a {} byte burst",
        burst.len()
    );
}

/// The eighth MMIO device class: the machine's virtio-vsock device, opened over
/// the memory-mapped transport and handed to the `AF_VSOCK` family.
///
/// The device xtask attaches is a `vhost-vsock-device` on the host's vhost
/// backend, and that is what makes this class unlike the seven before it: vsock
/// is the one virtio device QEMU does *not* emulate in userspace, so there is no
/// window here unless the host carries `/dev/vhost-vsock`. A host without one is
/// therefore a boot with no window on the tree, and that is a line rather than a
/// failure - the family then answers `EAFNOSUPPORT`, which is exactly the refusal
/// the first machine gives when its own PCI probe finds nothing. What is a
/// failure is a window that *does* claim to be a vsock device and will not open,
/// which is `M8_VSOCK_FAILED` and stops the boot honestly.
///
/// This finds the window by `DeviceID`, opens it and hands it to
/// `net::vsock::init`, so from here `socket(AF_VSOCK)`, `listen`, `accept` and
/// the rest are the first machine's family over the second machine's transport -
/// the same shared code the pump in `virtio::spawn_service` keeps moving
/// (`PLAN.md` §13.28). The family answering the same CID the device reported is
/// the whole claim `getsockname` rests on, so it is checked rather than assumed.
fn check_vsock(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_VSOCK_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_VSOCK) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        // No device, no failure: this is the honest absent case, and it is what
        // a host without `/dev/vhost-vsock` produces. The first machine prints
        // the same shape when its bus carries no device.
        let _ = writeln!(
            out,
            "virtio-vsock: no device on the tree; AF_VSOCK answers EAFNOSUPPORT"
        );
        return;
    }
    for &(virt, size) in &windows {
        let Some(dev) = mpos::drivers::virtio::mmio::vsock_device(virt, size) else {
            continue;
        };
        let cid = mpos::drivers::virtio::VsockDevice::guest_cid(&*dev);
        // The same line the first machine prints for its own device, so a reader
        // of either boot log reads one story.
        let _ = writeln!(out, "virtio-vsock: guest cid {cid} answers on AF_VSOCK");
        mpos::net::vsock::init(dev);
        if mpos::net::vsock::guest_cid() != Some(cid as u32) {
            fail!("the AF_VSOCK family did not come up over the device (device cid {cid})");
        }
        let _ = writeln!(
            out,
            "M8_VSOCK_OK: the MMIO virtio-vsock device is the AF_VSOCK family's wire (guest cid {cid})"
        );
        return;
    }
    fail!("a virtio-vsock window exists but none opened and negotiated as an MMIO transport");
}

/// The ninth MMIO device class: the host's 9p share, opened over the
/// memory-mapped transport and handed to the mount layer.
///
/// The device xtask attaches is a `virtio-9p-device` on a `-fsdev` backend - the
/// same crate driver, the same 9P2000 client and the same `mount -t 9p` face the
/// first machine has, reached through the [`P9Device`] contract rather than
/// through a second copy of any of it (`PLAN.md` §13.29). This finds every window
/// whose `DeviceID` is `9` and hands what opens to `fs9p::install`, so from here
/// the shares are in the one registry `by_tag` and `mount_share` read - both
/// machines' shares are the same registry, filled by whatever bus answered.
///
/// The *marker* belongs to [`check_p9_dev`], which reads the mounted share back
/// after the root switch; a window that will not open is `M8_P9_FAILED` here. A
/// machine with no window is a line and not a failure: a share is the host's to
/// offer, and a boot without one is a quieter boot rather than a broken one -
/// the same shape the first machine prints.
///
/// [`P9Device`]: mpos::drivers::virtio::P9Device
fn check_p9(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_P9_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_9P) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        let _ = writeln!(out, "virtio-9p: no share on the tree");
        return;
    }
    let mut found: alloc::vec::Vec<alloc::sync::Arc<dyn mpos::drivers::virtio::P9Device>> =
        alloc::vec::Vec::new();
    for &(virt, size) in &windows {
        if let Some(dev) = mpos::drivers::virtio::mmio::p9_device(virt, size) {
            // The same line the first machine prints for each share it finds, so
            // a reader of either boot log reads one story.
            let _ = writeln!(
                out,
                "virtio-9p: \"{}\" exported over MMIO",
                mpos::drivers::virtio::P9Device::mount_tag(&*dev)
            );
            found.push(dev);
        }
    }
    if found.is_empty() {
        fail!("a virtio-9p window exists but none opened and negotiated as an MMIO transport");
    }
    mpos::drivers::virtio::fs9p::install(found);
}

/// Stage 8-9p's verdict: the share the machine installed is mounted at
/// `/mnt/share`, and a file the *host* laid there reads back through the VFS.
///
/// The mount itself is `steps::mount_share`, the one copy both machines run -
/// called from `devices()` right after `check_p9` filled the registry, so the
/// share hangs under the ramfs `/mnt` that the disk is then mounted *onto*, and
/// the longest-prefix walk reaches it as `/mnt/share` on either side of the
/// script's root switch (`PLAN.md` §15.6). What this adds is the read:
/// `/mnt/share/mpos-9p-probe` is a file xtask writes into the exported directory,
/// so the bytes that come back travelled host -> 9P2000 wire -> `vfs::p9` -> the
/// mount table -> this read. A mount that succeeded but served nothing (`Twalk`
/// or `Tread` refusing, a fid table that never attached) fails here rather than
/// passing on the strength of the mount alone, which is the one thing a share
/// that merely mounted could fake.
fn check_p9_dev() {
    if mpos::drivers::virtio::fs9p::devices().is_empty() {
        return;
    }
    /// What xtask lays in every exported share directory. Written literally
    /// here and in xtask's own writer, the same way the disk image's pattern
    /// sector is: a fixture both sides have to agree on is a fixture with two
    /// copies, and the check's job is to notice if they drift.
    const PROBE: &str = "mpos reads from the host share over 9p\n";
    let probe = match vfs::read_file("/mnt/share/mpos-9p-probe") {
        Ok(bytes) => bytes,
        Err(e) => {
            mpos::println!("M8_P9_FAILED: /mnt/share/mpos-9p-probe did not read: {e:?}");
            hal::halt()
        }
    };
    if probe != PROBE.as_bytes() {
        mpos::println!(
            "M8_P9_FAILED: the share served {} bytes of mpos-9p-probe, not the {} the host laid",
            probe.len(),
            PROBE.len()
        );
        hal::halt()
    }
    mpos::println!(
        "M8_P9_OK: /mnt/share is the host share over MMIO, mpos-9p-probe read back ({} bytes)",
        probe.len()
    );
}

/// The sixth MMIO device class: the machine's virtio-gpu card, opened over the
/// memory-mapped transport and handed to the render task.
///
/// This is the class whose seam was *not* one helper (`PLAN.md` §13.26): the
/// crate's `Transport` is not object-safe, so `Gpu` became generic over it and
/// `video::render` - which owns the one device and is the only context that may
/// call `flush` - holds it as a `Box<dyn GpuDevice>`. What is left for this
/// machine is the part that was always its own: find the window whose
/// `DeviceID` is 16, open it, and let `VirtioGpuDev::install` build the buffer
/// view and publish it for `/dev/fb0`.
///
/// It runs here, in `init` and before [`check_dev`], rather than with the other
/// device checks, for the one reason the card cannot be opened in `devices()`:
/// the render task that owns the device is a *task*, and a task needs a
/// scheduler - which `steps::start_scheduler` has already started by here.
/// Installing before `check_dev` is also what puts `/dev/fb0` in the device set
/// that call builds.
///
/// The marker belongs to [`check_gpu_dev`], which reads the node back through
/// the mounted `/dev`; a window that will not open is `M8_GPU_FAILED` here.
fn check_gpu(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_GPU_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_GPU) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-gpu (device {VIRTIO_ID_GPU}) window");
    }
    for &(virt, size) in &windows {
        let Some((view, device)) = mpos::drivers::virtio::mmio::gpu_device(virt, size) else {
            continue;
        };
        // The same line the first machine prints for its own card, so a reader
        // of either boot log reads one story.
        let (width, height) = view.dims();
        let _ = writeln!(
            out,
            "virtio-gpu: {width}x{height}, {} KiB at /dev/fb0",
            view.fb_len() / 1024
        );
        // The task that owns the device and is the only caller of `flush`. It
        // is permanent: a writer parked in `write(2)` depends on it for as long
        // as the machine runs.
        mpos::drivers::video::render::spawn(device, view);
        return;
    }
    fail!("no virtio-gpu window opened and negotiated as an MMIO transport");
}

/// Stage 8-gpu's verdict: the card [`check_gpu`] installed is the machine's
/// `/dev/fb0`, read back the way a program reads it.
///
/// Three facts. The card is installed (`gpu::global()`), the node resolves
/// through the *mounted* `/dev` (the same walk a userspace `open` makes), and
/// one row of pixels comes back whole - which is the only honest way to say the
/// node's buffer *is* the device's: a node whose length was a claim rather than
/// a mapping would short-read here.
///
/// The render task is asserted running with them, and that is the part a class
/// with no compositor could otherwise fake: `write(2)` to this node parks until
/// that task has flushed, so a card installed without it is a node a writer
/// blocks on for ever. What is deliberately *not* asserted is a pixel *value*:
/// the buffer holds whatever the host last put there, and a boot has no way to
/// say what that is.
fn check_gpu_dev() {
    let Some(view) = mpos::drivers::virtio::gpu::global() else {
        mpos::println!("M8_GPU_FAILED: no virtio-gpu card was installed");
        hal::halt()
    };
    let Ok(fb) = vfs::resolve("/dev/fb0") else {
        mpos::println!("M8_GPU_FAILED: /dev/fb0 does not resolve through the mounted /dev");
        hal::halt()
    };
    if !mpos::drivers::video::render::running() {
        mpos::println!(
            "M8_GPU_FAILED: the render task is not running, so a write to /dev/fb0 could never be flushed"
        );
        hal::halt()
    }
    let (width, height) = view.dims();
    let frame = (width as usize) * (height as usize) * 4;
    if width == 0 || height == 0 || view.fb_len() < frame {
        mpos::println!(
            "M8_GPU_FAILED: the card reports {width}x{height} in {} bytes, which cannot hold one frame",
            view.fb_len()
        );
        hal::halt()
    }
    let mut row = alloc::vec![0u8; width as usize * 4];
    match fb.read_at(0, &mut row) {
        Ok(n) if n == row.len() => {}
        other => {
            mpos::println!(
                "M8_GPU_FAILED: reading one row of /dev/fb0 gave {other:?}, expected {} bytes",
                row.len()
            );
            hal::halt()
        }
    }
    mpos::println!(
        "M8_GPU_OK: the MMIO virtio-gpu card drives /dev/fb0 at {width}x{height} ({} KiB) and the render task owns it",
        view.fb_len() / 1024
    );
}

/// The fifth MMIO device class: the machine's virtio console port, opened over
/// the memory-mapped transport and installed as the console userland opens.
///
/// The device xtask attaches is a `virtio-serial-device` carrying one
/// `virtconsole` port - the same shape the first machine attaches on its PCI
/// bus - and its far end is a host socket nothing has to be listening on. This
/// finds the window by `DeviceID`, opens it, and hands it to
/// `console::install`, so from here the port *is* the machine's `/dev/hvc0`: the
/// devfs build below publishes it and the stream face reads and writes through
/// it. The write face is exercised here, as a probe line the port's queue has to
/// take; that claims nothing about a reader, because a socket nobody has
/// connected to takes bytes the way a port with no reader does.
///
/// The *marker* is deliberately not printed here. `M8_CONSOLE_OK` belongs to
/// [`check_console_dev`], which resolves the node through the mounted `/dev` -
/// and this runs in `devices()`, before there is a `/dev` to resolve anything
/// through. A window that will not open is `M8_CONSOLE_FAILED` (a terminator)
/// and stops the boot honestly, at the point it happens.
fn check_console(info: &BootInfo) {
    use fmt::Write;

    let mut out = MachineWriter;

    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = writeln!(out, "M8_CONSOLE_FAILED: {}", format_args!($($arg)*));
            hal::halt();
        }};
    }

    let windows = match virtio_device_windows(info, VIRTIO_ID_CONSOLE) {
        Ok(windows) => windows,
        Err(message) => fail!("{message}"),
    };
    if windows.is_empty() {
        fail!("the tree describes no virtio-console (device {VIRTIO_ID_CONSOLE}) window");
    }
    for &(virt, size) in &windows {
        let Some(port) = mpos::drivers::virtio::mmio::console_device(virt, size) else {
            continue;
        };
        let port = mpos::drivers::virtio::console::install(port);
        let probe = b"mpos: virtio-console over MMIO\n";
        if !mpos::drivers::virtio::ConsolePort::send(&*port, probe) {
            fail!("the virtio-console port at {virt:#x} did not take a probe line");
        }
        match mpos::drivers::virtio::ConsolePort::size(&*port) {
            // The same two lines the first machine prints for its own port, so a
            // reader of either boot log reads one story.
            Some((columns, rows)) => {
                let _ = writeln!(out, "virtio-console: {columns}x{rows} at /dev/hvc0");
            }
            None => {
                let _ = writeln!(out, "virtio-console: up at /dev/hvc0");
            }
        }
        return;
    }
    fail!("no virtio-console window opened and negotiated as an MMIO transport");
}

/// Stage 8-console's verdict: the port [`check_console`] installed is the
/// machine's `/dev/hvc0`, read back the way a program opens it.
///
/// Two facts. The port is installed (`console::global()`, so the devfs build had
/// something to publish), and both names it answers to resolve through the
/// *mounted* `/dev` - `hvc0`, and the `hvc` alias Linux keeps for "the first
/// one" - which is the same walk a userspace `open` makes.
///
/// It stands here, after [`check_dev`], for that reason and no other: the node
/// exists only once the boot has mounted the filesystem that carries it. What it
/// deliberately does *not* assert is the node's device number: `DevFs::register`
/// gives a `CharDevice` `(0, 0)`, on both machines, so there is no number here to
/// pin - named in `PLAN.md` §13.25 rather than papered over with an assertion
/// that would pass by accident.
fn check_console_dev() {
    if mpos::drivers::virtio::console::global().is_none() {
        mpos::println!("M8_CONSOLE_FAILED: no virtio-console port was installed");
        hal::halt()
    }
    for path in ["/dev/hvc0", "/dev/hvc"] {
        if vfs::resolve(path).is_err() {
            mpos::println!("M8_CONSOLE_FAILED: {path} does not resolve through the mounted /dev");
            hal::halt()
        }
    }
    mpos::println!("M8_CONSOLE_OK: the MMIO console port is installed, /dev/hvc0 resolves");
}

/// Stage 8-input's verdict: the registry [`check_input`] filled is the machine's
/// input stack, read back the way a program reads it.
///
/// Three facts, and each is a way for the class to be absent rather than present.
/// The registry is not empty (`input::nodes()`, so the device that opened also
/// published a node); the node resolves through the *mounted* `/dev`
/// (`vfs::resolve`, the same walk a userspace `open` makes, so the devfs build
/// published it and the mount reaches it); and it reports evdev's own device
/// number (`rdev()`, the `13:64` pair udev keys its database by and libinput
/// finds the device through - a node reporting `0,0` is a node no input stack
/// can see, however well its events read).
///
/// It runs here, after [`check_dev`], and that placement is the point: `/dev`
/// only exists once the boot has mounted it, so the registry can be read through
/// a path only from this side of that call. Event flow is not asserted and could
/// not be - it needs a keystroke and the harness is headless - so what the marker
/// pins is the registry and the node, not a record.
fn check_input_registry() {
    let nodes = mpos::drivers::virtio::input::nodes();
    if nodes.is_empty() {
        mpos::println!("M8_INPUT_FAILED: the evdev registry holds no device");
        hal::halt()
    }
    let Ok(event0) = vfs::resolve("/dev/input/event0") else {
        mpos::println!(
            "M8_INPUT_FAILED: /dev/input/event0 does not resolve through the mounted /dev"
        );
        hal::halt()
    };
    let want = (
        mpos::drivers::virtio::input::INPUT_MAJOR,
        mpos::drivers::virtio::input::input_minor(0),
    );
    match event0.rdev() {
        Some(rdev) if rdev == want => {}
        other => {
            mpos::println!(
                "M8_INPUT_FAILED: /dev/input/event0 reports {other:?}, expected evdev's {want:?}"
            );
            hal::halt()
        }
    }
    mpos::println!(
        "M8_INPUT_OK: the evdev registry holds {} device(s), /dev/input/event0 resolves as {want:?}",
        nodes.len()
    );
}
/// Stage 7-dev's evidence: the machine builds the device set a real init mounts
/// as `/dev` and reads a working node out of it.
///
/// `drivers::devices::standard_devfs()` is exactly what `mount -t devtmpfs /dev`
/// hands back - the dispatch arm this stage opened to aarch64 - and it is built
/// from the console tty, so it can only run once `vfs::tty::init()` has filled
/// the tty table. That is why it is here rather than an early boot milestone: a
/// check that reached for the tty before init registered it panicked in
/// `tty::global()` (the measured `PLAN.md` §9.1 `/dev` finding).
///
/// It reads two nodes through the trait surface (`root -> lookup -> read_at`), the
/// same moves an `open`/`read` make: `/dev/urandom` (the pool `rand` seeded from
/// the MMIO rng) and `/dev/hwrng` (the MMIO rng itself, reached through the same
/// `fill` `check_rng` used), proving the generic set is present and entropy flows
/// on this machine. The virtio-PCI `hvc`/`dsp`/`fb0` and the DRM card are gated
/// out here (the machine has no path to them yet), so the set is the honest
/// subset, not a stub of the full x86_64 tree; `/dev/input` and `uinput` are in
/// it now, because the machine has the registry behind them (`PLAN.md` §15.2).
fn check_dev() {
    let dev = mpos::drivers::devices::standard_devfs();
    // Mount it. This mirrors what Linux's `CONFIG_DEVTMPFS_MOUNT` does: the
    // kernel creates `/dev` on the root and mounts its devtmpfs there, so a
    // userspace init finds the device set already in place rather than having
    // to build it. The ramfs root carries no `/dev` yet (the aarch64 initramfs
    // is only the hand-encoded ELF programs), so the mountpoint is created here
    // first - exactly the dir an `mknod`/`init` would have had to make anyway.
    let vfs = mpos::vfs::global();
    let _ = vfs.root_inode().create("dev", mpos::vfs::NodeKind::Dir);
    if let Err(e) = vfs::mount("/dev", dev.clone()) {
        mpos::println!("M7_DEV_FAILED: mounting devfs at /dev gave {e:?}");
        hal::halt()
    }

    // Read two nodes through the *live mount table* (`resolve` is the same walk
    // a userspace `open` makes), proving `/dev` is genuinely on the filesystem -
    // not merely that `standard_devfs()` constructs. `/dev/urandom` fills the
    // whole buffer (it neither blocks nor short-reads), so a 16-byte ask must
    // come back with 16 bytes.
    let Ok(urandom) = vfs::resolve("/dev/urandom") else {
        mpos::println!("M7_DEV_FAILED: /dev/urandom does not resolve through the mount table");
        hal::halt()
    };
    let mut buf = [0u8; 16];
    match urandom.read_at(0, &mut buf) {
        Ok(n) if n == buf.len() => {}
        other => {
            mpos::println!("M7_DEV_FAILED: reading /dev/urandom gave {other:?}, expected 16 bytes");
            hal::halt()
        }
    }
    // `/dev/hwrng` is the entropy device itself - the MMIO virtio-rng `check_rng`
    // installed - reached through the same `fill`. A healthy device answers a read
    // with the whole buffer, so an 8-byte ask must come back with 8 host-generated
    // bytes. Both resolve and read through the mounted tree, not the devfs root.
    let Ok(hwrng) = vfs::resolve("/dev/hwrng") else {
        mpos::println!("M7_DEV_FAILED: /dev/hwrng does not resolve through the mount table");
        hal::halt()
    };
    let mut hbuf = [0u8; 8];
    match hwrng.read_at(0, &mut hbuf) {
        Ok(n) if n == hbuf.len() => mpos::println!("M7_DEV_OK"),
        other => {
            mpos::println!("M7_DEV_FAILED: reading /dev/hwrng gave {other:?}, expected 8 bytes");
            hal::halt()
        }
    }
}

/// Stage 8's live network: the shared smoltcp stack over this machine's adapter.
///
/// The mirror of x86_64's `init_net`, and deliberately the same two calls. The
/// difference is only where the device comes from: there the PCI bus is probed
/// (`drivers::virtio::net::VirtioNet::init`), here the adapter is the MMIO window
/// `check_net` found in the device tree and *published* (`mmio::net_device`) -
/// which is why the ring that check posted is the ring the stack's first frame
/// arrives on, rather than a second open of the same window.
///
/// The marker this prints is narrow on purpose. It says the adapter was handed to
/// `net::init`, the two interfaces were built over it (eth0 at the address the
/// stack configures, plus smoltcp's own loopback), and the service task that polls
/// them was spawned. It does **not** claim a round trip: traffic is proven by the
/// programs that drive the stack - `/t-socket`, `/t-udp-loopback` and the rest on
/// this machine's sweep - because a kernel-side self-test ping is exactly the hook
/// this project keeps out of `net` (job 4's pin is a real EL0 program).
///
/// A machine with no adapter is not a failure here: the absence is printed, the
/// stack is never initialized, and the socket syscalls answer `ENETDOWN`, which is
/// the honest answer for a machine without a card. (`check_net` having failed is a
/// different thing entirely, and stops the boot.)
fn init_net() {
    let Some(dev) = drivers::virtio::mmio::net_device() else {
        mpos::println!("net: no virtio-net window opened; sockets answer ENETDOWN");
        return;
    };
    steps::bring_up_net(dev);
    mpos::println!("M8_NET_STACK_OK");
}

/// The interrupt-driven console: the machine queues what it writes and the
/// UART's own interrupt hands it to the wire, rather than the writer waiting for
/// the transmitter with interrupts masked.
///
/// The queue is not this machine's - it is `drivers::tx_ring`, which the first
/// machine's 16550 drains through the same three operations this machine's PL011
/// implements - so what is being pinned is narrower and more useful than "output
/// appears": it is that *this* machine's transmitter answer reaches it, that the
/// GIC delivers its line, and that a writer which offered more than one burst
/// was parked and woken rather than spinning. The last of those is why the check
/// asks for more than a line: [`check_console_rx`] covered a byte, and a queue
/// that only ever held one line's worth would pass a test that small.
///
/// How it knows the difference. `stats()` counts what writers queued and what the
/// *handler* handed over, and the second number can only rise inside an
/// interrupt: a polled write moves no bytes through the queue at all, so a
/// machine whose line never fired reports a queue that nothing drains. It is
/// also the check that catches a transmitter asked to interrupt when it should
/// not have been - an idle line asserting for ever would have shown up here as
/// visits that moved nothing, and before there was a counter to ask, as nothing
/// at all.
fn check_serial_tx() {
    const LINE: usize = 512;

    let before = hal::tx_ring::stats();
    // The write goes through `crate::console` rather than straight to the wire,
    // because that is the path every line above the machine layer takes - and
    // because its parking half only engages for a caller that may sleep, which
    // this task may.
    let filler: alloc::string::String = core::iter::repeat_n('.', LINE).collect();
    mpos::println!("serial tx: {LINE} bytes through the transmit ring{filler}");

    // Wait for the handler to have handed them over. The wait is bounded, and
    // the bound is ticks rather than a count of its own: a machine whose clock
    // stopped is a machine nothing here can finish anyway.
    let mut waited = 0;
    loop {
        if hal::tx_ring::stats().moved >= before.moved + LINE {
            break;
        }
        waited += 1;
        if waited > 200 {
            // Reported through the *machine's* polled writer, not `println!`:
            // a failure here is by definition a queue that is not draining, and
            // a report filed through that same queue is a report nobody reads.
            let now = hal::tx_ring::stats();
            mpos::arch::print_args(format_args!(
                "\nM5_SERIAL_TX_FAILED: {} bytes queued but only {} handed to the PL011 \
                 ({waited} ticks, no progress)\n",
                now.queued - before.queued,
                now.moved - before.moved
            ));
            hal::halt()
        }
        sched::sleep_ticks(1);
    }
    let now = hal::tx_ring::stats();
    let moved = now.moved - before.moved;
    let visits = now.visits - before.visits;
    if visits == 0 {
        mpos::arch::print_args(format_args!(
            "\nM5_SERIAL_TX_FAILED: the bytes went out without a single interrupt\n"
        ));
        hal::halt()
    }
    mpos::println!(
        "serial tx: {moved} bytes handed to the PL011 by {visits} interrupt(s), \
         ({waited} ticks elapsed)"
    );
    mpos::println!("M5_SERIAL_TX_OK");
}

/// Stage 7b's evidence: the polled console input reaches a reader through the
/// `hal::ConsoleInput` capability, not just the raw UART.
///
/// `check_serial_rx` in `arch` already proved `pl011::read_byte` reads the FIFO
/// (loopback). What this proves is the seam *above* the machine layer: a byte put
/// in the receiver by [`hal::console_inject_input`] comes back out of
/// [`hal::console_poll_input`] and, handed to the tty, is readable through the
/// tty's own `read` - the exact moves the pump makes at runtime, driven here by
/// loopback so a headless boot can exercise it. Runs before the pump task is
/// spawned, so the poll here is the only consumer of the injected bytes.
fn check_console_rx() {
    // Feed a line: a probe byte and a newline, each injected into the receiver and
    // pulled back out through the capability forward - the exact moves the pump
    // makes at runtime, driven here by loopback so a headless boot can exercise it.
    let mut typed = [0u8; 2];
    typed.copy_from_slice(b"Z\n");
    let tty = vfs::tty::global();
    for &byte in &typed {
        hal::console_inject_input(byte);
        match hal::console_poll_input() {
            Some(got) if got == byte => tty.receive_byte(got),
            other => {
                mpos::println!(
                    "M6_CONSOLE_RX_FAILED: poll after inject gave {other:?}, not {byte:#04x}"
                );
                hal::halt()
            }
        }
    }

    // Read it back through the tty the way a shell would read `fd 0`. A canonical
    // tty returns a whole line once the newline has landed.
    let mut line = [0u8; 8];
    match tty.read(&mut line) {
        Ok(n) if &line[..n] == b"Z\n" => mpos::println!("M6_CONSOLE_RX_OK"),
        other => {
            mpos::println!("M6_CONSOLE_RX_FAILED: tty read gave {other:?}, expected \"Z\\n\"");
            hal::halt()
        }
    }
}

/// Stage 4b's evidence: `crate::console` - un-gated for this machine one stage ago -
/// actually reaches the wire. Every boot line before this one was `arch`'s own
/// `pl011::Writer`; this is the first through the console service (`println!` ->
/// `print_args` -> the `hal::Console` sink -> the PL011). If the service were
/// mis-wired on this machine the milestone line simply would not appear, and
/// `cargo xtask boot-aarch64` asserts on its presence.
///
/// The record goes unused here, unlike the first machine's counterpart: that one
/// reads the command line for its `console=` list, and this machine has the one
/// console, so there is nothing in the list to decide.
fn milestone(info: &BootInfo) {
    // Bring the service's sink up (idempotent over the PL011 the boot already
    // poked) and send one line, then the marker, through the same path a
    // `println!` anywhere above the machine layer will use.
    mpos::console::init();
    mpos::println!("console: kernel log reached the PL011 through crate::console");
    mpos::println!("M4_CONSOLE_OK");
    // And the record, on the same wire and at the same point the first machine
    // prints it: the shared banner, one line per fact the loader handed over,
    // before anything below acts on any of them (`board::report`).
    report::banner(info);
}
