//! The x86_64 kernel: what this machine's boot does once it is up, and what it
//! accepts as proof that it works.
//!
//! The *machine* half of the boot - the entry, SSE for user processes, the boot
//! record, physical memory, the descriptor tables, the kernel's own page tables
//! and the wire that reports them - is `mpos_arch::arch::x86_64::boot`, because
//! that is the machine layer's business and a machine crate is the only place
//! that may touch it (`NOTES.md` §14.8). What is left here is the kernel's own
//! boot: the subsystems on top of that machine, the devices on its bus, and the
//! milestone checks. None of it could live in `mpos-arch`, because all of it
//! names `vfs`, `blk`, `net` and `drivers`, which sit above the machine layer.
//!
//! `src/main.rs` enters this file through [`start`], and the arch crate calls
//! back into [`milestone`], [`devices`] and [`init`] at the three points where
//! the machine layer needs the kernel to act.
//!
//! # Boot flow
//!
//! 1. **Initialization** — `init_mm → init_sched → init_vfs → init_blk →
//!    init_net → init_virtio → init_rand`. Each sets up a subsystem without
//!    running tests. (The machine's own bring-up, `init_arch`'s
//!    interrupt/page-table half, is what `arch::boot` has already done by the
//!    time this file runs.) After all inits complete, the system is ready.
//!
//! 2. **Milestone pre-checks** — `check_mm → check_arch → check_sched →
//!    check_vfs → check_ipc`. Each validates the corresponding subsystem.
//!    Groups are ordered by dependency and print `M*_OK` markers the CI
//!    harness asserts on. (`check_blk_process` and `check_shell` are covered
//!    by the block layer's own checks and by the init handoff respectively.)
//!
//! 3. **Init handoff** — `handoff_to_init` spawns `/sbin/init` and parks the
//!    boot task.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use super::report;
use super::steps;
use mpos::blk;
use mpos::boot::console_list::ConsoleList;
use mpos::boot::{self, BootInfo};
use mpos::drivers;
use mpos::mm;
use mpos::println;
use mpos::sched::{self};
use mpos::vfs::{self, NodeKind};

// What only the test build's suites reach for. A `use` is not a cost, but an
// unused one is a warning, and the warning is the *right* one here: it is the
// compiler saying that this name left the shipping kernel with the check that
// needed it (`src/board/mod.rs` says where the checks went).
#[cfg(feature = "selftest")]
use super::kunit;
#[cfg(feature = "selftest")]
use core::ptr;
#[cfg(feature = "selftest")]
use mpos::arch;
#[cfg(feature = "selftest")]
use mpos::case_check;
#[cfg(feature = "selftest")]
use mpos::hal;
#[cfg(feature = "selftest")]
use mpos::mm::{MapFlags, Mapping, PAGE_SIZE};
#[cfg(feature = "selftest")]
use mpos::syscall::{self};

const MIB: usize = 1024 * 1024;

/// The data partition device (partition 4), mounted at /mnt by `init_blk`.
/// Kept for the checks that need the raw block device rather than the mounted
/// tree (e.g. remounting to prove a write reached the disk).
static DATA_DEV: spin::Once<alloc::sync::Arc<dyn mpos::blk::BlockDev>> = spin::Once::new();

// ---------------------------------------------------------------------------
// The machine layer's entry and its three callbacks
// ---------------------------------------------------------------------------

/// Run this machine's kernel from the loader's record. Never returns.
///
/// `src/main.rs` hands this to `mpos_arch::arch::boot` as the entry, so this is
/// where the kernel's own boot starts on x86_64: by the time it runs, the machine
/// layer has enabled SSE, read the record, brought the console up, started
/// physical memory, configured interrupts and installed the kernel's own page
/// tables.
///
/// # Safety
///
/// The caller is the loader's branch into `_start`, with `RDI` holding the record
/// it wrote, and every address that record names mapped for as long as the kernel
/// runs. Nothing chooses this call from Rust.
pub unsafe fn start(handoff: *const boot::handoff::Handoff) -> ! {
    // SAFETY: the caller's contract, restated in `arch::x86_64::boot`'s own
    // `# Safety` - the record handed over is the one the loader wrote, and the
    // machine layer's translation of it is what checks the magic.
    unsafe { mpos::arch::boot(handoff, milestone, devices, init) }
}

