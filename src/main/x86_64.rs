//! The x86_64 kernel image: boot entry, milestone acceptance checks, panic
//! handler.
//!
//! Everything reusable lives in the `mpos` library so it can be unit-tested on
//! the host; this file is only the parts that must run on the machine, and only
//! the parts that are x86_64's. `src/main.rs` chooses it by `target_arch` the
//! same way `src/arch/mod.rs` chooses its body, so a second machine adds its
//! own file beside this one and edits the `#[path]` line that picks between
//! them - not a `#[cfg]` on every item in here.
//!
//! # Boot flow
//!
//! Boot proceeds in three phases:
//!
//! 1. **Initialization** — `init_mm → init_arch → init_sched → init_vfs →
//!    init_blk → init_net → init_rand`. Each sets up a subsystem without
//!    running tests. After all inits complete, the system is ready.
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

use core::alloc::Layout;
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use mpos::arch;
use mpos::blk;
use mpos::boot::console_list::ConsoleList;
use mpos::boot::{self, BootInfo};
use mpos::drivers;
use mpos::hal;
use mpos::mm::{self, MapFlags, Mapping, PAGE_SIZE};
use mpos::println;
use mpos::sched::{self, Priority};
use mpos::syscall::{self, err, linux::nr as lnr};
use mpos::vfs::{self, FileSystem, Inode, NodeKind};

const MIB: usize = 1024 * 1024;

/// The word the loader writes at the bottom of the boot stack, mirroring the
/// loader's own `BOOT_STACK_CANARY`. Spelled as bytes again rather than shared,
/// because the two sides of the handoff ABI are separate compilations with no
/// crate between them - the same reason [`mpos::boot`]'s magic is written twice.
/// Reading it as `b"mpos_boo"` little-endian is a courtesy to whoever is staring
/// at a hexdump.
const BOOT_STACK_CANARY: u64 = u64::from_le_bytes(*b"mpos_boo");

/// The data partition device (partition 4), mounted at /mnt by `init_blk`.
/// Kept for the checks that need the raw block device rather than the mounted
/// tree (e.g. remounting to prove a write reached the disk).
static DATA_DEV: spin::Once<alloc::sync::Arc<dyn mpos::blk::BlockDev>> = spin::Once::new();

// ---------------------------------------------------------------------------
// Boot entry
// ---------------------------------------------------------------------------

/// `mpos-boot` jumps here in 64-bit long mode, ring 0, interrupts disabled, on
/// a stack it mapped itself, with `RDI` holding the [`Handoff`] record it wrote.
///
/// The register is the whole interface: the loader has already done the parts
/// that need firmware - read the kernel off the ESP, applied its relocations,
/// built the direct map and the higher-half alias, and left boot services - so
/// there is no protocol to negotiate and no call to make. It is also why the
/// record has to be checked rather than trusted: nothing in the machine
/// enforces that `RDI` holds what this function was compiled to expect, and a
/// wrong guess has to be a panic and not a wild read.
///
/// # Safety
///
/// The caller is the loader's jump to this symbol, with `RDI` holding the
/// record it wrote. Nothing chooses this call from Rust, and the pointer is
/// dereferenced before it can be validated - the check above happens *after*
/// the read - so the validity of `handoff` is the caller's contract and not
/// something this function can establish.
///
/// [`Handoff`]: mpos::boot::handoff::Handoff
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(handoff: *const boot::handoff::Handoff) -> ! {
    // Serial first: it depends on nothing the loader wrote, so it is usable even
    // if the record turns out to be the wrong one and `collect` panics below.
    mpos::console::init();

    // Enable SSE/AVX for user processes before anything else touches CR0/CR4.
    hal::enable_user_simd();

    let info = unsafe { boot::handoff::collect(handoff) };
    // Hand the command line to the filesystem layer before anyone can read
    // `/proc/cmdline`: the initramfs's init parses `root=` out of it, and an
    // empty file there sends the whole boot down the diskless path instead of
    // mounting the real root.
    boot::set_cmdline(info.cmdline);
    banner(&info);

    let stats = unsafe { mm::init(&info) };
    println!(
        "pmm: {} zones, {} frames ({} MiB)",
        stats.zones,
        stats.total,
        stats.total * PAGE_SIZE / MIB
    );
    // Logging comes up as soon as the heap does, because the subscriber it
    // installs allocates and the level it prints at is read from the command
    // line. Everything after this point is worth being able to turn on or off.
    mpos::logging::init();

    kernel_main(&info);
    // The system never stops booting: `kernel_main` ends parked in the boot
    // task, with init and its shell running. If it ever returned, the stack
    // underneath is the loader's and there is nothing left to do.
    hal::halt()
}

fn banner(info: &BootInfo) {
    println!("mpos {} booting", env!("CARGO_PKG_VERSION"));
    println!(
        "bootloader: {} {}",
        info.bootloader_name, info.bootloader_version
    );
    println!(
        "kernel: phys {:#x}, virt {:#x}",
        info.kernel_phys_base, info.kernel_virt_base
    );
    println!("hhdm offset: {:#x}", info.hhdm_offset);
    println!("cmdline: {:?}", info.cmdline);
    println!("memory map ({} regions):", info.region_count);
    for region in info.regions() {
        println!("  {:?}", region);
    }
    println!("allocatable: {} MiB", info.allocatable_bytes() / MIB);
}

// ---------------------------------------------------------------------------
// Module-based milestone checks
// ---------------------------------------------------------------------------

