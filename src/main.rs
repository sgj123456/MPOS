//! The kernel image: boot entry, milestone acceptance checks, panic handler.
//!
//! Everything reusable lives in the `mpos` library so it can be unit-tested on
//! the host; this binary is only the parts that must run on the machine.

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

use mpos::arch::{self, interrupts, paging, qemu, timer};
use mpos::blk;
use mpos::boot::{self, BootInfo};
use mpos::mm::{self, MapFlags, Mapping, PAGE_SIZE};
use mpos::println;
use mpos::sched::{self, Priority};
use mpos::syscall::{self, err, nr};
use mpos::vfs::{self, FileSystem, NodeKind};

const MIB: usize = 1024 * 1024;

/// Limine jumps here in 64-bit long mode, ring 0, paging enabled, interrupts
/// disabled, on a bootloader-provided stack.
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Serial first: it depends on no bootloader response, so it is usable even
    // if Limine rejected our base revision and `collect` panics below.
    mpos::console::init();

    let info = boot::limine::collect();
    banner(&info);

    let stats = unsafe { mm::init(&info) };
    println!(
        "pmm: {} zones, {} frames ({} MiB)",
        stats.zones,
        stats.total,
        stats.total * PAGE_SIZE / MIB
    );

    kernel_main(&info)
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

fn kernel_main(info: &BootInfo) -> ! {
    m0();
    m1();
    m2(info);
    m3(info);
    m4();
    m6();
    m7();
    m8(info);
    m9();
    m10();
    m11();
    m12();
    qemu::exit(qemu::EXIT_SUCCESS)
}

/// The global allocator works end to end.
fn m0() {
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
    println!("M0_OK");
}