/// Bring up the kernel console, and say what was booted.
///
/// Called by the machine layer as soon as it has read the record: nothing above
/// this can allocate yet, so this is the console service, the console list, and
/// the banner - and nothing else. The list comes from the command line because a
/// device-presence test cannot answer the question: a QEMU booted without a window
/// still has a virtio-gpu and still reports a display mode, while the command line
/// is the one place the answer actually is.
fn milestone(info: &BootInfo) {
    mpos::console::init();
    mpos::console::set_console_list(ConsoleList::from_cmdline(info.cmdline, |unknown| {
        println!("console={unknown}: no such console here, ignored");
    }));
    report::banner(info);
}

/// The machine's devices, once the kernel's own page tables reach them.
///
/// On this machine that is the PCI bus, and walking it is deliberately the first
/// thing done with a device: describing a BAR means asking the device what it
/// decodes, which means writing all ones into the register and reading back what
/// it answers. Done here, before any driver has claimed anything, the only
/// registers disturbed are ones nothing has used yet - and every one of them is
/// written back. Done later, a device could be asked to answer for an address
/// nobody assigned it while a driver was mid-transfer.
///
/// What it publishes is what `/sys` and `/proc` describe the machine from. The
/// *drivers* come after, in [`init`], and record themselves with
/// `drivers::pci::claim`, so `lspci -k` names a driver that really did bind rather
/// than one that merely matches.
///
/// The record is used for one thing here, and it is the same thing the second
/// machine reads its device tree for: where the firmware wrote down what it
/// knows. This machine's firmware describes itself with ACPI, so the record
/// carries the physical address of the root pointer, and the bus walk asks it
/// where the memory-mapped configuration window is before falling back to the
/// port mechanism.
fn devices(info: &BootInfo) {
    let count = drivers::pci::init(info);
    println!("pci: {count} functions enumerated");
}

// ---------------------------------------------------------------------------
// Module-based milestone checks
// ---------------------------------------------------------------------------

/// The kernel's own boot: prove the kernel works, then hand the machine to init
/// and stand aside. It ends inside `handoff_to_init`, which parks the boot task
/// rather than returning - a system that stops running itself is not a system.
///
/// Handed to the machine layer as its `init` callback, so it runs on the kernel's
/// own page tables, with the console up, the record already turned into
/// `BootInfo`, and the machine's bus published.
///
/// The steps it shares with the other port are calls into the sibling `steps` -
/// the scheduler, the bootstrap root, the console arming and the pid-1 handoff -
/// and everything between them is this machine's own: PCI enumeration, the block
/// devices behind it, the data partition, the 9p share, and the M0..M8 checks.
fn init(info: &BootInfo) -> ! {
    // Logging comes up as soon as the heap does, because the subscriber it
    // installs allocates and the level it prints at is read from the command
    // line. Everything after this point is worth being able to turn on or off.
    mpos::logging::init();

    // The self-tests are a *build* rather than a word on the command line: this
    // kernel carries them only when it was built for them (`--features selftest`,
    // which `cargo xtask test` passes), so a kernel that ships has no test branch
    // here to pay for and none left open for one. What the build still has to say
    // is *when* each suite runs - the boot step it needs - which is what the
    // `Phase` names (`src/board/kunit.rs`), and why each marker below stands
    // directly after the step it depends on rather than all of them at the end.
    init_mm();
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Heap, info);

    // The machine's interrupts, descriptor tables and page tables are already up
    // - `arch::boot` installed them before calling here - so what the arch suite
    // proves needs no `init_arch` in front of it.
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Arch, info);

    steps::start_scheduler();
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Scheduler, info);

    init_vfs(info);
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Vfs, info);

    init_blk();
    init_net();
    init_virtio();
    steps::register_exit_hooks();

    // The machine's rng device, if the PCI enumeration found one, before the
    // generator seeds: `rand` folds in "this machine's rng device" and is not
    // allowed to know what that is or which bus carried it (`NOTES.md` §14.3).
    mpos::rand::set_device_entropy(drivers::virtio::entropy);
    mpos::rand::init();

    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Ipc, info);
    #[cfg(feature = "selftest")]
    kunit::reached(kunit::Phase::Done, info);

    println!("All ready!");

    handoff_to_init(info)
}

