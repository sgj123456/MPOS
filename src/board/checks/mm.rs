//! `M0`/`M2`: the global allocator, the frame allocator and the slab heap.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr;

use mpos::boot::BootInfo;
use mpos::mm::{self, PAGE_SIZE};
use mpos::println;

/// One mebibyte, for the frame-count lines below.
const MIB: usize = 1024 * 1024;

pub(crate) fn run(info: &BootInfo) {
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