/// The boot task's whole life: prove the kernel works, then hand the machine
/// to init and stand aside. It ends inside `handoff_to_init`, which parks the
/// boot task rather than returning - a system that stops running itself is not
/// a system.
fn kernel_main(info: &BootInfo) {
    // The milestone checks prove each subsystem in isolation, but they are pure
    // probes: `check_arch` busy-waits half a second and `check_sched` sleeps out
    // several more ticks, so on an ordinary boot they are nothing but latency
    // and serial chatter between the real initialisation and init. They run only
    // when the command line carries `mpos.selftest` - which `cargo xtask test`
    // adds and the real rootfs boot does not - so a normal boot pays none of it.
    let cmdline = boot::cmdline();
    let has = |word: &str| cmdline.split_whitespace().any(|a| a == word);

    let selftest = has("mpos.selftest");

    // Which consoles carry the kernel log, in Linux's spelling: every `console=`
    // word names one, and the last is the preferred one - the sink whose writes
    // park until the hardware has taken them rather than being dropped. Nothing
    // else about how this machine was launched reaches the kernel: there is no
    // "gui" or "terminal" word here, and a word naming a console this kernel does
    // not have is reported and ignored rather than guessed at.
    //
    // A device-presence test could not answer this question instead: a QEMU
    // booted without a window still has a virtio-gpu and still reports a display
    // mode. The command line is the one place the answer actually is.
    mpos::console::set_console_list(ConsoleList::from_cmdline(cmdline, |unknown| {
        println!("console={unknown}: no such console here, ignored");
    }));

    init_mm();
    if selftest {
        check_mm(info);
    }

    init_arch(info);
    if selftest {
        check_arch(info);
    }

    init_sched();
    if selftest {
        check_sched();
    }

    init_vfs(info);
    if selftest {
        check_vfs();
    }

    init_pci();
    init_blk();
    init_net();
    init_virtio();
    register_exit_hooks();

    mpos::rand::init();

    if selftest {
        check_ipc();
        check_boot_stack(info, "after the self-tests");
    }

    println!("All ready!");

    handoff_to_init(info);
}