// ---------------------------------------------------------------------------
// Phase 1: initialization — set up subsystems, no tests
// ---------------------------------------------------------------------------

fn init_mm() {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend(b"hello from mpos".iter().copied());
    buffer.push(b'!');

    let text = String::from_utf8(buffer).expect("valid utf8");
    println!("{}", text);

    let heap = mm::heap_stats();
    println!(
        "heap: {} B live, {} B cached, {} B mapped",
        heap.live, heap.cached, heap.mapped
    );
}

fn init_vfs(info: &BootInfo) {
    steps::mount_ramfs_root();

    // The archive arrives in the record here rather than as a slice the machine
    // layer stashed: this machine's root switch happens in the initramfs's own
    // `/init` script, so nothing between the loader and this call has thrown the
    // record's pointer away.
    let archive = info.initramfs.expect("the boot config loaded no initramfs");
    steps::unpack(archive);

    // No /dev here. The kernel used to build one and mount it on the temporary
    // root, which meant it existed before the block layer did - so it had no
    // disk in it, and `/init` mounting its own got `EBUSY` and kept the crippled
    // one. Devices belong to whoever mounts /dev, and on this machine that is
    // the initramfs's own init script.
}

fn init_blk() {
    // `/mnt` exists whether or not anything is mounted on it: it is where the
    // initramfs's own init puts the data partition when the image has one, and a
    // directory is cheaper than making a userland mount point conditional on the
    // kernel's opinion of the layout.
    let root = vfs::resolve("/").expect("no root filesystem");
    if root.lookup("mnt").is_err() {
        root.create("mnt", NodeKind::Dir).expect("create /mnt");
    }

    // A machine with no disk is a machine that boots and has nothing to mount -
    // the diskless path the boot used to take when the command line named no
    // root. Nothing below this can run without a device to read, and none of it
    // is load-bearing for a kernel that is going to sit in the initramfs.
    // The boot disk, from whichever controller has one. Both are tried rather
    // than either: a machine can have the paravirtual disk the image builder
    // attaches *and* a real SATA controller, and then it has two disks under two
    // names (`NOTES.md` §7.3). The first one brought up is the boot disk, whose
    // partitions the early userland mounts.
    if let Some(dev) = drivers::virtio::blk::VirtioBlk::init() {
        let sectors = dev.capacity_sectors();
        println!(
            "virtio-blk: vda, {} sectors ({} MiB)",
            sectors,
            sectors * 512 / MIB as u64
        );
        // The name, the major and the tree position are the *driver's* facts,
        // said once where the driver is brought up: a virtio disk is `vda` under
        // major 254 in the virtio tree, a SATA disk is `sda` under major 8 in the
        // SATA one. Everything that names a disk to userland reads them from
        // there rather than deciding for itself (`uevent::Numbering`) - and the
        // virtio one is a single call because both machines register the same
        // disk and only the transport differs.
        drivers::virtio::register_block_disk(dev);
    }
    if let Some(dev) = drivers::ahci::init() {
        let sectors = dev.capacity_sectors();
        println!(
            "ahci: sda, {} sectors ({} MiB)",
            sectors,
            sectors * 512 / MIB as u64
        );
        blk::add_disk(
            "sda",
            mpos::uevent::SCSI_DISK_MAJOR,
            "/devices/ahci/block",
            dev,
        );
    }
    let Some(dev) = blk::raw_disk() else {
        println!("blk: no disk answered on the PCI bus, so there is nothing to mount");
        return;
    };

    // The data partition is a second disk filesystem - ext2 or ext4, whichever
    // the image builder made; the same driver reads both - and it is the only thing
    // on the disk the kernel reads for its own sake rather than to hand to
    // userland - seed files laid down by the image builder, so that a check can
    // read something the machine did not write. Not every layout has room for
    // one: a UEFI image is an ESP and a root filesystem and nothing else, and
    // asking for a fourth partition there would be asking the disk to carry a
    // filesystem nothing on it uses. So its absence is a quieter boot, not a
    // failed one.
    let Some(data) = blk::part::find_data(dev) else {
        println!("blk: no data partition on this disk, so /mnt stays empty");
        return;
    };
    let fs: alloc::sync::Arc<dyn vfs::FileSystem> = alloc::sync::Arc::new(
        vfs::ext4::Ext4Fs::mount(data.clone()).expect("the data image did not mount"),
    );
    // What it is on disk is the filesystem's own answer: ext2 and ext4 take
    // different names from the same driver.
    println!(
        "{}: {}-sector partition",
        fs.name(),
        data.capacity_sectors()
    );
    vfs::mount("/mnt", fs).expect("mount /mnt");
    DATA_DEV.call_once(|| data);
}

