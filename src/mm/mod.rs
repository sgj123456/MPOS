//! Kernel memory management: physical frames first, then everything built on them.
//!
//! The ordering matters. The kernel heap, page tables and task stacks are all
//! clients of the frame allocator, so `init` brings that up and nothing else: the
//! heap needs no setup of its own, it takes frames on demand.

pub mod elf;
pub mod pframe;
pub mod vm;

#[cfg(target_os = "none")]
pub mod heap;

pub use pframe::{Buddy, MAX_ORDER, PAGE_SHIFT, PAGE_SIZE, Pmm};
pub use vm::{KernelSections, MapFlags, Mapping};

#[cfg(target_os = "none")]
use crate::boot::BootInfo;
#[cfg(target_os = "none")]
use core::cell::RefCell;
#[cfg(target_os = "none")]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(target_os = "none")]
use critical_section::Mutex;

/// The physical memory manager. A `static` because it must exist before the heap
/// does, and because on a single core `critical_section` is all the exclusion it
/// needs - no lock that could be held across a preemption.
#[cfg(target_os = "none")]
pub static PMM: Mutex<RefCell<Pmm>> = Mutex::new(RefCell::new(Pmm::empty()));

/// Offset the bootloader told us to add to a physical address to reach it.
#[cfg(target_os = "none")]
static HHDM: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "none")]
#[global_allocator]
static ALLOCATOR: heap::KernelHeap = heap::KernelHeap::empty();

#[cfg(target_os = "none")]
pub fn phys_to_virt(phys: usize) -> usize {
    phys + HHDM.load(Ordering::Relaxed)
}

/// The offset [`phys_to_virt`] adds, for callers that need it directly rather
/// than the address it produces.
#[cfg(target_os = "none")]
pub fn hhdm_offset() -> usize {
    HHDM.load(Ordering::Relaxed)
}

#[cfg(target_os = "none")]
pub fn virt_to_phys(virt: usize) -> usize {
    virt - HHDM.load(Ordering::Relaxed)
}

/// Allocate `2^order` contiguous physical frames, aligned to their own size.
#[cfg(target_os = "none")]
pub fn alloc_frames(order: usize) -> Option<usize> {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().alloc(order))
}

/// Return frames allocated by [`alloc_frames`] with the same order.
#[cfg(target_os = "none")]
pub fn free_frames(phys: usize, order: usize) {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free(phys, order))
}

/// [`alloc_frames`], returned at the address the kernel can actually use.
#[cfg(target_os = "none")]
pub fn alloc_pages(order: usize) -> Option<usize> {
    alloc_frames(order).map(phys_to_virt)
}

/// Return a block allocated by [`alloc_pages`].
#[cfg(target_os = "none")]
pub fn free_pages(virt: usize, order: usize) {
    free_frames(virt_to_phys(virt), order)
}

#[cfg(target_os = "none")]
pub fn frame_stats() -> FrameStats {
    critical_section::with(|cs| {
        let pmm = PMM.borrow(cs).borrow();
        FrameStats {
            zones: pmm.zone_count(),
            free: pmm.free_frames(),
            total: pmm.total_frames(),
        }
    })
}

#[cfg(target_os = "none")]
#[derive(Clone, Copy)]
pub struct FrameStats {
    pub zones: usize,
    pub free: usize,
    pub total: usize,
}

#[cfg(target_os = "none")]
pub fn heap_stats() -> heap::HeapStats {
    ALLOCATOR.stats()
}

/// Bring up physical memory. The heap needs no setup: it takes pages from here
/// on demand, so it is usable the moment this returns.
///
/// # Safety
///
/// `info` must describe live memory mapped at `info.hhdm_offset`.
#[cfg(target_os = "none")]
pub unsafe fn init(info: &BootInfo) -> FrameStats {
    HHDM.store(info.hhdm_offset, Ordering::Relaxed);
    critical_section::with(|cs| unsafe { PMM.borrow(cs).borrow_mut().init(info) });

    let stats = frame_stats();
    assert!(
        stats.total > 0,
        "the boot memory map contains no usable memory"
    );
    stats
}