/// Check that the boot stack never ran into the canary the loader planted at
/// its bottom.
///
/// The boot stack is the one stack nothing in the kernel owns: the loader
/// allocated it, the kernel spends `_start` through `handoff_to_init` on it, and
/// after that the boot task keeps it forever - it is adopted, so there is no
/// `Stack` whose canary the reaper would check. That leaves it the one stack
/// whose overflow has no witness at all, which is the worst place for the gap:
/// the boot path is the deepest single-threaded run in the machine, with
/// `handoff_to_init` nesting an ELF parse and a whole address space on top of
/// every initialisation step that preceded it.
///
/// An overflow here is not a clean failure. Whatever trap comes next builds its
/// frame where the overflow already wrote, so the `iretq` out of it pops a word
/// that was never a selector and the machine dies reporting a `#GP` on a
/// descriptor that was never loaded. That is a symptom pointing at the GDT, and
/// it costs a long afternoon to find out it was the stack.
///
/// Called at the two deepest points in the boot: before the handoff, and again
/// after init has been spawned - `spawn_init` is the single deepest call the
/// boot task ever makes.
fn check_boot_stack(info: &BootInfo, when: &str) {
    let base = info.boot_stack_base;
    // Zero means the loader planted no canary, which is how an older loader's
    // handoff reads. Nothing to check, and not an error.
    if base == 0 {
        return;
    }
    let canary = unsafe { core::ptr::read_volatile(base as *const u64) };
    assert_eq!(
        canary, BOOT_STACK_CANARY,
        "the boot stack ran off its bottom by {when}: the canary at {base:#x} is \
         {canary:#x}, not {BOOT_STACK_CANARY:#x}. The loader gave it 64 pages; a \
         build that needs more says so here rather than as a #GP on a selector \
         nothing ever loaded."
    );
    println!("boot stack: intact {when}");
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

fn init_arch(info: &BootInfo) {
    hal::init_interrupts();
    hal::enable_interrupts();

    let sections = arch::kernel_sections();
    println!(
        "image: text {} KiB, rodata {} KiB, data {} KiB",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024
    );

    let tables = unsafe { hal::install(info, &sections) };
    println!(
        "page tables: {} frames ({} KiB)",
        tables,
        tables * PAGE_SIZE / 1024
    );
}

fn init_sched() {
    sched::init();
    println!(
        "scheduler up: {} slots, running task {:?}",
        sched::MAX_TASKS,
        sched::current()
    );
}

/// The named child of `dir`, made if it is not there and reused if it is.
///
/// The kernel's checks were written against a root filesystem of their own
/// making, where every name they wanted was free. Alpine's tree is not obliged
/// to leave those gaps: it has its own `/etc/hostname` and its own `/tmp`, and
/// both already hold something. The checks want a file to write through, not a
/// file that was not there before - so they pick names Alpine does not use.
fn ensure_child(dir: &Arc<dyn Inode>, name: &str, kind: NodeKind) -> Arc<dyn Inode> {
    match dir.lookup(name) {
        Ok(child) => child,
        Err(_) => dir.create(name, kind).expect("create the scratch file"),
    }
}

fn init_vfs(info: &BootInfo) {
    vfs::file::init_standard(vfs::tty::init());

    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(&*root);

    let archive = info.initramfs.expect("the boot config loaded no initramfs");
    let members = vfs::tar::unpack_into(vfs::global(), archive).expect("the initramfs was broken");
    println!("initramfs: {members} members unpacked");

    // No /dev here. The kernel used to build one and mount it on the temporary
    // root, which meant it existed before the block layer did - so it had no
    // disk in it, and `/init` mounting its own got `EBUSY` and kept the crippled
    // one. Devices belong to whoever mounts /dev, and on this machine that is
    // the initramfs's own init script.
}

/// Walk the PCI bus once, and publish what is on it.
///
/// This is the first thing that touches a device, and it has to be: describing
/// a BAR means asking the device what it decodes, which means writing all ones
/// into the register and reading back what it answers. Done here, before any
/// driver has claimed anything, the only registers disturbed are ones nothing
/// has used yet - and every one of them is written back. Done later, a device
/// could be asked to answer for an address nobody assigned it while a driver
/// was mid-transfer.
///
/// What it publishes is what `/sys` and `/proc` describe the machine from. The
/// *drivers* come after, and record themselves with [`drivers::pci::claim`], so
/// `lspci -k` names a driver that really did bind rather than one that merely
/// matches.
fn init_pci() {
    let count = drivers::pci::init();
    println!("pci: {count} functions enumerated");
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
    // names (`PLAN.md` §7.3). The first one brought up is the boot disk, whose
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
        // there rather than deciding for itself (`uevent::Numbering`).
        blk::add_disk(
            "vda",
            mpos::uevent::VIRTIO_BLK_MAJOR,
            "/devices/virtio/block",
            dev,
        );
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
    let fs = vfs::ext4::Ext4Fs::mount(data.clone()).expect("the data image did not mount");
    // What it is on disk is the filesystem's own answer: ext2 and ext4 take
    // different names from the same driver.
    println!(
        "{}: {}-sector partition",
        fs.name(),
        data.capacity_sectors()
    );
    vfs::mount("/mnt", &fs).expect("mount /mnt");
    DATA_DEV.call_once(|| data);
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
fn check_mm(info: &BootInfo) {
    // M0: the global allocator works.
    {
        println!("M0_OK");
    }

    // M2: frame allocator + heap deep verification.
    {
        // Frames: buddy allocator hands out disjoint blocks, merges on free.
        let stats = mm::frame_stats();
        println!(
            "pmm: {} of {} frames free ({} MiB)",
            stats.free,
            stats.total,
            stats.free * PAGE_SIZE / MIB
        );
        assert!(stats.zones > 0, "no memory zones were built");

        // HHDM: allocate a frame, write through the direct map, read back.
        let frame = mm::alloc_frames(0).expect("out of order-0 frames");
        let virt = info.phys_to_virt(frame);
        unsafe {
            ptr::write_bytes(virt as *mut u8, 0xa5, PAGE_SIZE);
            assert_eq!(*(virt as *const u8), 0xa5);
            assert_eq!(*((virt + PAGE_SIZE - 1) as *const u8), 0xa5);
        }
        mm::free_frames(frame, 0);
        println!("frame written and read back through the hhdm");

        // Buddy merge: allocate order-1, free it, re-allocate and verify
        // we get the same block back. This exercises the merge path because
        // the allocator must coalesce order-0 buddies to satisfy order-1.
        let order1 = mm::alloc_frames(1).expect("no order-1 block for merge test");
        mm::free_frames(order1, 1);
        assert_eq!(
            mm::alloc_frames(1),
            Some(order1),
            "buddy merge failed: freed order-1 block was not re-allocated"
        );
        mm::free_frames(order1, 1);
        assert_eq!(mm::frame_stats().free, stats.free, "frames leaked");

        // Large block from the zone's largest orders.
        let big = mm::alloc_frames(mm::MAX_ORDER).expect("out of order-12 frames");
        let virt = info.phys_to_virt(big);
        let bytes = PAGE_SIZE << mm::MAX_ORDER;
        unsafe {
            ptr::write_bytes(virt as *mut u8, 0x5a, bytes);
            assert_eq!(*((virt + bytes - 1) as *const u8), 0x5a);
        }
        println!("{} MiB block written end to end", bytes / MIB);
        mm::free_frames(big, mm::MAX_ORDER);
        assert_eq!(mm::frame_stats().free, stats.free, "frames leaked");

        // Heap: every size class, large bypass, vector growth through realloc.
        let baseline = mm::heap_stats();
        let mut live: Vec<(*mut u8, Layout)> = Vec::new();

        for class in [16, 32, 64, 128, 256, 512, 1024, 2048] {
            for align in [1, 8, 16, class] {
                let layout =
                    Layout::from_size_align(class / 2 + 1, align).expect("layout is representable");
                take(&mut live, layout);
            }
        }

        for layout in [
            Layout::from_size_align(4096, 8).unwrap(),
            Layout::from_size_align(3000, 8192).unwrap(),
            Layout::from_size_align(1 << 20, 4096).unwrap(),
        ] {
            take(&mut live, layout);
        }

        let mut grown: Vec<u8> = Vec::new();
        for i in 0..100 * 1024 {
            grown.push((i % 251) as u8);
        }
        assert_eq!(grown.len(), 100 * 1024);
        assert!(grown.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
        drop(grown);

        let held = live.len();
        for (ptr, layout) in live {
            unsafe { alloc::alloc::dealloc(ptr, layout) };
        }
        println!("{held} allocations across every class and the large path");

        let after = mm::heap_stats();
        assert_eq!(
            after.live, baseline.live,
            "dealloc did not return every byte to the heap"
        );
        assert!(
            after.cached >= baseline.cached,
            "freed slots did not go back to their class"
        );

        // Cross-check: heap and frame allocator agree on memory handed out.
        // Frames consumed by non-heap code (page tables, DMA buffers from
        // init_blk) are subtracted so only heap-owned frames are compared.
        let non_heap = (stats.total - stats.free) * PAGE_SIZE - baseline.mapped;
        let frames = mm::frame_stats();
        let heap = mm::heap_stats();
        assert_eq!(
            (frames.total - frames.free) * PAGE_SIZE - non_heap,
            heap.mapped,
            "heap and frame allocator disagree about memory handed out"
        );
        println!(
            "heap holds {} KiB in {} free frames of {}",
            heap.mapped / 1024,
            frames.free,
            frames.total
        );

        println!("M2_OK");
    }
}

/// Allocate `layout`, check the contract held, and fill the memory so that a
/// later hand-out of the same bytes to someone else is caught.
fn take(live: &mut Vec<(*mut u8, Layout)>, layout: Layout) {
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    assert!(!ptr.is_null(), "allocation of {layout:?} failed");
    assert_eq!(
        ptr as usize % layout.align(),
        0,
        "{layout:?} came back misaligned"
    );
    for &(other, other_layout) in live.iter() {
        assert!(
            !overlaps(ptr, layout, other, other_layout),
            "{layout:?} at {ptr:p} overlaps a live allocation at {other:p}"
        );
    }

    let fill = (live.len() & 0xff) as u8;
    unsafe { ptr::write_bytes(ptr, fill, layout.size()) };
    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(
        bytes.iter().all(|&b| b == fill),
        "{layout:?} at {ptr:p} did not read back what was written"
    );

    live.push((ptr, layout));
}

fn overlaps(a: *mut u8, a_layout: Layout, b: *mut u8, b_layout: Layout) -> bool {
    let (a_start, b_start) = (a as usize, b as usize);
    a_start < b_start + b_layout.size() && b_start < a_start + a_layout.size()
}

// ---- arch: interrupts, timer, paging, address spaces ---------------------

/// Prove the clock works, exceptions are catchable, the kernel runs on its
/// own page tables with correct permissions, and processes get isolated
/// address spaces.
fn check_arch(info: &BootInfo) {
    // M1: clock interrupts and breakpoint exception.
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
        assert!(
            (half - tol..=half + tol).contains(&elapsed),
            "timer tick rate is wrong"
        );

        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("int3")
        };
        assert_eq!(arch::breakpoints_caught(), 1, "#BP was not caught");
        println!("breakpoint caught and returned from");

        // The machine's copy of the Linux register-file field order must agree
        // with the ABI's. The two tables are duplicated on purpose - one side is
        // bare-metal only, the other is host-built, so neither can name the
        // other's module - and this is what keeps the duplication honest rather
        // than hoped-for (`arch/x86_64/context.rs`, `syscall/linux/abi.rs`).
        for (name, index) in arch::ABI_REG_INDEX {
            assert_eq!(
                syscall::linux::abi::uregs_by_name(name),
                Some(index),
                "the machine's {name} disagrees with user_regs_struct"
            );
        }
        assert_eq!(
            syscall::linux::abi::UREGS_COUNT,
            arch::ABI_REG_COUNT,
            "the register file is a different size on the two sides"
        );
        println!("register file field order agrees with the ABI");

        println!("M1_OK");
    }

    // M3: kernel page tables, W^X, user address spaces.
    {
        let sections = arch::kernel_sections();

        // Direct map works after the switch.
        let mut proof: Vec<u8> = Vec::new();
        proof.resize(64 * 1024, 0x3c);
        assert!(
            proof.iter().all(|&byte| byte == 0x3c),
            "the direct map is broken after the switch"
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
                    .unwrap_or_else(|| panic!("{name} at {virt:#x} is not mapped"));
                assert_eq!(
                    flags, expect,
                    "{name} at {virt:#x} has the wrong permissions"
                );
                assert_eq!(
                    virt - phys,
                    image_offset,
                    "{name} at {virt:#x} does not point at the image"
                );
                virt += PAGE_SIZE;
            }
        }
        println!("text r-x, rodata r--, data rw- across the whole image");

        // Direct map is writable; null page is unmapped.
        let frame = mm::alloc_frames(0).expect("out of frames");
        let (back, flags) = hal::translate(info.phys_to_virt(frame))
            .expect("the direct map is missing a frame the kernel owns");
        assert_eq!(back, frame, "the direct map translates to the wrong frame");
        assert_eq!(flags, MapFlags::READ | MapFlags::WRITE);
        mm::free_frames(frame, 0);

        assert!(hal::translate(0).is_none(), "the null page is mapped");
        println!("direct map writable and non-executable, null page unmapped");

        // Kernel stack is not reclaimable.
        let probe = 0u8;
        let stack = mm::virt_to_phys(&raw const probe as usize);
        let region = info
            .regions()
            .iter()
            .find(|r| r.base <= stack && stack < r.end())
            .unwrap_or_else(|| panic!("the stack at {stack:#x} is in no memory-map region"));
        assert!(
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

        assert_eq!(
            mapped_cost, 4,
            "a single user page needed {mapped_cost} tables"
        );
        assert_eq!(
            empty_cost, 1,
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
        critical_section::with(|_| {
            space.switch();
            assert_eq!(hal::active_root(), space.root(), "switch() did not switch");

            let (back, flags) = hal::translate(virt).expect("the user page is not mapped");
            assert_eq!(back, frame, "the user page points at the wrong frame");
            assert_eq!(flags, MapFlags::READ | MapFlags::WRITE | MapFlags::USER);
            unsafe {
                ptr::write_bytes(virt as *mut u8, 0x77, PAGE_SIZE);
                assert_eq!(*(virt as *const u8), 0x77);
                assert_eq!(*((virt + PAGE_SIZE - 1) as *const u8), 0x77);
            }
            println!("user page at {virt:#x} mapped rwu and written through");

            let mut shared: Vec<u8> = Vec::new();
            shared.resize(8 * 1024, 0x11);
            assert!(
                shared.iter().all(|&byte| byte == 0x11),
                "the heap stopped working in a process address space"
            );
            drop(shared);

            empty.switch();
            assert!(
                hal::translate(virt).is_none(),
                "a user page leaked between address spaces"
            );
            let text = arch::kernel_sections().text.start;
            assert!(
                hal::translate(text).is_some(),
                "the kernel half was not shared with the new address space"
            );
            println!("second address space: kernel half shared, user half empty");

            hal::switch_to_kernel();
        });

        let before_teardown = mm::frame_stats().free;
        drop(empty);
        drop(space);
        assert_eq!(
            mm::frame_stats().free,
            before_teardown + mapped_cost + empty_cost,
            "an address space did not give back every table it took"
        );
        mm::free_frames(frame, 0);
        println!(
            "both address spaces torn down, {} frames returned",
            mapped_cost + empty_cost
        );

        println!("M3_OK");
    }
}

// ---- sched: context switch, round-robin, preemption, sleep, priority -----

/// Which task ran, in the order they ran.
///
/// The checks below assert on this rather than on the log, because interleaving
/// is precisely the thing that has to be proven: a boot log that looks right is
/// not evidence that two tasks took turns.
static RECORD: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static RECORD_LEN: AtomicUsize = AtomicUsize::new(0);

fn record(who: u8) {
    critical_section::with(|_| {
        let index = RECORD_LEN.fetch_add(1, Ordering::Relaxed);
        assert!(index < RECORD.len(), "the run record is full");
        RECORD[index].store(who, Ordering::Relaxed);
    });
}

fn recorded() -> Vec<u8> {
    RECORD
        .iter()
        .take(RECORD_LEN.load(Ordering::Relaxed))
        .map(|entry| entry.load(Ordering::Relaxed))
        .collect()
}

fn reset_record() {
    RECORD_LEN.store(0, Ordering::Relaxed);
}

/// Wait until every task this milestone started has finished and been reaped.
fn drain() {
    while sched::task_count() > sched::permanent_tasks() {
        discard_codes();
        sched::sleep_ticks(1);
    }
}

/// Wait until `id` has left an exit code, by polling rather than sleeping.
fn wait_for_code(id: sched::TaskId) {
    while sched::exit_code(id).is_none() {
        sched::sleep_ticks(1);
    }
}

/// Take every exit code nobody is going to ask for, so the slots they hold
/// can be freed.
fn discard_codes() {
    for slot in 2..sched::MAX_TASKS - 1 {
        sched::discard_exit_code(sched::TaskId(slot));
    }
}

/// Two tasks at the same priority take turns.
fn alternating() {
    reset_record();
    critical_section::with(|_| {
        for who in [0u8, 1] {
            sched::spawn(
                move || {
                    for slice in 0..5 {
                        record(who);
                        println!("task {who} runs slice {slice}");
                        sched::yield_now();
                    }
                },
                Priority::NORMAL,
            );
        }
    });
    drain();

    let got = recorded();
    let repeats = got.windows(2).filter(|pair| pair[0] == pair[1]).count();
    assert!(
        repeats <= 2,
        "two tasks at the same priority did not take turns: {got:?}"
    );
    println!("two tasks alternated five slices each");
}

/// A task that never yields still loses the core.
fn preempted() {
    reset_record();
    const SPIN_TICKS: u64 = 10;

    sched::spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::NORMAL,
    );
    sched::spawn(
        || {
            for _ in 0..3 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![1, 1, 1, 0],
        "a task that never yielded held the core to itself"
    );
    println!("a task that never yielded was preempted for {SPIN_TICKS} ticks");
}

/// A sleeping task is off the ready queue until the clock puts it back.
fn sleeping() {
    reset_record();
    const SLEEP_TICKS: u64 = 10;

    sched::spawn(
        || {
            record(0);
            let start = hal::ticks();
            sched::sleep_ticks(SLEEP_TICKS);
            let slept = hal::ticks() - start;
            assert!(
                (SLEEP_TICKS..SLEEP_TICKS + 3).contains(&slept),
                "asked for {SLEEP_TICKS} ticks and got {slept}"
            );
            record(0);
        },
        Priority::NORMAL,
    );
    sched::sleep_ticks(1);
    sched::spawn(
        || {
            for _ in 0..3 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::NORMAL,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![0, 1, 1, 1, 0],
        "a sleeping task kept running"
    );
    println!("a task slept {SLEEP_TICKS} ticks while the other ran");
}

/// The scheduler's idle path still wakes on the clock.
fn idling() {
    reset_record();
    sched::spawn(|| record(0), Priority::NORMAL);
    drain();

    record(1);
    sched::sleep_ticks(5);
    record(1);

    assert_eq!(
        recorded(),
        vec![0, 1, 1],
        "the boot task did not come back from a sleep with nothing else runnable"
    );
    println!("the scheduler idled with nothing to run and the clock woke it");
}

/// A higher band takes the core from a task that is already running.
fn priority_wins() {
    reset_record();
    const SPIN_TICKS: u64 = 5;
    const WAKE_AFTER: u64 = 2;

    sched::spawn(
        || {
            let start = hal::ticks();
            while hal::ticks() - start < SPIN_TICKS {
                core::hint::spin_loop();
            }
            record(0);
        },
        Priority::LOW,
    );
    sched::spawn(
        || {
            sched::sleep_ticks(WAKE_AFTER);
            for _ in 0..2 {
                record(1);
                sched::yield_now();
            }
        },
        Priority::HIGH,
    );
    drain();

    assert_eq!(
        recorded(),
        vec![1, 1, 0],
        "a HIGH task did not outrank a LOW one that held the core"
    );
    println!("a HIGH task took the core from a spinning LOW one");
}

/// Prove the scheduler: context switch, round-robin, preemption, sleep, idle,
/// and priority-based core stealing.
fn check_sched() {
    let before = (mm::frame_stats().free, mm::heap_stats().mapped);
    alternating();
    preempted();
    sleeping();
    idling();
    priority_wins();

    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "a task that exited was never reaped"
    );

    let after = (mm::frame_stats().free, mm::heap_stats().mapped);
    let heap_growth = (after.1 - before.1) / PAGE_SIZE;
    assert_eq!(
        before.0 - after.0,
        heap_growth,
        "a task stack was not returned to the frame allocator"
    );
    println!("every task reaped, every stack returned");
    println!("M4_OK");
}

// ---- syscall + vfs: gate, ramfs, mounts, initramfs, devfs ---------------

/// Issue a Linux x86_64 syscall: one entry into [`syscall::linux_dispatch`],
/// the same path the `syscall` gate drives for ring 3, called here from the
/// kernel's own checks with `from_user` false.
#[cfg(target_arch = "x86_64")]
unsafe fn syscall(
    nr: u64,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> isize {
    syscall::linux_dispatch(nr, [a1, a2, a3, a4, a5, a6], false, core::ptr::null_mut())
}

/// Prove the syscall gate, ramfs, mounts, initramfs unpack, and device filesystem.
fn check_vfs() {
    // M6: syscall dispatch and the fd table.
    {
        // Probe a number beyond the table: 999 is now CLEANUP_AFTER_KTHREAD.
        assert_eq!(
            syscall::linux_dispatch(1024, [0; syscall::ARG_COUNT], false, core::ptr::null_mut()),
            err::ENOSYS
        );
        assert_eq!(
            syscall::linux_dispatch(lnr::READ, [0, 0, 0, 0, 0, 0], false, core::ptr::null_mut()),
            0,
            "a zero-length read is empty, not an error"
        );
        assert_eq!(
            syscall::linux_dispatch(lnr::READ, [3, 0, 0, 0, 0, 0], false, core::ptr::null_mut()),
            err::EBADF
        );
        assert_eq!(
            syscall::linux_dispatch(lnr::WRITE, [7, 0, 0, 0, 0, 0], false, core::ptr::null_mut()),
            err::EBADF
        );

        let msg = b"syscall: write through dispatch\n";
        let wrote = syscall::linux_dispatch(
            lnr::WRITE,
            [1, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
            false,
            core::ptr::null_mut(),
        );
        assert_eq!(wrote as usize, msg.len());

        // The same dispatch, run inside a real task: getpid answers this
        // task's id, yield and sleep suspend and resume it, and exit through
        // dispatch takes the task down.
        sched::spawn(
            || {
                let pid = syscall::linux_dispatch(
                    lnr::GETPID,
                    [0; syscall::ARG_COUNT],
                    false,
                    core::ptr::null_mut(),
                );
                assert!(pid >= 2, "getpid returned {pid}");

                let msg = b"syscall: write through dispatch in a spawned task\n";
                let wrote = syscall::linux_dispatch(
                    lnr::WRITE,
                    [1, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
                    false,
                    core::ptr::null_mut(),
                );
                assert_eq!(wrote as usize, msg.len(), "dispatch dropped bytes");

                assert_eq!(
                    syscall::linux_dispatch(
                        lnr::SCHED_YIELD,
                        [0; syscall::ARG_COUNT],
                        false,
                        core::ptr::null_mut()
                    ),
                    0
                );
                // 2 ms, 1 byte, 0 padding — a request for the handler to
                // exercise, not to time precisely.
                let req: [u64; 2] = [0, 2_000_000];
                assert_eq!(
                    syscall::linux_dispatch(
                        lnr::NANOSLEEP,
                        [req.as_ptr() as usize, 0, 0, 0, 0, 0],
                        false,
                        core::ptr::null_mut(),
                    ),
                    0
                );
                syscall::linux_dispatch(
                    lnr::EXIT_GROUP,
                    [0, 0, 0, 0, 0, 0],
                    false,
                    core::ptr::null_mut(),
                );
            },
            Priority::NORMAL,
        );
        drain();
        assert_eq!(
            sched::task_count(),
            sched::permanent_tasks(),
            "exit through dispatch did not end the task"
        );
        println!("M6_OK");
    }

    // M7: ramfs, mounts, multi-task file I/O through the fd table.
    {
        // Ensure /etc exists before poking a scratch file into it. The
        // initramfs normally carries /etc, but a stripped archive must not
        // turn into a panic in the middle of a milestone check.
        let etc = match vfs::resolve("/etc") {
            Ok(dir) => dir,
            Err(_) => vfs::resolve("/")
                .unwrap()
                .create("etc", NodeKind::Dir)
                .expect("create /etc"),
        };
        ensure_child(&etc, "mpos-hostname", NodeKind::File);

        let fd = vfs::open("/etc/mpos-hostname").expect("open /etc/mpos-hostname");
        assert_eq!(vfs::write(fd, b"mpos\n").unwrap(), 5);
        vfs::close(fd).unwrap();

        let fd = vfs::open("/etc/mpos-hostname").unwrap();
        let mut buf = [0u8; 32];
        let n = vfs::read(fd, &mut buf).unwrap();
        vfs::close(fd).unwrap();
        assert_eq!(&buf[..n], b"mpos\n");

        let tmp = vfs::ramfs::RamFs::new();
        ensure_child(&vfs::resolve("/").unwrap(), "tmp", NodeKind::Dir);
        vfs::mount("/tmp", &*tmp).expect("mount /tmp");
        let inner_root = tmp.root();
        assert!(
            Arc::ptr_eq(&vfs::resolve("/tmp").unwrap(), &inner_root),
            "resolving /tmp did not cross the mount"
        );
        assert_eq!(vfs::mount("/tmp", &*tmp).err(), Some(vfs::VfsError::Busy));
        // Inside the freshly mounted filesystem, so nothing of Alpine's is here
        // to collide with - it is a new root, not the tree behind the mount.
        vfs::resolve("/tmp")
            .unwrap()
            .create("shared", NodeKind::File)
            .unwrap();

        sched::spawn(
            || {
                let fd = vfs::open("/tmp/shared").expect("writer: open");
                for chunk in [&b"one"[..], &b"two"[..]] {
                    assert_eq!(vfs::write(fd, chunk).unwrap(), 3, "writer: write");
                }
                vfs::close(fd).unwrap();
                sched::exit(0);
            },
            Priority::NORMAL,
        );
        sched::spawn(
            || {
                while sched::task_count() > sched::permanent_tasks() + 1 {
                    sched::sleep_ticks(1);
                }
                let fd = vfs::open("/tmp/shared").expect("reader: open");
                let mut buf = [0u8; 6];
                let n = vfs::read(fd, &mut buf).expect("reader: read");
                vfs::close(fd).unwrap();
                assert_eq!(n, 6, "reader: the file was short");
                assert_eq!(&buf, b"onetwo", "reader: the wrong bytes came back");
                sched::exit(0);
            },
            Priority::NORMAL,
        );
        drain();
        assert_eq!(
            sched::task_count(),
            sched::permanent_tasks(),
            "a vfs task did not exit"
        );
        println!("M7_OK");
    }
}

// ---- ipc: pipes, TTY line discipline, signals ---------------------------

/// Prove pipes, TTY line discipline, and signal delivery.
fn check_ipc() {
    use mpos::sched::signal::{SIGKILL, SIGTERM};

    // --- pipe: bytes cross tasks, EOF after writer leaves.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    assert_ne!(rfd, wfd, "pipe() handed out the same fd twice");

    sched::spawn(
        move || {
            sched::sleep_ticks(2);
            let msg = b"ping through a pipe\n";
            let wrote = unsafe {
                syscall(
                    lnr::WRITE,
                    wfd as usize,
                    msg.as_ptr() as usize,
                    msg.len(),
                    0,
                    0,
                    0,
                )
            };
            assert_eq!(wrote as usize, msg.len(), "the pipe dropped bytes");
            vfs::close(wfd).unwrap();
        },
        Priority::NORMAL,
    );

    let mut buf = [0u8; 64];
    let got = unsafe {
        syscall(
            lnr::READ,
            rfd as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
            0,
            0,
            0,
        )
    };
    assert_eq!(got as usize, b"ping through a pipe\n".len());
    assert_eq!(&buf[..got as usize], b"ping through a pipe\n");
    let got = vfs::read(rfd, &mut buf).unwrap();
    assert_eq!(got, 0, "a pipe with no writers reads EOF, not an error");
    vfs::close(rfd).unwrap();
    drain();
    println!("pipe: bytes crossed between two tasks, EOF after the writer left");

    // --- pipe: write with no reader is EPIPE.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    sched::spawn(
        move || {
            vfs::close(rfd).unwrap();
        },
        Priority::NORMAL,
    );
    drain();
    let rc = unsafe { syscall(lnr::WRITE, wfd as usize, b"x".as_ptr() as usize, 1, 0, 0, 0) };
    assert_eq!(rc, err::EPIPE, "a write to a readerless pipe must be EPIPE");
    vfs::close(wfd).unwrap();
    println!("pipe: write with no reader answered EPIPE and nobody died");

    // --- tty: editing, echo, canonical reads, Ctrl-D.
    let tty = vfs::tty::global();
    for &b in b"hellx\x7fo\n" {
        tty.receive_byte(b);
    }
    let mut line = [0u8; 32];
    // Every one of these reads is fed before it is asked, so it never blocks -
    // and a boot task with no signal dispositions cannot be interrupted either.
    // The `expect` is the honest form of "this answer cannot be an error here".
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(&line[..n], b"hello\n", "backspace did not edit the line");

    for &b in b"gate\n\x04" {
        tty.receive_byte(b);
    }
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(&line[..n], b"gate\n");
    let n = tty.read(&mut line).expect("a fed tty read answers a count");
    assert_eq!(n, 0, "Ctrl-D on an empty line is EOF");

    // Blocking fd 0 read through the gate.
    sched::spawn(
        || {
            sched::sleep_ticks(2);
            for &b in b"slow\n" {
                vfs::tty::global().receive_byte(b);
            }
        },
        Priority::NORMAL,
    );
    let n = unsafe {
        syscall(
            lnr::READ,
            0,
            line.as_mut_ptr() as usize,
            line.len(),
            0,
            0,
            0,
        )
    };
    assert_eq!(n as usize, b"slow\n".len());
    assert_eq!(&line[..n as usize], b"slow\n");
    drain();
    println!("tty: line editing, echo, Ctrl-D, and a blocking fd 0 read");

    // --- signals: SIGTERM, SIGKILL, self-sent, kill(0) probe.
    let paused = sched::spawn(|| sched::pause(), Priority::NORMAL);
    sched::sleep_ticks(2);
    assert!(sched::send_signal(paused, SIGTERM));
    wait_for_code(paused);
    assert_eq!(
        sched::exit_code(paused),
        Some(-(SIGTERM as isize)),
        "SIGTERM did not end the paused task"
    );
    drain();

    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(lnr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    let blocked = sched::spawn(
        move || {
            let mut buf = [0u8; 8];
            let n = vfs::read(rfd, &mut buf).unwrap();
            unreachable!("a read with no writer and no signal returned {n}");
        },
        Priority::NORMAL,
    );
    sched::sleep_ticks(2);
    assert!(sched::send_signal(blocked, SIGKILL));
    wait_for_code(blocked);
    assert_eq!(
        sched::exit_code(blocked),
        Some(-(SIGKILL as isize)),
        "SIGKILL did not end the blocked reader"
    );
    drain();
    vfs::close(wfd).unwrap();

    let selfsent = sched::spawn(
        || {
            sched::send_signal(sched::current(), SIGTERM);
            sched::pause();
        },
        Priority::NORMAL,
    );
    wait_for_code(selfsent);
    assert_eq!(sched::exit_code(selfsent), Some(-(SIGTERM as isize)));
    drain();

    assert_eq!(
        unsafe { syscall(lnr::KILL, sched::current().0, 0, 0, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { syscall(lnr::KILL, 9999, SIGTERM as usize, 0, 0, 0, 0) },
        err::ESRCH
    );
    // The signal-refusal boundary. Stage B opened the real-time range, so
    // `SIGRTMIN` (32) is now a valid `kill` target (as on Linux); the number
    // that must be refused is the one past `_NSIG` (64), not 32.
    assert_eq!(
        unsafe { syscall(lnr::KILL, 0, 65, 0, 0, 0, 0) },
        err::EINVAL
    );
    println!("signals: SIGTERM, SIGKILL, and the kill(0) probe behave");

    // NB: not M8_OK here. The M8 marker belongs to the init handoff,
    // which is the real "the machine is up" event. This check's marker is
    // its own, so a CI harness that asserts on M8_OK is asserting on
    // init, not on IPC.
    println!("M8_IPC_OK");
}

// ---- init handoff (terminal, not a check) --------------------------------

/// Bring the network stack up.
///
/// Called once, before the shell sweep, because the sweep's last tests are the
/// ones that need a wire. The task this starts lives as long as the machine
/// does - it is what keeps a connection moving while its owner is blocked -
/// which is why it is started permanent rather than by `spawn`.
fn init_net() {
    // The adapter is this machine's to find - the PCI bus is where x86_64's
    // virtio-net lives - and the stack receives it through the `NetDevice`
    // seam rather than probing for it, so a machine whose card is MMIO hands
    // over its own device and the stack above is the same code.
    let Some(dev) = mpos::drivers::virtio::net::VirtioNet::init() else {
        println!("net: no virtio-net device on the bus; sockets answer ENETDOWN");
        return;
    };
    mpos::net::init(dev);
    mpos::net::spawn_service();
    println!("net: virtio-net up at 10.0.2.15/24, gateway 10.0.2.2");
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

    let inputs = mpos::drivers::virtio::input::init();
    if inputs.is_empty() {
        println!("virtio-input: no device on the bus");
    }
    for (index, (_node, name)) in inputs.iter().enumerate() {
        println!("virtio-input: event{index}: {name}");
    }

    if let Some(snd) = mpos::drivers::virtio::sound::init_global() {
        let (rate, channels) = snd.format();
        println!("virtio-sound: {rate} Hz, {channels} channels at /dev/dsp");
    }

    mount_share();

    match mpos::drivers::virtio::vsock::VirtioVsock::init() {
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

/// Tell the scheduler's exit path what a dying process's descriptors own.
///
/// Each of these is a *per-open-file resource*: state whose lifetime is exactly
/// "some process holds this descriptor", which has to be given up at the moment
/// the last reference goes - `do_exit`'s moment, not the reaper's. The
/// scheduler owns tasks and none of these, so each subsystem registers here
/// instead of being named by the teardown, and this function is the one place
/// that says who they are (`PLAN.md` §11.1.3, V-3).
///
/// **The order is the order the resources have to be released in**, because the
/// registry runs hooks in registration order: record locks first (they are keyed
/// by `(device, inode)` and the inode is reached through a descriptor), then the
/// state keyed by the open file description - the socket, the inotify watch, the
/// evdev reader position, and the uinput device a program made for itself. Each
/// hook runs before the batch is dropped, while the reference counts still say
/// which description is really the last one.
///
/// The `FD_CLOEXEC` registry is the `execve` counterpart, and it is deliberately
/// shorter: only the two whose reader position is keyed by an address the fresh
/// image reuses are told when the process *lives on*, while locks, the socket and
/// the inotify watch have their own close-time handling and stay exit-only. Same
/// hooks, registered into the second list, so `sched` never names `drivers` for
/// the cloexec pass.
fn register_exit_hooks() {
    mpos::sched::exit::register(mpos::vfs::locks::release_on_process_exit);
    mpos::sched::exit::register(mpos::net::socket::close_sockets_at_exit);
    mpos::sched::exit::register(mpos::vfs::inotify::close_at_exit);
    mpos::sched::exit::register(mpos::drivers::virtio::input::close_at_exit);
    mpos::sched::exit::register(mpos::drivers::uinput::close_at_exit);

    mpos::sched::exit::register_cloexec(mpos::drivers::virtio::input::close_at_exit);
    mpos::sched::exit::register_cloexec(mpos::drivers::uinput::close_at_exit);
}

/// The host share, mounted by the boot the way the data partition is: the
/// boot does it because a share with no mountpoint is a device nobody can
/// reach, and `/mnt/share` is the name. A machine with no share - or one
/// whose tag the host never exported - skips the step with a line, which is
/// a quieter boot rather than a failed one.
fn mount_share() {
    let Some(dev) = mpos::drivers::virtio::fs9p::devices().into_iter().next() else {
        println!("virtio-9p: no share on the bus");
        return;
    };
    let tag = dev.mount_tag();
    let fs = match mpos::vfs::p9::mount(tag) {
        Ok(fs) => fs,
        Err(e) => {
            println!("virtio-9p: \"{tag}\" refused to open: {e:?}");
            return;
        }
    };
    let root = vfs::resolve("/").expect("no root filesystem");
    // /mnt exists - `init_blk` made it - and the share hangs under its own
    // name, made here if the mount is the first thing to want it.
    let mnt = root.lookup("mnt").expect("the boot made /mnt");
    if mnt.lookup("share").is_err()
        && let Err(e) = mnt.create("share", NodeKind::Dir)
    {
        println!("virtio-9p: /mnt/share could not be made: {e:?}");
        return;
    }
    // The filesystem is held by `Arc` - its inodes hold it by `Weak`, so a
    // share that outlives its mount is answered rather than crashed - and
    // the mount API takes a filesystem by trait: one deref bridges the two.
    match vfs::mount("/mnt/share", &*fs) {
        Ok(()) => println!("virtio-9p: \"{tag}\" mounted at /mnt/share"),
        Err(e) => println!("virtio-9p: \"{tag}\" did not mount: {e:?}"),
    }
}

/// M8: a real system start. The checks above are the kernel proving itself;
/// this is the kernel handing the machine over.
fn handoff_to_init(info: &BootInfo) {
    let init = vfs::read_file("/init").expect("the initramfs carries /init");
    let argv = alloc::vec![String::from("/init")];
    let pid = sched::spawn_init(&init, &argv, &[]).expect("the init image loaded");
    assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1");

    // The deepest the boot stack ever gets: `spawn_init` parsed an ELF and built
    // a process's address space and stack on top of everything above. Checking
    // here rather than after the handoff is the point - once init runs, the trap
    // that lands on an overrun stack is the one that cannot report it.
    check_boot_stack(info, "under spawn_init");

    println!("mpos: handing the machine to /init");

    sched::discard_exit_code(pid);

    // The mux owns the serial input from here on: every byte typed at COM1
    // belongs to the active virtual console, and the six consoles' output is
    // carried back over the same wire. It goes up before the receiver is
    // enabled, so no keystroke can arrive with nowhere to go.
    vfs::vcmux::init();
    drivers::serial::enable_rx();
    // And the other direction: from here the console queues into the transmit
    // ring and lets the UART's own interrupt drain it, instead of the writer
    // waiting for the hardware with the clock and the keyboard masked off.
    drivers::serial::enable_tx();
    // init forks, execve's the shell in the child, and waits. So a child
    // appearing under init is exactly the "the shell is up" signal - and init
    // itself staying alive is what makes that a shell and not a machine with
    // nobody left to start programs.
    while !sched::has_live_child(pid) {
        sched::sleep_ticks(1);
    }
    println!("M8_OK");
    park()
}

/// Where the boot task spends the rest of the machine's life.
///
/// It *blocks* here rather than halting on the spot, and the difference is the
/// whole of the wake latency. `wait_for_interrupt` from a task stops the core
/// rather than giving it up: this task stays runnable, so the picker hands it
/// the CPU, and a task that has halted can only be taken off it by an
/// interrupt - of which the only one that reliably arrives is the clock. Any
/// wake that lands behind it in the ready queue therefore waits for the next
/// tick, which is 4 ms at this build's 250 Hz, on every blocking call in the
/// system. Parking in the scheduler instead leaves the idle halt where the rest
/// of the kernel already assumes it is - in the scheduler's own loop, where a
/// wake is followed by a turn and not by a tick.
fn park() -> ! {
    loop {
        sched::block_until(|| false);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // A panic must reach the host on the wire even if the console had moved to
    // the framebuffer, and must not park on a transmit ring mid-report.
    mpos::console::force_serial();
    println!("\nKERNEL PANIC: {}", info);
    stack_scan();
    drivers::qemu::exit(drivers::qemu::EXIT_FAILURE)
}

/// How much of the running task's kernel stack is in use, and whether it is
/// still inside its own block at all.
///
/// A wild write and a stack that ran off its bottom look the same from the
/// place the damage is noticed - a collection's bookkeeping is wrong - and
/// they want opposite investigations. The one number that separates them is
/// how far the stack pointer has come down from the top of the block it was
/// given: a stack still thousands of bytes above its base did not overflow,
/// and one sitting at or below the base did.
#[cfg(target_arch = "x86_64")]
fn report_stack_use() {
    let mut rsp: usize;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem)) };
    match mpos::sched::current_stack_range() {
        Some((base, top)) => println!(
            "  kernel stack: rsp {rsp:#x} of [{base:#x}..{top:#x}), {} bytes in use, \
             {} bytes to spare",
            top.saturating_sub(rsp),
            rsp.saturating_sub(base)
        ),
        None => println!("  kernel stack: rsp {rsp:#x}, not a task stack"),
    }
}

/// Dump the return addresses sitting on the stack above the panic handler.
///
/// A panic names where core *noticed* the problem, which is rarely where it
/// was caused: `slice::get_unchecked` fires deep inside a collection whose
/// bookkeeping a write somewhere else broke, and the location line points at
/// core, not at the culprit. The stack is the only trail back, and the words
/// worth following are the ones in the kernel's own address range - return
/// addresses into code, which `rip_symbol.py` turns into a function name and
/// an offset. Everything else on the stack is data, so it is skipped rather
/// than dumped as noise.
#[cfg(target_arch = "x86_64")]
fn stack_scan() {
    const KERNEL_TEXT: usize = 0xffff_ffff_8000_0000;
    report_stack_use();
    mpos::syscall::trail::dump();
    let mut rsp: usize;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem)) };
    println!("  stack from {rsp:#x}, kernel-text words only:");
    let words = unsafe { core::slice::from_raw_parts(rsp as *const u64, 64) };
    for (i, word) in words.iter().enumerate() {
        let value = *word as usize;
        if value >= KERNEL_TEXT {
            println!("    rsp+{:3} {:#018x}", i * 8, value);
        }
    }
}