// ---- arch: interrupts, timer, paging, address spaces ---------------------

/// The arch suite's first case: the clock ticks at the rate it declares, a
/// breakpoint is catchable, and the machine's register-file table agrees with the
/// host-built ABI table.
///
/// Compiled only into a test build: it costs half a second of busy-waiting, and
/// nothing outside the suite registry calls it.
#[cfg(feature = "selftest")]
pub(crate) fn check_arch_clock(_info: &BootInfo) -> Result<(), String> {
    // The clock, and a breakpoint exception.
    {
        let before = hal::ticks();
        hal::busy_wait_ms(500);
        let elapsed = hal::ticks() - before;
        println!("clock: {} ticks in ~500ms ({} Hz)", elapsed, hal::TICK_HZ);
        // Expect half a second's worth of ticks, within ten percent. Written
        // against TICK_HZ, not a literal, so raising the tick rate does not
        // silently turn a working clock into a failed assertion.
        let half = hal::TICK_HZ as u64 / 2;
        let tol = hal::TICK_HZ as u64 / 10;
        case_check!(
            (half - tol..=half + tol).contains(&elapsed),
            "the clock ticked {elapsed} times in ~500 ms, not the {half}±{tol} that {} Hz promises",
            hal::TICK_HZ
        );
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("int3")
        };
        case_check!(
            arch::breakpoints_caught() == 1,
            "#BP was not caught: {} breakpoints recorded",
            arch::breakpoints_caught()
        );
        println!("breakpoint caught and returned from");

        // The machine's copy of the Linux register-file field order must agree
        // with the ABI's. The two tables are duplicated on purpose - one side is
        // bare-metal only, the other is host-built, so neither can name the
        // other's module - and this is what keeps the duplication honest rather
        // than hoped-for (`arch/x86_64/context.rs`, `syscall/linux/abi.rs`).
        for (name, index) in arch::ABI_REG_INDEX {
            case_check!(
                syscall::linux::abi::uregs_by_name(name) == Some(index),
                "the machine puts {name} at {index} and user_regs_struct puts it at {:?}",
                syscall::linux::abi::uregs_by_name(name)
            );
        }
        case_check!(
            syscall::linux::abi::UREGS_COUNT == arch::ABI_REG_COUNT,
            "the register file is {} fields on one side and {} on the other",
            syscall::linux::abi::UREGS_COUNT,
            arch::ABI_REG_COUNT
        );
        println!("register file field order agrees with the ABI");
    }
    Ok(())
}

