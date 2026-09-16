//! The kernel image: boot entry, milestone acceptance checks, panic handler.
//!
//! Everything reusable lives in the `mpos` library so it can be unit-tested on
//! the host; this binary is only the parts that must run on the machine.
//!
//! # Boot flow
//!
//! Boot proceeds in three phases:
//!
//! 1. **Initialization** — `init_mm → init_arch → init_sched → init_vfs →
//!    init_blk → init_ipc → init_shell`. Each sets up a subsystem without
//!    running tests. After all inits complete, the system is ready.
//!
//! 2. **Milestone pre-checks** — `check_mm → check_arch → check_sched →
//!    check_vfs → check_blk_process → check_ipc → check_shell`. Each
//!    validates the corresponding subsystem. Groups are ordered by dependency
//!    and print `M*_OK` markers the CI harness asserts on.
//!
//! 3. **Init handoff** — `m13` spawns `/sbin/init` and parks the boot task.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::Layout;
use core::panic::PanicInfo;
use core::ptr;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use mpos::arch::{self, interrupts, paging, timer};
use mpos::blk;
use mpos::boot::{self, BootInfo};
use mpos::drivers;
use mpos::mm::{self, MapFlags, Mapping, PAGE_SIZE};
use mpos::println;
use mpos::sched::{self, Priority};
use mpos::syscall::{self, err, linux::nr as lnr};
use mpos::vfs::{self, FileSystem, Inode, NodeKind};

const MIB: usize = 1024 * 1024;

/// The virtio-blk device, probed once in init_blk and reused everywhere.
static BLK_DEV: spin::Once<alloc::sync::Arc<dyn mpos::blk::BlockDev>> = spin::Once::new();
/// The raw virtio-blk device (before partitioning), for partition lookups.
/// The data partition device (partition 4), mounted at /mnt after pivot_root.
static DATA_DEV: spin::Once<alloc::sync::Arc<dyn mpos::blk::BlockDev>> = spin::Once::new();

// ---------------------------------------------------------------------------
// Boot entry
// ---------------------------------------------------------------------------

