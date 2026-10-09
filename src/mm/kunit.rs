//! The `mm` suite: the global allocator, the frame allocator and the slab heap.
//!
//! A child of `mm` rather than a check in the wiring layer, which is what makes it
//! a *test* rather than a client: it can read the module's private state, so
//! nothing here needs an accessor added to `mm` for its sake (`docs/NOTES.md`
//! §15.20). It runs at [`Phase::Heap`] - the prerequisite, which is the fact the
//! machine knows - and it is compiled only into a test build.
//!
//! Every deliberate check answers a value through `case_check!` instead of
//! asserting: `panic = "abort"` means an `assert!` here would end the machine and
//! hide the cases after it. What is *not* converted is the incidental panic - an
//! `Option::expect` on a frame the allocator just said it had, an index that
//! cannot be out of range. Those are not checks, and a machine that hits one is
//! broken in a way no later case would have told us about.

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr;

use crate::boot::BootInfo;
use crate::case_check;
use crate::kunit::{Case, Phase, Suite};
use crate::println;

use super::{MAX_ORDER, PAGE_SIZE, alloc_frames, frame_stats, free_frames, heap_stats};

/// The suite this module contributes to the boot's self-tests.
pub const SUITE: Suite = Suite {
    name: "mm",
    phase: Phase::Heap,
    cases: &[
        Case {
            name: "frames",
            run: frames,
        },
        Case {
            name: "large_block",
            run: large_block,
        },
        Case {
            name: "heap",
            run: heap,
        },
    ],
};

/// One mebibyte, for the frame-count lines below.
const MIB: usize = 1024 * 1024;

/// The buddy allocator: zones exist, a frame survives a write through the direct
/// map, and a freed order-1 block is handed back as the same block.
fn frames(info: &BootInfo) -> Result<(), String> {
    let stats = frame_stats();
    println!(
        "pmm: {} of {} frames free ({} MiB)",
        stats.free,
        stats.total,
        stats.free * PAGE_SIZE / MIB
    );
    case_check!(stats.zones > 0, "no memory zones were built");

    // HHDM: allocate a frame, write through the direct map, read back.
    let Some(frame) = alloc_frames(0) else {
        return Err("out of order-0 frames".into());
    };
    let virt = info.phys_to_virt(frame);
    unsafe {
        ptr::write_bytes(virt as *mut u8, 0xa5, PAGE_SIZE);
        case_check!(
            *(virt as *const u8) == 0xa5,
            "the first byte of a frame did not come back through the hhdm"
        );
        case_check!(
            *((virt + PAGE_SIZE - 1) as *const u8) == 0xa5,
            "the last byte of a frame did not come back through the hhdm"
        );
    }
    free_frames(frame, 0);
    println!("frame written and read back through the hhdm");

    // Buddy merge: allocate order-1, free it, re-allocate and verify we get the
    // same block back. This exercises the merge path, because the allocator must
    // coalesce order-0 buddies to satisfy order-1 at all.
    let Some(order1) = alloc_frames(1) else {
        return Err("no order-1 block for the merge test".into());
    };
    free_frames(order1, 1);
    case_check!(
        alloc_frames(1) == Some(order1),
        "buddy merge failed: the freed order-1 block was not re-allocated"
    );
    free_frames(order1, 1);
    case_check!(
        frame_stats().free == stats.free,
        "frames leaked: {} free before, {} after",
        stats.free,
        frame_stats().free
    );
    Ok(())
}

/// The largest block a zone hands out, written end to end and given back.
fn large_block(info: &BootInfo) -> Result<(), String> {
    let stats = frame_stats();
    let Some(big) = alloc_frames(MAX_ORDER) else {
        return Err(alloc::format!("out of order-{MAX_ORDER} frames"));
    };
    let virt = info.phys_to_virt(big);
    let bytes = PAGE_SIZE << MAX_ORDER;
    unsafe {
        ptr::write_bytes(virt as *mut u8, 0x5a, bytes);
        case_check!(
            *((virt + bytes - 1) as *const u8) == 0x5a,
            "the last byte of a {} MiB block did not come back",
            bytes / MIB
        );
    }
    println!("{} MiB block written end to end", bytes / MIB);
    free_frames(big, MAX_ORDER);
    case_check!(
        frame_stats().free == stats.free,
        "frames leaked: {} free before, {} after",
        stats.free,
        frame_stats().free
    );
    Ok(())
}