/// The arch suite's second case: the kernel runs on its own page tables with the
/// permissions it meant, the direct map is usable, and processes get isolated
/// address spaces.
///
/// Compiled only into a test build, like the case above.
#[cfg(feature = "selftest")]
pub(crate) fn check_arch_pages(info: &BootInfo) -> Result<(), String> {
    // The kernel's own page tables, W^X, and user address spaces.
    {
        let sections = arch::kernel_sections();

        // Direct map works after the switch.
        let mut proof: Vec<u8> = Vec::new();
        proof.resize(64 * 1024, 0x3c);
        case_check!(
            proof.iter().all(|&byte| byte == 0x3c),
            "the direct map is broken after the switch: a 64 KiB buffer did not read back"
        );
        println!("64 KiB allocated and written after the switch");

        // W^X across the whole image.
        let image_offset = info.kernel_virt_base - info.kernel_phys_base;
        for (name, range, expect) in [
            ("text", &sections.text, MapFlags::READ | MapFlags::EXEC),
            ("rodata", &sections.rodata, MapFlags::READ),
            ("data", &sections.data, MapFlags::READ | MapFlags::WRITE),
        ] {
            let mut virt = range.start;
            while virt < range.end {
                let (phys, flags) = hal::translate(virt)
                    .ok_or_else(|| alloc::format!("{name} at {virt:#x} is not mapped"))?;
                case_check!(
                    flags == expect,
                    "{name} at {virt:#x} is {flags:?}, not the {expect:?} it was mapped with"
                );
                case_check!(
                    virt - phys == image_offset,
                    "{name} at {virt:#x} points at offset {:#x}, not the image's {image_offset:#x}",
                    virt - phys
                );
                virt += PAGE_SIZE;
            }
        }
        println!("text r-x, rodata r--, data rw- across the whole image");

        // Direct map is writable; null page is unmapped.
        let frame = mm::alloc_frames(0).expect("out of frames");
        let (back, flags) = hal::translate(info.phys_to_virt(frame))
            .expect("the direct map is missing a frame the kernel owns");
        case_check!(
            back == frame,
            "the direct map translates frame {frame:#x} to {back:#x}"
        );
        case_check!(
            flags == MapFlags::READ | MapFlags::WRITE,
            "the direct map is {flags:?}, not read-write"
        );
        mm::free_frames(frame, 0);

        case_check!(hal::translate(0).is_none(), "the null page is mapped");
        println!("direct map writable and non-executable, null page unmapped");

        // Kernel stack is not reclaimable.
        let probe = 0u8;
        let stack = mm::virt_to_phys(&raw const probe as usize);
        let region = info
            .regions()
            .iter()
            .find(|r| r.base <= stack && stack < r.end())
            .ok_or_else(|| alloc::format!("the stack at {stack:#x} is in no memory-map region"))?;
        case_check!(
            !region.kind.is_allocatable(),
            "the kernel stack sits in {region:?}, which the frame allocator hands out"
        );
        println!("kernel stack at {stack:#x} in {region:?}, not reclaimable yet");

        // User address spaces: isolated lower halves, shared kernel half.
        let virt = 0x4000_0000;
        let frame = mm::alloc_frames(0).expect("out of frames");

        let before_mapped = mm::frame_stats().free;
        let mut space = hal::AddressSpace::new();
        space.map(Mapping {
            virt,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        let mapped_cost = before_mapped - mm::frame_stats().free;

        let before_empty = mm::frame_stats().free;
        let empty = hal::AddressSpace::new();
        let empty_cost = before_empty - mm::frame_stats().free;

        case_check!(
            mapped_cost == 4,
            "a single user page needed {mapped_cost} tables"
        );
        case_check!(
            empty_cost == 1,
            "an empty address space needed {empty_cost} tables"
        );

        // The two switches below move CR3 behind the scheduler's back: the
        // boot task has no `space` of its own, so a clock tick landing inside
        // this block would send the dispatcher's `prepare` to switch straight
        // back to the kernel root - and the assert would see a half-done
        // switch, which it did, one boot in ten. Nothing in here sleeps, so
        // holding interrupts off for the block is just the truth about what
        // it needs. exec_replace has the same shape and is already safe: it
        // records the task's new space before pointing CR3 at it.
        //
        // The answer comes *out* of the block rather than returning from inside
        // it, and the kernel's own tables come back on every path: a case that
        // left CR3 on a process's space would take the rest of the boot down with
        // it, which is the mistake §13.14/§13.15 are the record of.
        let in_user_space = critical_section::with(|_| -> Result<(), String> {
            space.switch();
            case_check!(
                hal::active_root() == space.root(),
                "switch() did not switch: the active root is {:#x}, not {:#x}",
                hal::active_root(),
                space.root()
            );

            let (back, flags) = hal::translate(virt).expect("the user page is not mapped");
            case_check!(
                back == frame,
                "the user page points at {back:#x}, not {frame:#x}"
            );
            case_check!(
                flags == MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                "the user page is {flags:?}, not read-write-user"
            );
            unsafe {
                ptr::write_bytes(virt as *mut u8, 0x77, PAGE_SIZE);
                case_check!(
                    *(virt as *const u8) == 0x77 && *((virt + PAGE_SIZE - 1) as *const u8) == 0x77,
                    "a write through the user page did not land at both ends of it"
                );
            }
            println!("user page at {virt:#x} mapped rwu and written through");

            let mut shared: Vec<u8> = Vec::new();
            shared.resize(8 * 1024, 0x11);
            case_check!(
                shared.iter().all(|&byte| byte == 0x11),
                "the heap stopped working in a process address space"
            );
            drop(shared);

            empty.switch();
            case_check!(
                hal::translate(virt).is_none(),
                "a user page leaked between address spaces"
            );
            let text = arch::kernel_sections().text.start;
            case_check!(
                hal::translate(text).is_some(),
                "the kernel half was not shared with the new address space"
            );
            println!("second address space: kernel half shared, user half empty");

            hal::switch_to_kernel();
            Ok(())
        });
        case_check!(
            hal::active_root() == hal::kernel_root(),
            "the machine came out of the user space on {:#x}, not the kernel's own root {:#x}",
            hal::active_root(),
            hal::kernel_root()
        );
        in_user_space?;

        let before_teardown = mm::frame_stats().free;
        drop(empty);
        drop(space);
        case_check!(
            mm::frame_stats().free == before_teardown + mapped_cost + empty_cost,
            "an address space did not give back every table it took: {} frames came back, {} went out",
            mm::frame_stats().free - before_teardown,
            mapped_cost + empty_cost
        );
        mm::free_frames(frame, 0);
        println!(
            "both address spaces torn down, {} frames returned",
            mapped_cost + empty_cost
        );
    }
    Ok(())
}

// ---- sched: context switch, round-robin, preemption, sleep, priority -----

/// Called once, before the shell sweep, because the sweep's last tests are the
/// ones that need a wire.
///
/// The adapter is this machine's to find - the PCI bus is where x86_64's
/// virtio-net lives - and the stack above it is `steps::bring_up_net`, the one
/// copy both machines call. What is left here is the probe and the honest line a
/// machine with no card owes.
fn init_net() {
    let Some(dev) = mpos::drivers::virtio::net::VirtioNet::init() else {
        println!("net: no virtio-net device on the bus; sockets answer ENETDOWN");
        return;
    };
    steps::bring_up_net(dev);
}

/// Bring up the rest of the virtio devices: the entropy source, the console
/// port, the GPU, the input devices, the sound card, the 9p share and the
/// vsock device. Each is probed, each reports what it found, and none of
/// them is load-bearing - a machine without one boots without it, which is
/// what keeps these lines facts rather than gates.
///
/// The entropy source goes first, because `rand::init` - the next thing
/// the boot does - mixes its bytes into the seed it stamps, and a probe
/// after the seed would be entropy nobody got. The service task goes last,
/// when everything it polls has been built.
fn init_virtio() {
    if mpos::drivers::virtio::rng::init_global().is_some() {
        println!("virtio-rng: host entropy mixed into the kernel seed");
    } else {
        println!("virtio-rng: no device on the bus; the seed stands on RDRAND alone");
    }

    if let Some(hvc) = mpos::drivers::virtio::console::init_global() {
        match hvc.size() {
            Some((columns, rows)) => println!("virtio-console: {columns}x{rows} at /dev/hvc0"),
            None => println!("virtio-console: up at /dev/hvc0"),
        }
    }

    if let Some((gpu, device)) = mpos::drivers::virtio::gpu::init_global() {
        let (width, height) = gpu.dims();
        println!(
            "virtio-gpu: {width}x{height}, {} KiB at /dev/fb0",
            gpu.fb_len() / 1024
        );
        // Hand the device to the render task whenever the probe found one. This
        // is not a console decision: `/dev/fb0` is a device a program may open,
        // and a session may claim the card to drive the scanout itself, whatever
        // the kernel's own console list says. What the list decides is whether
        // the kernel's log moves onto the screen, and the task reads that for
        // itself.
        mpos::drivers::video::render::spawn(device, gpu);
    }

    #[cfg(mpos_input_registry)]
    {
        let inputs = mpos::drivers::virtio::input::init();
        if inputs.is_empty() {
            println!("virtio-input: no device on the bus");
        }
        for (index, (_node, name)) in inputs.iter().enumerate() {
            println!("virtio-input: event{index}: {name}");
        }
    }

    if let Some(snd) = mpos::drivers::virtio::sound::init_global() {
        let (rate, channels) = snd.format();
        println!("virtio-sound: {rate} Hz, {channels} channels at /dev/dsp");
    }

    // The host share's device, probed *before* the mount. This call is the one
    // this machine was missing: `mount_share` read `fs9p::devices()`, nothing
    // ever filled it, so every boot printed "no share on the bus" and no share
    // was reachable even though QEMU exports one (`NOTES.md` §13.29).
    mpos::drivers::virtio::fs9p::init_global();
    steps::mount_share();

    match mpos::drivers::virtio::vsock::init_global() {
        Some(dev) => {
            println!(
                "virtio-vsock: guest cid {} answers on AF_VSOCK",
                dev.guest_cid()
            );
            mpos::net::vsock::init(dev);
        }
        None => println!("virtio-vsock: no device on the bus; AF_VSOCK answers EAFNOSUPPORT"),
    }

    // One permanent task polls the input devices and the vsock device - the
    // two whose work arrives whether or not anyone is waiting for it.
    mpos::drivers::virtio::spawn_service();
}

/// The boot transition moved to userland.
///
/// The kernel unpacks the initramfs and runs its `/init`, and *that script*
/// mounts /proc, /sys and /dev, mounts the real rootfs on `/dev/vda3`,
/// `pivot_root`s into it and `exec`s `/sbin/init`. From there OpenRC mounts
/// the canonical directories of the real root and starts the services.
///
/// Nothing here mounts a directory any more. A kernel that assembles /proc
/// and /dev before init exists is doing init's job, and the pieces it mounts
/// are exactly the pieces an owner of the system cannot then see, change or
/// unmount - because they were never in a mount table userland can read.
/// Everything the boot needs - the block devices to mount from, the
/// filesystem types to mount - is reachable through `mount(2)` instead.

/// M8: a real system start. The checks above are the kernel proving itself;
/// this is the kernel handing the machine over.
fn handoff_to_init(info: &BootInfo) -> ! {
    let pid = steps::hand_to_init();

    // The deepest the boot stack ever gets: `spawn_init` parsed an ELF and built
    // a process's address space and stack on top of everything above. Checking
    // here rather than after the handoff is the point - once init runs, the trap
    // that lands on an overrun stack is the one that cannot report it.
    report::check_boot_stack(info, "under spawn_init");

    println!("mpos: handing the machine to /init");

    sched::discard_exit_code(pid);

    // The wire, in the two steps both machines take (`steps::arm_console`
    // carries the ordering contract). This machine arms it *here* rather than
    // before the spawn because nothing reads the console until the init's shell
    // is up: the spawn is what makes the difference between a machine whose
    // input goes nowhere and one with a session to deliver to.
    steps::arm_console();
    // init forks, execve's the shell in the child, and waits. So a child
    // appearing under init is exactly the "the shell is up" signal - and init
    // itself staying alive is what makes that a shell and not a machine with
    // nobody left to start programs.
    while !sched::has_live_child(pid) {
        sched::sleep_ticks(1);
    }
    println!("M8_OK");
    report::park()
}