/// Limine jumps here in 64-bit long mode, ring 0, paging enabled, interrupts
/// disabled, on a bootloader-provided stack.
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Serial first: it depends on no bootloader response, so it is usable even
    // if Limine rejected our base revision and `collect` panics below.
    mpos::console::init();

    // Enable SSE/AVX for user processes before anything else touches CR0/CR4.
    arch::enable_user_sse();

    let info = boot::limine::collect();
    banner(&info);

    let stats = unsafe { mm::init(&info) };
    println!(
        "pmm: {} zones, {} frames ({} MiB)",
        stats.zones,
        stats.total,
        stats.total * PAGE_SIZE / MIB
    );

    kernel_main(&info);
    // The system never stops booting: `kernel_main` ends parked in the boot
    // task, with init and its shell running. If it ever returned, the stack
    // underneath is the bootloader's and there is nothing left to do.
    arch::halt()
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
/// to init and stand aside. It ends inside `m13`, which parks the boot task
/// rather than returning - a system that stops running itself is not a system.
fn kernel_main(info: &BootInfo) {
    // ---- phase 1: initialization (no tests, just set up subsystems) --------
    init_mm();
    init_arch(info);
    init_sched();
    init_vfs(info);
    init_blk();
    // After `init_arch`, not before: the seed mixes in the boot instant the
    // RTC reported and the tick counter, and neither exists until the clock
    // subsystem is up. Seeding later costs nothing - `fill` seeds on first use
    // anyway - but seeding here means the very first reader gets a stream
    // seeded from the machine rather than from a cold counter.
    mpos::rand::init();
    println!("系统就绪");

    // ---- phase 2: milestone pre-checks (pure tests, assume init done) ------
    check_mm(info);
    check_arch(info);
    check_sched();
    check_vfs();
    check_blk_process();
    check_ipc();
    // The network comes up before the shell sweep rather than after it: the
    // sweep ends with the tests that leave the machine - a DNS query, a TCP
    // connection - and none of them can pass on a stack that is not up yet.
    // The service task this starts is permanent, so the drains the checks run
    // do not wait for a task that is never going to finish.
    init_net();
    check_shell();
    println!("全部通过");

    // ---- phase 3: hand the machine to /sbin/init ----------------------------
    handoff_to_init();
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
    arch::init_interrupts();
    interrupts::enable();

    let sections = arch::kernel_sections();
    println!(
        "image: text {} KiB, rodata {} KiB, data {} KiB",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024
    );

    let tables = unsafe { arch::paging::install(info, &sections) };
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

/// The Alpine release the root filesystem is built from, as
/// `/etc/alpine-release` spells it. Kept in step with `scripts/fetch-alpine.sh`
/// by hand: the kernel asserts on it, so a silent version bump in the packaging
/// shows up here rather than as a userland that is not the one that was tested.
const ALPINE_RELEASE: &str = "3.24.1";

fn init_vfs(info: &BootInfo) {
    vfs::file::init_standard(vfs::tty::init());

    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(&*root);

    let archive = info.initramfs.expect("the boot config loaded no initramfs");
    let members = vfs::tar::unpack_into(vfs::global(), archive).expect("the initramfs was broken");
    println!("initramfs: {members} members unpacked");

    let dev = drivers::devices::standard_devfs();
    vfs::mount("/dev", &*dev).expect("mount /dev");
}

fn init_blk() {
    let dev = drivers::virtio::blk::VirtioBlk::init()
        .expect("no virtio-blk device answered on the PCI bus");
    let sectors = dev.capacity_sectors();
    println!(
        "virtio-blk: {} sectors ({} MiB)",
        sectors,
        sectors * 512 / MIB as u64
    );
    blk::set_raw_disk(dev.clone());

    // The disk carries a partition table now, and *two* ext2 partitions: the
    // rootfs (which `/init` mounts) and the data partition (which this check
    // works against - its seed files are laid down by mke2fs at image build
    // time). The data one is picked by its position in the table, not by
    // "whichever ext2 comes first": with a full rootfs on the disk, first
    // found is the root, and the check would be reading the wrong disk.
    let dev = blk::part::find_data(dev).expect("no data partition on the disk");
    println!(
        "ext2: filesystem on a {}-sector partition",
        dev.capacity_sectors()
    );

    let root = vfs::resolve("/").expect("no root filesystem");
    if root.lookup("mnt").is_err() {
        root.create("mnt", NodeKind::Dir).expect("create /mnt");
    }
    let fs = vfs::ext2::Ext2Fs::mount(dev.clone()).expect("the ext2 image did not mount");
    vfs::mount("/mnt", &fs).expect("mount /mnt");
    // Both handles name the data partition now: BLK_DEV for the checks that
    // read through the mounted tree, DATA_DEV for the ones that remount the
    // raw device to prove a write reached the disk.
    DATA_DEV.call_once(|| dev.clone());
    BLK_DEV.call_once(|| dev);
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
        let before = timer::ticks();
        timer::busy_wait_ms(500);
        let elapsed = timer::ticks() - before;
        println!("clock: {} ticks in ~500ms ({} Hz)", elapsed, timer::TICK_HZ);
        assert!((40..=60).contains(&elapsed), "timer tick rate is wrong");

        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("int3")
        };
        assert_eq!(interrupts::breakpoints_caught(), 1, "#BP was not caught");
        println!("breakpoint caught and returned from");

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
                let (phys, flags) = arch::paging::translate(virt)
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
        let (back, flags) = arch::paging::translate(info.phys_to_virt(frame))
            .expect("the direct map is missing a frame the kernel owns");
        assert_eq!(back, frame, "the direct map translates to the wrong frame");
        assert_eq!(flags, MapFlags::READ | MapFlags::WRITE);
        mm::free_frames(frame, 0);

        assert!(
            arch::paging::translate(0).is_none(),
            "the null page is mapped"
        );
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
        let mut space = paging::AddressSpace::new();
        space.map(Mapping {
            virt,
            phys: frame,
            length: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        });
        let mapped_cost = before_mapped - mm::frame_stats().free;

        let before_empty = mm::frame_stats().free;
        let empty = paging::AddressSpace::new();
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
            assert_eq!(
                paging::active_root(),
                space.root(),
                "switch() did not switch"
            );

            let (back, flags) = paging::translate(virt).expect("the user page is not mapped");
            assert_eq!(back, frame, "the user page points at the wrong frame");
            assert_eq!(flags, MapFlags::READ | MapFlags::WRITE | MapFlags::USER);
            unsafe {
                ptr::write_bytes(virt as *mut u8, 0x77, PAGE_SIZE);
                assert_eq!(*(virt as *const u8), 0x77);
                assert_eq!(*(virt as *const u8), 0x77);
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
                paging::translate(virt).is_none(),
                "a user page leaked between address spaces"
            );
            let text = arch::kernel_sections().text.start;
            assert!(
                paging::translate(text).is_some(),
                "the kernel half was not shared with the new address space"
            );
            println!("second address space: kernel half shared, user half empty");

            paging::switch_to_kernel();
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
            let start = timer::ticks();
            while timer::ticks() - start < SPIN_TICKS {
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
            let start = timer::ticks();
            sched::sleep_ticks(SLEEP_TICKS);
            let slept = timer::ticks() - start;
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
            let start = timer::ticks();
            while timer::ticks() - start < SPIN_TICKS {
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
        ensure_child(
            &vfs::resolve("/etc").unwrap(),
            "mpos-hostname",
            NodeKind::File,
        );
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

    // M8: initramfs unpack and device filesystem.
    {
        // The proof that the archive unpacked used to be "the motd says what we
        // put in it". The root filesystem is Alpine's now, so the motd is
        // Alpine's - and the stronger claim is that a file no one but Alpine
        // ships is there with exactly the release it was fetched at.
        let mut buf = [0u8; 64];
        let release = vfs::open("/etc/alpine-release")
            .expect("the unpacked tree carries no /etc/alpine-release");
        let n = vfs::read(release, &mut buf).unwrap();
        vfs::close(release).unwrap();
        assert_eq!(
            core::str::from_utf8(&buf[..n])
                .expect("alpine-release is utf8")
                .trim(),
            ALPINE_RELEASE,
            "the unpacked tree is not the Alpine release that was packaged"
        );

        // And the motd is readable, which is what this check always really
        // cared about: a file's contents came out of the archive intact.
        let motd = vfs::open("/etc/motd").expect("no /etc/motd after the unpack");
        let n = vfs::read(motd, &mut buf).unwrap();
        vfs::close(motd).unwrap();
        assert!(n > 0, "the unpacked /etc/motd is empty");
        println!(
            "motd: {}",
            core::str::from_utf8(&buf[..n])
                .expect("motd is utf8")
                .lines()
                .next()
                .unwrap_or("")
        );

        let console = vfs::open("/dev/console").expect("open /dev/console");
        let line = b"this line went through /dev/console\n";
        assert_eq!(vfs::write(console, line).unwrap(), line.len());
        vfs::close(console).unwrap();

        let null = vfs::open("/dev/null").expect("open /dev/null");
        assert_eq!(vfs::write(null, b"gone").unwrap(), 4);
        assert_eq!(vfs::read(null, &mut buf).unwrap(), 0, "null read is EOF");
        vfs::close(null).unwrap();

        let zero = vfs::open("/dev/zero").expect("open /dev/zero");
        let mut zeros = [1u8; 8];
        assert_eq!(vfs::read(zero, &mut zeros).unwrap(), 8);
        assert!(zeros.iter().all(|&b| b == 0), "zero read nonzero");
        vfs::close(zero).unwrap();

        println!("M8_OK");
    }
}

// ---- blk + user processes: ELF loading, virtio-blk, ext2 -----------------

/// The exit code the user program ends with.
const HELLO_EXIT: isize = 42;

/// Prove user processes (ELF loading, ring 3) and the block layer (virtio-blk,
/// ext2 read/write/persistence).
fn check_blk_process() {
    // M9: user processes — load /bin/hello, enter ring 3, exit with code 42.
    {
        let mut program = Vec::new();
        let fd = vfs::open("/bin/hello").expect("the initramfs carries no /bin/hello");
        let mut chunk = [0u8; 512];
        loop {
            let n = vfs::read(fd, &mut chunk).expect("read /bin/hello");
            if n == 0 {
                break;
            }
            program.extend_from_slice(&chunk[..n]);
        }
        vfs::close(fd).unwrap();
        println!("initramfs: /bin/hello is {} bytes of ELF", program.len());

        let id = sched::spawn_user(&program, Priority::NORMAL);
        wait_for_code(id);
        assert_eq!(
            sched::exit_code(id),
            Some(HELLO_EXIT),
            "hello did not end with the exit code it chose"
        );
        drain();
        assert_eq!(
            sched::task_count(),
            sched::permanent_tasks(),
            "the user task was not reaped"
        );
        println!("M9_OK");
    }

    // M10: virtio-blk and ext2.
    {
        const SEED: &[u8] = b"ext2 seed: formatted by mke2fs on the host, read by mpos\n";
        let fd = vfs::open("/mnt/seed.txt").expect("open /mnt/seed.txt");
        let mut buf = [0u8; 128];
        let n = vfs::read(fd, &mut buf).expect("read /mnt/seed.txt");
        vfs::close(fd).unwrap();
        assert_eq!(&buf[..n], SEED);

        let fd = vfs::open("/mnt/docs/note.txt").expect("open /mnt/docs/note.txt");
        let mut buf = [0u8; 64];
        let n = vfs::read(fd, &mut buf).expect("read /mnt/docs/note.txt");
        vfs::close(fd).unwrap();
        assert_eq!(&buf[..n], b"nested file\n");
        println!("ext2: host-seeded files read back through /mnt");

        vfs::resolve("/mnt")
            .unwrap()
            .create("written.bin", NodeKind::File)
            .expect("create /mnt/written.bin");
        const LEN: usize = 20 * 1024;
        let bytes: Vec<u8> = (0..LEN).map(|i| (i * 31 + 7) as u8).collect();
        let fd = vfs::open("/mnt/written.bin").expect("open /mnt/written.bin");
        assert_eq!(
            vfs::write(fd, &bytes).unwrap(),
            LEN,
            "the write came up short"
        );
        vfs::close(fd).unwrap();

        let dev = DATA_DEV.get().unwrap().clone();
        let fs2 = vfs::ext2::Ext2Fs::mount(dev.clone()).expect("remount the ext2 image");
        let back = fs2
            .root()
            .lookup("written.bin")
            .expect("written.bin persisted");
        assert_eq!(back.metadata().size as usize, LEN);
        let mut read_back = vec![0u8; LEN];
        let n = back
            .read_at(0, &mut read_back)
            .expect("read written.bin back");
        assert_eq!(n, LEN);
        assert!(
            read_back
                .iter()
                .enumerate()
                .all(|(i, &b)| b == (i * 31 + 7) as u8),
            "written.bin did not read back the pattern it was written with"
        );

        let seed = fs2.root().lookup("seed.txt").expect("seed.txt survived");
        let mut buf = [0u8; 128];
        let n = seed.read_at(0, &mut buf).expect("read seed.txt back");
        assert_eq!(&buf[..n], SEED);

        vfs::resolve("/mnt")
            .unwrap()
            .unlink("written.bin")
            .expect("unlink /mnt/written.bin");
        let dev = DATA_DEV.get().unwrap().clone();
        let fs3 = vfs::ext2::Ext2Fs::mount(dev).expect("remount after the unlink");
        assert_eq!(
            fs3.root().lookup("written.bin").err(),
            Some(vfs::VfsError::NoEntry),
            "written.bin is still on the disk after the unlink"
        );

        println!("ext2: {LEN} bytes written through /mnt, verified on a cold remount and removed");
        println!("M10_OK");
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
    let n = tty.read(&mut line);
    assert_eq!(&line[..n], b"hello\n", "backspace did not edit the line");

    for &b in b"gate\n\x04" {
        tty.receive_byte(b);
    }
    let n = tty.read(&mut line);
    assert_eq!(&line[..n], b"gate\n");
    let n = tty.read(&mut line);
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
    assert_eq!(
        unsafe { syscall(lnr::KILL, 0, 32, 0, 0, 0, 0) },
        err::EINVAL
    );
    println!("signals: SIGTERM, SIGKILL, and the kill(0) probe behave");

    println!("M11_OK");
}

// ---- shell + init: coreutils, pipelines, redirection, init handoff ------

/// The script the gate types at the shell, one byte at a time.
const SHELL_SCRIPT: &[&[u8]] = &[
    b"echo hello from the shell\n",
    b"cat /etc/motd\n",
    b"ls /\n",
    b"ls /bin\n",
    b"true\n",
    b"false\n",
    b"echo M12_OK\n",
    b"echo M13_'PIPE' | cat\n",
    b"echo M13_'REDIR' > /tmp/gate.txt\n",
    b"cat /tmp/gate.txt\n",
    b"GATE=shell\n",
    b"echo M13_$GATE\n",
    b"false\n",
    b"echo M14_$?\n",
    // A background job: the line ends in `&`, so the shell must answer 0
    // straight away rather than wait - and the child must still be collected,
    // which the job list does before the next prompt.
    b"sleep 1 &\n",
    b"echo M13_BG_$?\n",
    // The conformance suite: one test per syscall, each leaving its verdict
    // in $? - 0 pass, 1 fail - echoed back as a greppable marker. Every line
    // must read `<name>_0`; the one exception is t_exit_group, whose whole
    // contract is the distinctive code it leaves: `<name>_42`.
    //
    // Process identity first (cheap, no filesystem side effects), then file
    // I/O, then the fs namespace mutators (each cleans up after itself), then
    // memory, then signals/time, then the exec/fork pair that replaces or
    // multiplies the running process.
    b"t_getpid\n",
    b"echo t_getpid_$?\n",
    b"t_gettid\n",
    b"echo t_gettid_$?\n",
    b"t_getuid\n",
    b"echo t_getuid_$?\n",
    b"t_uname\n",
    b"echo t_uname_$?\n",
    b"t_arch_prctl\n",
    b"echo t_arch_prctl_$?\n",
    b"t_getrandom\n",
    b"echo t_getrandom_$?\n",
    b"t_sched_getaffinity\n",
    b"echo t_sched_getaffinity_$?\n",
    b"t_set_robust_list\n",
    b"echo t_set_robust_list_$?\n",
    b"t_rseq\n",
    b"echo t_rseq_$?\n",
    b"t_set_tid_address\n",
    b"echo t_set_tid_address_$?\n",
    b"t_openat\n",
    b"echo t_openat_$?\n",
    b"t_read\n",
    b"echo t_read_$?\n",
    b"t_write\n",
    b"echo t_write_$?\n",
    b"t_lseek\n",
    b"echo t_lseek_$?\n",
    b"t_readv\n",
    b"echo t_readv_$?\n",
    b"t_writev\n",
    b"echo t_writev_$?\n",
    b"t_fstat\n",
    b"echo t_fstat_$?\n",
    b"t_newfstatat\n",
    b"echo t_newfstatat_$?\n",
    b"t_statx\n",
    b"echo t_statx_$?\n",
    b"t_access\n",
    b"echo t_access_$?\n",
    b"t_faccessat\n",
    b"echo t_faccessat_$?\n",
    b"t_readlink\n",
    b"echo t_readlink_$?\n",
    b"t_fcntl\n",
    b"echo t_fcntl_$?\n",
    b"t_dup\n",
    b"echo t_dup_$?\n",
    b"t_dup2\n",
    b"echo t_dup2_$?\n",
    b"t_dup3\n",
    b"echo t_dup3_$?\n",
    b"t_ioctl\n",
    b"echo t_ioctl_$?\n",
    b"t_getdents64\n",
    b"echo t_getdents64_$?\n",
    b"t_pipe2\n",
    b"echo t_pipe2_$?\n",
    b"t_mkdirat\n",
    b"echo t_mkdirat_$?\n",
    b"t_renameat2\n",
    b"echo t_renameat2_$?\n",
    // The same call, but reached the way a package manager reaches it: both
    // names bare, both relative to a directory descriptor. `t_renameat2`
    // above only ever passes `AT_FDCWD` with absolute paths, which is the
    // shape that hides the bug this one pins.
    b"t_rename\n",
    b"echo t_rename_$?\n",
    // Names that are made and taken away by their bare name, and the `..` that
    // has to actually go up: the shapes a shell needs before it can do anything
    // relative at all.
    b"t_rmdir\n",
    b"echo t_rmdir_$?\n",
    b"t_unlinkat\n",
    b"echo t_unlinkat_$?\n",
    b"t_getcwd\n",
    b"echo t_getcwd_$?\n",
    b"t_chdir\n",
    b"echo t_chdir_$?\n",
    b"t_brk\n",
    b"echo t_brk_$?\n",
    b"t_mmap\n",
    b"echo t_mmap_$?\n",
    b"t_mprotect\n",
    b"echo t_mprotect_$?\n",
    b"t_munmap\n",
    b"echo t_munmap_$?\n",
    b"t_mremap\n",
    b"echo t_mremap_$?\n",
    b"t_madvise\n",
    b"echo t_madvise_$?\n",
    b"t_rt_sigaction\n",
    b"echo t_rt_sigaction_$?\n",
    b"t_rt_sigprocmask\n",
    b"echo t_rt_sigprocmask_$?\n",
    b"t_sigaltstack\n",
    b"echo t_sigaltstack_$?\n",
    b"t_kill\n",
    b"echo t_kill_$?\n",
    b"t_clock_gettime\n",
    b"echo t_clock_gettime_$?\n",
    b"t_nanosleep\n",
    b"echo t_nanosleep_$?\n",
    b"t_poll\n",
    b"echo t_poll_$?\n",
    b"t_prlimit64\n",
    b"echo t_prlimit64_$?\n",
    b"t_futex\n",
    b"echo t_futex_$?\n",
    b"t_exit_group\n",
    b"echo t_exit_group_$?\n",
    b"t_execve\n",
    b"echo t_execve_$?\n",
    b"t_fork\n",
    b"echo t_fork_$?\n",
    b"t_wait4\n",
    b"echo t_wait4_$?\n",
    b"t_clone\n",
    b"echo t_clone_$?\n",
    // The network's own tests, last and together: they are the only ones
    // that need the machine to have a wire, and a failure in them should
    // not hide a verdict from the calls before them. t_udp is the one that
    // leaves the machine at all - a DNS query to slirp's resolver and an
    // answer back - so it proves the stack against something that is not
    // this kernel.
    b"t_socket\n",
    b"echo t_socket_$?\n",
    b"t_sockopt\n",
    b"echo t_sockopt_$?\n",
    b"t_tcp\n",
    b"echo t_tcp_$?\n",
    b"t_udp\n",
    b"echo t_udp_$?\n",
    // The pair `wget` builds for its TLS helper. It needs no wire, but it
    // belongs with the socket tests: it is the other half of what
    // "a socket the userland can use" means, and the one where the descriptor
    // has to survive an `execve` for HTTPS to be possible at all.
    b"t_socketpair\n",
    b"echo t_socketpair_$?\n",
    // t_stdargs pins the Linux startup contract from the std side: a musl
    // binary's Rust `env::args()` reads back the argument block the kernel
    // laid out, with no mpos libc in the way. It is the only test that runs a
    // whole libc runtime of its own, and its first `malloc` is what caught the
    // `syscall` entry clobbering r12 - which turned `argc` into a stack
    // address and asked the kernel for a 24 GiB mapping.
    b"t_stdargs one two three\n",
    b"echo STDARGS_$?\n",
    b"t_stdspawn\n",
    b"echo STDSPAWN_$?\n",
    // apk is the one program on the image that is linked against musl's
    // *shared* libc, so running it exercises two things nothing else does:
    // the kernel loading a program interpreter named by PT_INTERP, and a
    // shared object arriving through a file `mmap`. It is also the only
    // milestone here that is a real Alpine program rather than one of ours.
    b"apk --version > /dev/null && echo APK_OK\n",
    // The entropy device, which is what a TLS stack on this userland reads and
    // the only thing it reads: busybox's HTTPS opens `/dev/urandom` and never
    // calls `getrandom(2)`. The marker pins that a read comes back with the
    // bytes that were asked for; that two reads *differ* is pinned by the
    // host unit tests over the same generator, where it can be said in Rust.
    b"echo URANDOM_$(dd if=/dev/urandom bs=16 count=1 2>/dev/null | wc -c)\n",
    // b"t_mprotect\n",
    // b"echo t_mprotect_$?\n",
    // b"t_rt_sigprocmask\n",
    // b"echo t_rt_sigprocmask_$?\n",
    // b"t_brk\n",
    // b"echo t_brk_$?\n",
    // b"t_write\n",
    // b"echo t_write_$?\n",
    // b"t_munmap\n",
    // b"echo t_munmap_$?\n",
    // b"t_exit_group\n",
    // b"echo t_exit_group_$?\n",
    // The coreutils round: the syscalls the coreutils strace showed beyond
    // the musl startup set - file I/O, stats, the glibc startup pair. Same
    // verdict protocol: `<name>_0`, nothing else.
    // b"t_openat\n",
    // b"echo t_openat_$?\n",
    // b"t_read\n",
    // b"echo t_read_$?\n",
    // b"t_lseek\n",
    // b"echo t_lseek_$?\n",
    // b"t_fstat\n",
    // b"echo t_fstat_$?\n",
    // b"t_newfstatat\n",
    // b"echo t_newfstatat_$?\n",
    // b"t_statx\n",
    // b"echo t_statx_$?\n",
    // b"t_access\n",
    // b"echo t_access_$?\n",
    // b"t_fcntl\n",
    // b"echo t_fcntl_$?\n",
    // b"t_readlink\n",
    // b"echo t_readlink_$?\n",
    // b"t_getrandom\n",
    // b"echo t_getrandom_$?\n",
    // b"t_gettid\n",
    // b"echo t_gettid_$?\n",
    // b"t_prlimit64\n",
    // b"echo t_prlimit64_$?\n",
    // b"t_sched_getaffinity\n",
    // b"echo t_sched_getaffinity_$?\n",
    // b"t_set_robust_list\n",
    // b"echo t_set_robust_list_$?\n",
    // b"t_rseq\n",
    // b"echo t_rseq_$?\n",
    b"\x04",
];

/// Prove the shell, coreutils, pipelines, and redirection in ring 3.
fn check_shell() {
    let shell = vfs::read_file("/bin/sh").expect("the initramfs carries /bin/sh");
    let argv = alloc::vec![String::from("sh")];
    let sh = sched::try_spawn_user(
        &shell,
        Priority::NORMAL,
        sched::current(),
        &argv,
        &[],
        &[],
        "/bin/sh",
    )
    .expect("the shell image loaded");

    sched::spawn(
        || {
            sched::sleep_ticks(2);
            for line in SHELL_SCRIPT {
                for &b in *line {
                    vfs::tty::global().receive_byte(b);
                    sched::sleep_ticks(1);
                }
            }
        },
        Priority::NORMAL,
    );

    wait_for_code(sh);
    let code = sched::exit_code(sh).expect("the shell exited");
    assert_eq!(code, 0, "the shell did not end cleanly: {code}");
    discard();
    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "a process the shell started was never reaped"
    );
    println!("shell: ran the script, read its end of input, exited {code}");
}

/// Wait for every task a milestone started, and forget the codes they left.
fn discard() {
    discard_codes();
    drain();
}

// ---- init handoff (terminal, not a check) --------------------------------

/// Bring the network stack up.
///
/// Called once, before the shell sweep, because the sweep's last tests are the
/// ones that need a wire. The task this starts lives as long as the machine
/// does - it is what keeps a connection moving while its owner is blocked -
/// which is why it is started permanent rather than by `spawn`.
fn init_net() {
    if mpos::net::init() {
        mpos::net::spawn_service();
        println!("net: virtio-net up at 10.0.2.15/24, gateway 10.0.2.2");
    } else {
        println!("net: no virtio-net device on the bus; sockets answer ENETDOWN");
    }
}

/// M13: a real system start. The checks above are the kernel proving itself;
/// this is the kernel handing the machine over.
fn handoff_to_init() {
    let init = vfs::read_file("/init").expect("the initramfs carries /init");
    let argv = alloc::vec![String::from("/init")];
    let pid = sched::spawn_init(&init, &argv, &[]).expect("the init image loaded");
    assert_eq!(pid, sched::INIT_PID, "the first process must be pid 1");
    println!("mpos: handing the machine to /sbin/init");

    sched::discard_exit_code(pid);

    drivers::serial::enable_rx();
    // init forks, execve's the shell in the child, and waits. So a child
    // appearing under init is exactly the "the shell is up" signal - and init
    // itself staying alive is what makes that a shell and not a machine with
    // nobody left to start programs.
    while !sched::has_live_child(pid) {
        sched::sleep_ticks(1);
    }
    println!("M13_OK");
    park()
}

/// Where the boot task spends the rest of the machine's life.
fn park() -> ! {
    loop {
        sched::wait_for_interrupt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nKERNEL PANIC: {}", info);
    drivers::qemu::exit(drivers::qemu::EXIT_FAILURE)
}