/// Every slab size class and the large path, an allocation that grows through
/// realloc, and then the two allocators agreeing about what they have handed out.
///
/// The agreement check is why this is one case rather than two: it compares the
/// frames consumed *before* the allocations below against the heap's mapped bytes
/// *after* them, so both baselines have to be taken around the same body. Split
/// apart, the check would compare a baseline against itself and pass whatever the
/// kernel did.
fn heap(_info: &BootInfo) -> Result<(), String> {
    let frames_before = frame_stats();
    let baseline = heap_stats();
    let mut live: Vec<(*mut u8, Layout)> = Vec::new();

    for class in [16, 32, 64, 128, 256, 512, 1024, 2048] {
        for align in [1, 8, 16, class] {
            let layout =
                Layout::from_size_align(class / 2 + 1, align).expect("layout is representable");
            take(&mut live, layout)?;
        }
    }

    for layout in [
        Layout::from_size_align(4096, 8).unwrap(),
        Layout::from_size_align(3000, 8192).unwrap(),
        Layout::from_size_align(1 << 20, 4096).unwrap(),
    ] {
        take(&mut live, layout)?;
    }

    let mut grown: Vec<u8> = Vec::new();
    for i in 0..100 * 1024 {
        grown.push((i % 251) as u8);
    }
    case_check!(grown.len() == 100 * 1024);
    case_check!(
        grown.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8),
        "a vector's contents did not survive growing through realloc"
    );
    drop(grown);

    let held = live.len();
    for (ptr, layout) in live {
        unsafe { alloc::alloc::dealloc(ptr, layout) };
    }
    println!("{held} allocations across every class and the large path");

    let after = heap_stats();
    case_check!(
        after.live == baseline.live,
        "dealloc did not return every byte to the heap: {} B live, {} B at the start",
        after.live,
        baseline.live
    );
    case_check!(
        after.cached >= baseline.cached,
        "freed slots did not go back to their class: {} B cached, {} B at the start",
        after.cached,
        baseline.cached
    );

    // Cross-check: the heap and the frame allocator agree about the memory handed
    // out. Frames consumed by non-heap code - page tables, the block layer's DMA
    // buffers - are subtracted, so only heap-owned frames are compared.
    let non_heap = (frames_before.total - frames_before.free) * PAGE_SIZE - baseline.mapped;
    let frames = frame_stats();
    let heap = heap_stats();
    case_check!(
        (frames.total - frames.free) * PAGE_SIZE - non_heap == heap.mapped,
        "heap and frame allocator disagree about memory handed out: \
         {} B of frames since the start, {} B mapped by the heap",
        (frames.total - frames.free) * PAGE_SIZE - non_heap,
        heap.mapped
    );
    println!(
        "heap holds {} KiB in {} free frames of {}",
        heap.mapped / 1024,
        frames.free,
        frames.total
    );

    Ok(())
}

/// Allocate `layout`, check the contract held, and fill the memory so that a
/// later hand-out of the same bytes to someone else is caught.
fn take(live: &mut Vec<(*mut u8, Layout)>, layout: Layout) -> Result<(), String> {
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    case_check!(!ptr.is_null(), "allocation of {layout:?} failed");
    case_check!(
        (ptr as usize).is_multiple_of(layout.align()),
        "{layout:?} came back misaligned: {ptr:p}"
    );
    for &(other, other_layout) in live.iter() {
        case_check!(
            !overlaps(ptr, layout, other, other_layout),
            "{layout:?} at {ptr:p} overlaps a live allocation at {other:p}"
        );
    }

    let fill = (live.len() & 0xff) as u8;
    unsafe { ptr::write_bytes(ptr, fill, layout.size()) };
    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    case_check!(
        bytes.iter().all(|&b| b == fill),
        "{layout:?} at {ptr:p} did not read back what was written"
    );

    live.push((ptr, layout));
    Ok(())
}

fn overlaps(a: *mut u8, a_layout: Layout, b: *mut u8, b_layout: Layout) -> bool {
    let (a_start, b_start) = (a as usize, b as usize);
    a_start < b_start + b_layout.size() && b_start < a_start + a_layout.size()
}