/// The clock interrupts us, and a deliberate exception is catchable rather than
/// always fatal.
fn m1() {
    arch::init_interrupts();
    interrupts::enable();

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

/// Memory: the frame allocator hands out disjoint blocks and merges freed buddies
/// back together, and the heap on top of it honours every layout the allocator
/// contract allows.
fn m2(info: &BootInfo) {
    frames(info);
    heap();

    // The heap is the only consumer of frames so far, so the two allocators must
    // agree to the byte about how much memory is out.
    let frames = mm::frame_stats();
    let heap = mm::heap_stats();
    assert_eq!(
        (frames.total - frames.free) * PAGE_SIZE,
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

fn frames(info: &BootInfo) {
    let stats = mm::frame_stats();
    println!(
        "pmm: {} of {} frames free ({} MiB)",
        stats.free,
        stats.total,
        stats.free * PAGE_SIZE / MIB
    );
    assert!(stats.zones > 0, "no memory zones were built");

    let a = mm::alloc_frames(0).expect("out of order-0 frames");
    let b = mm::alloc_frames(0).expect("out of order-0 frames");
    assert_eq!(b, a + PAGE_SIZE, "expected the halves of one block");

    // Write through the HHDM at both ends of each frame: a bad translation or an
    // unbacked frame faults right here instead of later, quietly.
    for phys in [a, b] {
        let virt = info.phys_to_virt(phys);
        unsafe {
            ptr::write_bytes(virt as *mut u8, 0xa5, PAGE_SIZE);
            assert_eq!(*(virt as *const u8), 0xa5);
            assert_eq!(*((virt + PAGE_SIZE - 1) as *const u8), 0xa5);
        }
    }
    println!("two frames written and read back through the hhdm");

    mm::free_frames(a, 0);
    mm::free_frames(b, 0);
    assert_eq!(mm::frame_stats().free, stats.free, "frames leaked");

    // Freeing the pair must have merged them into one order-1 block; without a
    // merge this allocation would land somewhere else entirely.
    assert_eq!(mm::alloc_frames(1), Some(a), "buddies did not merge");
    mm::free_frames(a, 1);
    assert_eq!(mm::frame_stats().free, stats.free, "frames leaked");

    // A block big enough that it can only come from the zone's largest orders.
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
}

fn heap() {
    let baseline = mm::heap_stats();
    let mut live: Vec<(*mut u8, Layout)> = Vec::new();

    // Every size class, each at several alignments that still belong to it.
    for class in [16, 32, 64, 128, 256, 512, 1024, 2048] {
        for align in [1, 8, 16, class] {
            let layout =
                Layout::from_size_align(class / 2 + 1, align).expect("layout is representable");
            take(&mut live, layout);
        }
    }

    // Requests that must bypass the slabs: too big, or aligned past any class.
    for layout in [
        Layout::from_size_align(4096, 8).unwrap(),
        Layout::from_size_align(3000, 8192).unwrap(),
        Layout::from_size_align(1 << 20, 4096).unwrap(),
    ] {
        take(&mut live, layout);
    }

    // Growing a vector walks the realloc path through several classes and into
    // the large path, which is how the rest of the kernel will use the heap.
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

/// Paging: the kernel has left the bootloader's page tables for its own, and
/// processes can have address spaces of their own that share the kernel half.
fn m3(info: &BootInfo) {
    kernel_tables(info);
    address_spaces();
    println!("M3_OK");
}

/// The kernel runs on page tables it built itself, and the permissions in them are
/// the ones the mapping plan asked for.
fn kernel_tables(info: &BootInfo) {
    let sections = arch::kernel_sections();
    println!(
        "image: text {} KiB, rodata {} KiB, data {} KiB",
        (sections.text.end - sections.text.start) / 1024,
        (sections.rodata.end - sections.rodata.start) / 1024,
        (sections.data.end - sections.data.start) / 1024
    );

    let free_before = mm::frame_stats().free;
    let tables = unsafe { arch::paging::install(info, &sections) };
    let free_after = mm::frame_stats().free;
    assert_eq!(
        free_before - free_after,
        tables,
        "the page tables took a different number of frames than they reported"
    );
    println!(
        "page tables: {} frames ({} KiB)",
        tables,
        tables * PAGE_SIZE / 1024
    );

    // Getting past the switch is the first proof. The second is that memory only
    // the new tables map still works: growing a vector takes fresh frames and
    // writes them back through the direct map.
    let mut proof: Vec<u8> = Vec::new();
    proof.resize(64 * 1024, 0x3c);
    assert!(
        proof.iter().all(|&byte| byte == 0x3c),
        "the direct map is broken after the switch"
    );
    println!("64 KiB allocated and written after the switch");

    // W^X read back out of the hardware rather than trusted from the plan. Every
    // page, not just the ends: a huge page in the middle would otherwise carry the
    // wrong permissions unnoticed.
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

    // A frame the kernel owns, checked through the tables rather than the plan.
    let frame = mm::alloc_frames(0).expect("out of frames");
    let (back, flags) = arch::paging::translate(info.phys_to_virt(frame))
        .expect("the direct map is missing a frame the kernel owns");
    assert_eq!(back, frame, "the direct map translates to the wrong frame");
    assert_eq!(flags, MapFlags::READ | MapFlags::WRITE);
    mm::free_frames(frame, 0);

    // The lower half belongs to user processes, and the kernel's own address space
    // keeps it empty. A mapped null page would hide every null dereference.
    assert!(
        arch::paging::translate(0).is_none(),
        "the null page is mapped"
    );
    println!("direct map writable and non-executable, null page unmapped");

    // Limine's stack is still under us, and the frame allocator refuses to hand
    // out the region it sits in. If that ever stopped being true the symptom would
    // be a stack vanishing mid-function, so check it rather than trust the policy.
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
}

/// A process gets an address space of its own: its own lower half, the kernel's
/// upper half shared with it, and every table it took back when it goes away.
fn address_spaces() {
    let virt = 0x4000_0000;
    let frame = mm::alloc_frames(0).expect("out of frames");

    // Each space's cost is measured as it is built rather than guessed, so the
    // teardown check below is exact.
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

    // A root, then a PDPT, a PD and a PT to reach one 4 KiB leaf.
    assert_eq!(
        mapped_cost, 4,
        "a single user page needed {mapped_cost} tables"
    );
    // One root and nothing else: the kernel half is copied in as PML4 entries, so
    // sharing it costs no tables at all.
    assert_eq!(
        empty_cost, 1,
        "an empty address space needed {empty_cost} tables"
    );

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

    // The kernel half came along, which is the only reason the next line works:
    // the heap lives in the direct map, and this code is running from mapped text.
    let mut shared: Vec<u8> = Vec::new();
    shared.resize(8 * 1024, 0x11);
    assert!(
        shared.iter().all(|&byte| byte == 0x11),
        "the heap stopped working in a process address space"
    );
    drop(shared);

    // The other space shares the kernel half but none of this one's user pages.
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

    // Taken here, not at the top: the vector above grew the heap, and the slab
    // keeps freed runs for reuse instead of handing their frames back.
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
}

/// Which task ran, in the order they ran.
///
/// The checks below assert on this rather than on the log, because interleaving
/// is precisely the thing that has to be proven: a boot log that looks right is
/// not evidence that two tasks took turns.
static RECORD: [AtomicU8; 64] = [const { AtomicU8::new(0) }; 64];
static RECORD_LEN: AtomicUsize = AtomicUsize::new(0);

fn record(who: u8) {
    // One interrupts-off step. Claiming a slot and filling it are two
    // operations, and a task preempted between them would let the next task
    // record out of order - destroying the only thing this record measures.
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
///
/// Sleeping rather than yielding matters: the boot task runs at NORMAL, and the
/// policy is strict priority with no aging, so a boot task that busy-yielded
/// would sit ahead of any LOW worker forever and starve it. Going off the ready
/// queue lets every band make progress.
fn drain() {
    while sched::task_count() > sched::permanent_tasks() {
        sched::sleep_ticks(1);
    }
}

/// Two tasks at the same priority take turns, and the turn-taking is the whole
/// of round-robin: a task that yields goes to the back of its band and cannot
/// run again until everything queued ahead of it has.
fn alternating() {
    reset_record();
    // One interrupts-off window: if the clock preempted the boot task between
    // the two spawns, task 0 would run a slice before task 1 even existed.
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

    // The order is not fully deterministic: a tick can land between a task
    // being dispatched and its first `record`, and preempting then requeues it
    // behind its peer - the pair swaps and one slice doubles. Each such tick
    // costs exactly one repeat, so more than two repeats means the band is not
    // taking turns at all, which is the thing this check is for.
    let got = recorded();
    let repeats = got.windows(2).filter(|pair| pair[0] == pair[1]).count();
    assert!(
        repeats <= 2,
        "two tasks at the same priority did not take turns: {got:?}"
    );
    println!("two tasks alternated five slices each");
}

/// A task that never yields still loses the core.
///
/// This is the check that the scheduler is preemptive rather than cooperative:
/// nothing in task 0 gives the CPU up, so the only thing that can let task 1 run
/// is the clock interrupt.
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

    // Task 1 ran three times while task 0 was spinning, and task 0 records only
    // once it has stopped - so its entry is last.
    assert_eq!(
        recorded(),
        vec![1, 1, 1, 0],
        "a task that never yielded held the core to itself"
    );
    println!("a task that never yielded was preempted for {SPIN_TICKS} ticks");
}

/// A sleeping task is off the ready queue until the clock puts it back, and the
/// other task gets every slice in between.
fn sleeping() {
    reset_record();
    const SLEEP_TICKS: u64 = 10;

    sched::spawn(
        || {
            record(0);
            let start = timer::ticks();
            sched::sleep_ticks(SLEEP_TICKS);
            let slept = timer::ticks() - start;
            // Bounded above as well as below: a sleeper that overslept by
            // seconds would pass "at least", and the sleep queue is a deadline
            // heap for a reason. One tick of slack is the scheduler noticing on
            // its next turn rather than at the exact instant.
            assert!(
                (SLEEP_TICKS..SLEEP_TICKS + 3).contains(&slept),
                "asked for {SLEEP_TICKS} ticks and got {slept}"
            );
            record(0);
        },
        Priority::NORMAL,
    );
    // Give the sleeper a tick to reach its sleep before the recorder exists.
    // Spawned together, a tick landing between the sleeper's dispatch and its
    // first record would requeue it behind the recorder - honest round robin,
    // but it would swap the very order this check is trying to observe.
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

    // All three of task 1's slices land between task 0's two records, which is
    // what "off the ready queue" looks like from the outside.
    assert_eq!(
        recorded(),
        vec![0, 1, 1, 1, 0],
        "a sleeping task kept running"
    );
    println!("a task slept {SLEEP_TICKS} ticks while the other ran");
}

/// The scheduler's idle path. The stagger in `sleeping` passes through it for
/// a tick in passing; this is the check that a long, quiet idle - boot asleep
/// with nothing else runnable - still wakes on the clock. A wrong `sti; hlt`
/// pair would park the core for good, and it is much better to find that out
/// here than the first time the kernel goes idle for real.
fn idling() {
    reset_record();
    sched::spawn(|| record(0), Priority::NORMAL);
    drain();

    // Nothing left but the boot task, so this puts the scheduler itself to sleep.
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
///
/// The host tests check band ordering against the policy; this checks it against
/// a task that has the CPU and is not giving it up. Task 0 spins at LOW for five
/// ticks and task 1 wakes at HIGH two ticks in. Round-robin alone would resume
/// the spinner, because preemption puts a task at the back of its own band; the
/// only reason task 1 gets the core first is that its band is higher.
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

/// M4: tasks, the context switch, and the clock deciding between them.
fn m4() {
    sched::init();
    println!(
        "scheduler up: {} slots, running task {:?}",
        sched::MAX_TASKS,
        sched::current()
    );

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

    // The only frames allowed to have gone missing are the ones the heap grew
    // into, which it deliberately never gives back. Anything else is a task
    // stack that did not come home.
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

/// Issue a syscall through the gate with the ABI's register convention: number
/// in `rax`, arguments in `rdi, rsi, rdx, r10, r8, r9`, result in `rax`.
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
    let ret: isize;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") nr as isize => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// The syscall gate: the number table and dispatch first, then the interrupt
/// path itself - registers in, `rax` out, and exit ending the task from inside
/// the gate. This is the same path user code takes at M9.
fn m6() {
    // The standard descriptors belong to the gate's world: fd 0, 1 and 2 are
    // three descriptions of the console tty, and the first pipe() must not be
    // handed fd 0. Kept out of m0-m4 because m2's buddy test reads the frame
    // layout, and these allocations shift it.
    vfs::file::init_standard(vfs::tty::init());

    // Dispatch without the interrupt: unassigned numbers, and every fd check
    // the table makes, are observable from here. Kernel callers pass `false`:
    // their pointers are kernel pointers, needing no page-table walk.
    assert_eq!(
        syscall::dispatch(999, [0; syscall::ARG_COUNT], false),
        err::ENOSYS
    );
    // fd 0 is the tty now: a zero-length read is empty, not an error, and the
    // standard fds 0-2 all exist. The first unallocated fd is the error case.
    assert_eq!(
        syscall::dispatch(nr::READ, [0, 0, 0, 0, 0, 0], false),
        0,
        "a zero-length read is empty, not an error"
    );
    assert_eq!(
        syscall::dispatch(nr::READ, [3, 0, 0, 0, 0, 0], false),
        err::EBADF
    );
    assert_eq!(
        syscall::dispatch(nr::WRITE, [7, 0, 0, 0, 0, 0], false),
        err::EBADF
    );

    let msg = b"syscall: write through dispatch\n";
    let wrote = syscall::dispatch(
        nr::WRITE,
        [1, msg.as_ptr() as usize, msg.len(), 0, 0, 0],
        false,
    );
    assert_eq!(wrote as usize, msg.len());

    sched::spawn(
        || {
            let pid = unsafe { syscall(nr::GETPID, 0, 0, 0, 0, 0, 0) };
            assert!(pid >= 2, "getpid returned {pid}");

            let msg = b"syscall: write through the interrupt gate\n";
            let wrote = unsafe { syscall(nr::WRITE, 1, msg.as_ptr() as usize, msg.len(), 0, 0, 0) };
            assert_eq!(wrote as usize, msg.len(), "the gate dropped bytes");

            assert_eq!(unsafe { syscall(nr::YIELD, 0, 0, 0, 0, 0, 0) }, 0);
            assert_eq!(unsafe { syscall(nr::SLEEP, 2, 0, 0, 0, 0, 0) }, 0);
            unsafe { syscall(nr::EXIT, 0, 0, 0, 0, 0, 0) };
        },
        Priority::NORMAL,
    );
    drain();
    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "exit through the gate did not end the task"
    );
    println!("M6_OK");
}

/// M7: the VFS. A ramfs root, a second ramfs mounted inside it, paths walking
/// across the mount, and two tasks passing bytes through the fd table.
fn m7() {
    let root = vfs::ramfs::RamFs::new();
    vfs::mount_root(&*root);

    // A file filled through the fd table and read back through a fresh
    // description: the per-description cursor, exercised on the machine.
    // The initramfs in m8 brings /etc/motd; this is mpos's own file.
    vfs::resolve("/")
        .expect("no root after mount")
        .create("etc", NodeKind::Dir)
        .expect("create /etc");
    vfs::resolve("/etc")
        .unwrap()
        .create("hostname", NodeKind::File)
        .unwrap();
    let fd = vfs::open("/etc/hostname").expect("open /etc/hostname");
    assert_eq!(vfs::write(fd, b"mpos\n").unwrap(), 5);
    vfs::close(fd).unwrap();

    let fd = vfs::open("/etc/hostname").unwrap();
    let mut buf = [0u8; 32];
    let n = vfs::read(fd, &mut buf).unwrap();
    vfs::close(fd).unwrap();
    assert_eq!(&buf[..n], b"mpos\n");

    // A second ramfs over /tmp: resolving through the mountpoint lands in the
    // mounted filesystem, and mounting there again refuses.
    let tmp = vfs::ramfs::RamFs::new();
    vfs::resolve("/")
        .unwrap()
        .create("tmp", NodeKind::Dir)
        .unwrap();
    vfs::mount("/tmp", &*tmp).expect("mount /tmp");
    let inner_root = tmp.root();
    assert!(
        Arc::ptr_eq(&vfs::resolve("/tmp").unwrap(), &inner_root),
        "resolving /tmp did not cross the mount"
    );
    assert_eq!(vfs::mount("/tmp", &*tmp).err(), Some(vfs::VfsError::Busy));
    vfs::resolve("/tmp")
        .unwrap()
        .create("shared", NodeKind::File)
        .unwrap();

    // The writer is a task; the reader waits until the writer has been reaped,
    // which is the earliest point at which every write it made is final.
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

/// Writes go out to the console. Nothing arrives until there is a keyboard
/// driver, so reads are end of input rather than a silent hang.
struct ConsoleDev;

impl vfs::devfs::CharDevice for ConsoleDev {
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn write(&self, buf: &[u8]) -> usize {
        mpos::console::write_bytes(buf);
        buf.len()
    }
}

/// Writes vanish; reads are end of input.
struct NullDev;

impl vfs::devfs::CharDevice for NullDev {
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
}

/// Reads are zeros forever, however much is asked; writes vanish.
struct ZeroDev;

impl vfs::devfs::CharDevice for ZeroDev {
    fn read(&self, buf: &mut [u8]) -> usize {
        buf.fill(0);
        buf.len()
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
}

/// M8: the initramfs becomes the tree, and /dev is a filesystem of its own.
fn m8(info: &BootInfo) {
    const INITRAMFS_MOTD: &[u8] = b"welcome to mpos\n";

    let archive = info.initramfs.expect("the boot config loaded no initramfs");
    let members = vfs::tar::unpack_into(vfs::global(), archive).expect("the initramfs was broken");
    println!("initramfs: {members} members unpacked");

    let motd = vfs::open("/etc/motd").expect("no /etc/motd after the unpack");
    let mut buf = [0u8; 64];
    let n = vfs::read(motd, &mut buf).unwrap();
    vfs::close(motd).unwrap();
    assert_eq!(&buf[..n], INITRAMFS_MOTD);
    println!(
        "motd: {}",
        core::str::from_utf8(&buf[..n]).expect("motd is utf8")
    );

    // Devices mount over the /dev directory the archive provided.
    let dev = vfs::devfs::DevFs::new();
    dev.register("console", Arc::new(ConsoleDev));
    dev.register("null", Arc::new(NullDev));
    dev.register("zero", Arc::new(ZeroDev));
    vfs::mount("/dev", &*dev).expect("mount /dev");

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

/// The exit code the user program ends with. Both sides agree on it the way
/// they agree on the syscall numbers - it lives in `user/bin/hello.rs` too.
const HELLO_EXIT: isize = 42;

/// M9: processes. The user program comes through the VFS the way an init would
/// read it, the loader maps it into a fresh address space, and the task
/// enters ring 3 - from where every kernel touch, including its own exit, is
/// a syscall through the gate.
fn m9() {
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
    drain();
    assert_eq!(
        sched::exit_code(id),
        Some(HELLO_EXIT),
        "hello did not end with the exit code it chose"
    );
    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "the user task was not reaped"
    );
    println!("M9_OK");
}

/// M10: a disk and a filesystem on it. virtio-blk gives us a block device,
/// ext2 turns it into a tree, and the proof that writes really reached the
/// disk is a second, cold mount over the same device reading them back.
fn m10() {
    let dev = blk::virtio::VirtioBlk::init().expect("no virtio-blk device answered on the PCI bus");
    let sectors = dev.capacity_sectors();
    println!(
        "virtio-blk: {} sectors ({} MiB)",
        sectors,
        sectors * 512 / MIB as u64
    );
    assert_eq!(
        sectors,
        8 * MIB as u64 / 512,
        "the disk is not the 8 MiB image the build made"
    );

    // /mnt is the door into the disk filesystem; the initramfs does not carry it.
    if vfs::resolve("/mnt").is_err() {
        vfs::resolve("/")
            .expect("no root filesystem")
            .create("mnt", NodeKind::Dir)
            .expect("create /mnt");
    }
    let fs = vfs::ext2::Ext2Fs::mount(dev.clone()).expect("the ext2 image did not mount");
    vfs::mount("/mnt", &fs).expect("mount /mnt");

    // Seed files laid down by mke2fs on the host: reading them proves the
    // parser against something we did not write ourselves.
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

    // The write: created through the mount crossing, grown past the twelve
    // direct blocks into an indirect one.
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

    // A second mount over the same device starts with an empty cache, so the
    // only thing that can come back is what truly reached the disk.
    let fs2 = vfs::ext2::Ext2Fs::mount(dev).expect("remount the ext2 image");
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

    // And the host's files are unharmed by everything above them.
    let seed = fs2.root().lookup("seed.txt").expect("seed.txt survived");
    let mut buf = [0u8; 128];
    let n = seed.read_at(0, &mut buf).expect("read seed.txt back");
    assert_eq!(&buf[..n], SEED);

    println!("ext2: {LEN} bytes written through /mnt, verified on a cold remount");
    println!("M10_OK");
}

/// M11: pipes, the tty line discipline, and signals.
///
/// Every blocking path here goes through `block_until`, so each scenario is
/// also a lost-wakeup test: a writer that sleeps before writing means the
/// reader was genuinely asleep when the data it waits for appeared.
fn m11() {
    use mpos::sched::signal::{SIGKILL, SIGTERM};

    // --- a pipe carries bytes between tasks, then EOF when the writer leaves.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(nr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
        0,
        "pipe() failed"
    );
    let (rfd, wfd) = (fds[0], fds[1]);
    assert_ne!(rfd, wfd, "pipe() handed out the same fd twice");

    sched::spawn(
        move || {
            // Sleep first, so the reader below is verifiably blocked when the
            // data arrives; only a wake can get it moving again.
            sched::sleep_ticks(2);
            let msg = b"ping through a pipe\n";
            let wrote = unsafe {
                syscall(
                    nr::WRITE,
                    wfd as usize,
                    msg.as_ptr() as usize,
                    msg.len(),
                    0,
                    0,
                    0,
                )
            };
            assert_eq!(wrote as usize, msg.len(), "the pipe dropped bytes");
            // The last writer fd closes here: readers see EOF after this.
            vfs::close(wfd).unwrap();
        },
        Priority::NORMAL,
    );

    let mut buf = [0u8; 64];
    let got = unsafe {
        syscall(
            nr::READ,
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

    // --- writing with no reader is EPIPE, an error and not a death.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(nr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
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
    let rc = unsafe { syscall(nr::WRITE, wfd as usize, b"x".as_ptr() as usize, 1, 0, 0, 0) };
    assert_eq!(rc, err::EPIPE, "a write to a readerless pipe must be EPIPE");
    vfs::close(wfd).unwrap();
    println!("pipe: write with no reader answered EPIPE and nobody died");

    // --- the tty: editing, echo, canonical reads.
    let tty = vfs::tty::global();
    // The console shows `hellx`, the erase, then `o`: the line reads `hello`
    // because the DEL took the `x` back off.
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

    // fd 0 through the gate, blocking: input arrives two ticks after the read
    // starts, so only a wake can finish this syscall.
    sched::spawn(
        || {
            sched::sleep_ticks(2);
            for &b in b"slow\n" {
                vfs::tty::global().receive_byte(b);
            }
        },
        Priority::NORMAL,
    );
    let n = unsafe { syscall(nr::READ, 0, line.as_mut_ptr() as usize, line.len(), 0, 0, 0) };
    assert_eq!(n as usize, b"slow\n".len());
    assert_eq!(&line[..n as usize], b"slow\n");
    drain();
    println!("tty: line editing, echo, Ctrl-D, and a blocking fd 0 read");

    // --- signals: fatal, delivered at the next blocking point.
    // One paused; the wake itself is what ends it.
    let paused = sched::spawn(|| sched::pause(), Priority::NORMAL);
    sched::sleep_ticks(2);
    assert!(sched::send_signal(paused, SIGTERM));
    drain();
    assert_eq!(
        sched::exit_code(paused),
        Some(-(SIGTERM as isize)),
        "SIGTERM did not end the paused task"
    );

    // One blocked on a pipe read that will never be satisfied.
    let mut fds = [0i32; 2];
    assert_eq!(
        unsafe { syscall(nr::PIPE, fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0) },
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
    drain();
    assert_eq!(
        sched::exit_code(blocked),
        Some(-(SIGKILL as isize)),
        "SIGKILL did not end the blocked reader"
    );
    // The victim's read fd stays in the global table; close the other end so
    // the pipe's counts stay consistent with reality.
    vfs::close(wfd).unwrap();

    // One that signals itself before blocking: delivery from the loop top.
    let selfsent = sched::spawn(
        || {
            sched::send_signal(sched::current(), SIGTERM);
            sched::pause();
        },
        Priority::NORMAL,
    );
    drain();
    assert_eq!(sched::exit_code(selfsent), Some(-(SIGTERM as isize)));

    // kill(pid, 0) is the existence probe; unknown pids answer ESRCH.
    assert_eq!(
        unsafe { syscall(nr::KILL, sched::current().0, 0, 0, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { syscall(nr::KILL, 9999, SIGTERM as usize, 0, 0, 0, 0) },
        err::ESRCH
    );
    assert_eq!(unsafe { syscall(nr::KILL, 0, 32, 0, 0, 0, 0) }, err::EINVAL);
    println!("signals: SIGTERM, SIGKILL, and the kill(0) probe behave");

    println!("M11_OK");
}

/// The script the gate types at the shell, one byte at a time, as if typed.
/// The last line's output is the milestone marker; the Ctrl-D after it ends
/// the shell the way end of input ends any shell.
const SHELL_SCRIPT: &[&[u8]] = &[
    b"echo hello from the shell\n",
    b"cat /etc/motd\n",
    b"ls /\n",
    b"ls /bin\n",
    b"true\n",
    b"false\n",
    b"echo M12_OK\n",
    b"\x04",
];

/// M12: the thin libc, the shell, and the coreutils, all in ring 3.
///
/// The boot task spawns /bin/sh the way `spawn()` does - path, argv, parent
/// link - while a kernel task types a script into the tty. Every command the
/// script runs is a separate user process created through the gate: echo and
/// cat and ls are programs, not kernel routines. When the script's Ctrl-D
/// arrives the shell reads end of input and exits, and its exit code - read
/// through wait's own path - must be 0.
fn m12() {
    let shell = vfs::read_file("/bin/sh").expect("the initramfs carries /bin/sh");
    let argv = alloc::vec![String::from("sh")];
    let sh = sched::try_spawn_user(&shell, Priority::NORMAL, sched::current(), &argv)
        .expect("the shell image loaded");

    sched::spawn(
        || {
            // Give the shell a tick to reach its first read; the line
            // discipline would queue the bytes anyway, but this way the log
            // shows the prompt before the typing, as it would look.
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

    // The wait the shell itself performs for its children, exercised here on
    // the shell: block until it exits, then read the code it left.
    sched::block_until(|| sched::exit_code(sh).is_some());
    let code = sched::exit_code(sh).expect("the shell exited");
    assert_eq!(code, 0, "the shell did not end cleanly: {code}");
    drain();
    assert_eq!(
        sched::task_count(),
        sched::permanent_tasks(),
        "a process the shell started was never reaped"
    );
    println!("shell: ran the script, read its end of input, exited {code}");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nKERNEL PANIC: {}", info);
    qemu::exit(qemu::EXIT_FAILURE)
}
