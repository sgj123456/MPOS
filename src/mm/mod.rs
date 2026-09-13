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
/// Single-page allocations start with a reference count of 1.
#[cfg(target_os = "none")]
pub fn alloc_frames(order: usize) -> Option<usize> {
    let phys = critical_section::with(|cs| PMM.borrow(cs).borrow_mut().alloc(order))?;
    if order == 0 {
        critical_section::with(|cs| PMM.borrow(cs).borrow_mut().set_refcount_1(phys));
    }
    Some(phys)
}

/// Return frames allocated by [`alloc_frames`] with the same order.
/// For order-0 (single page) allocations, the reference count is decremented
/// first; the frame is only returned to the buddy allocator when no owners
/// remain.
#[cfg(target_os = "none")]
pub fn free_frames(phys: usize, order: usize) {
    if order == 0 {
        let should_free =
            critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free_frame_ref(phys));
        if !should_free {
            return;
        }
    }
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free(phys, order))
}

/// The reference count of a single page. Returns 0 for untracked pages.
#[cfg(target_os = "none")]
pub fn frame_refcount(phys: usize) -> u32 {
    critical_section::with(|cs| PMM.borrow(cs).borrow().refcount(phys))
}

/// Increment the reference count of a single page (fork shares a frame).
#[cfg(target_os = "none")]
pub fn clone_frame_ref(phys: usize) {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().clone_frame_ref(phys))
}

/// Decrement the reference count. Returns `true` when the frame should be
/// freed back to the buddy allocator (refcount hit 0).
#[cfg(target_os = "none")]
pub fn free_frame_ref(phys: usize) -> bool {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free_frame_ref(phys))
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
