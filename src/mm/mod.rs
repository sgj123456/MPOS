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
/// Where a device's registers get mapped.
///
/// The bootloader's direct map covers physical memory and nothing else, and a
/// PCI BAR is not memory: it is a range of addresses that answers to a device.
/// Devices therefore get a window of their own, above the user half and below
/// the kernel's own image, handed out a mapping at a time and never reused -
/// a device's registers are reachable for as long as the machine is up, so
/// there is nothing to recycle.
#[cfg(target_os = "none")]
pub const MMIO_BASE: usize = 0xffff_ffff_4000_0000;
/// One past the end of the window: 1 GiB of address space, which is room for
/// every BAR a machine could plausibly have.
#[cfg(target_os = "none")]
const MMIO_LIMIT: usize = 0xffff_ffff_8000_0000;

/// The next free byte in that window.
#[cfg(target_os = "none")]
static MMIO_NEXT: AtomicUsize = AtomicUsize::new(MMIO_BASE);

/// Map a device's registers into the kernel half and return the address they
/// can be reached through.
///
/// Called once per BAR, at the moment a driver decides it wants the device.
#[cfg(target_os = "none")]
pub fn map_mmio(phys: usize, len: usize) -> usize {
    let len = pframe::align_up(len, PAGE_SIZE);
    let virt = MMIO_NEXT.fetch_add(len, Ordering::Relaxed);
    assert!(
        virt + len <= MMIO_LIMIT,
        "out of room for device registers: {len} bytes at {phys:#x}"
    );
    crate::arch::paging::map_mmio(virt, phys, len);
    virt
}

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
    let hhdm = HHDM.load(Ordering::Relaxed);
    // Named rather than left to the subtraction: the only way to get here with
    // a smaller address is a caller handing back something that never came
    // from `alloc_pages` - a physical frame, or a user address - and which one
    // it is says where the mistake is.
    assert!(
        virt >= hhdm,
        "virt_to_phys({virt:#x}): not an address from the direct map (hhdm {hhdm:#x})"
    );
    virt - hhdm
}

/// Allocate `2^order` contiguous physical frames, aligned to their own size.
/// A single-page allocation comes back with a reference count of one, already
/// recorded by the zone that handed it out.
#[cfg(target_os = "none")]
pub fn alloc_frames(order: usize) -> Option<usize> {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().alloc(order))
}

/// Return frames allocated by [`alloc_frames`] with the same order.
/// For order-0 (single page) allocations, the reference count is decremented
/// first; the frame is only returned to the buddy allocator when no owners
/// remain.
#[cfg(target_os = "none")]
pub fn free_frames(phys: usize, order: usize) {
    // A pinned range is never handed back, whatever path asks. The pages in
    // one belong to a device the kernel cannot reclaim: the GPU's
    // framebuffer, whose DMA pages userland maps directly, so that a
    // `munmap`, a fork's copy-on-write, or a process exit freeing "its" page
    // would recycle memory the device is still drawing the screen out of.
    // The frame's reference count is left alone on this path - the count
    // only arbitrates between *owners*, and a pinned page has exactly one
    // that never lets go.
    if is_pinned(phys) {
        return;
    }
    if order == 0 {
        let should_free =
            critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free_frame_ref(phys));
        if !should_free {
            return;
        }
    }
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().free(phys, order))
}

/// Physical ranges that must never reach the free lists, because a device
/// owns them for the machine's life - set by [`pin_frames`], consulted by
/// [`free_frames`]. One frame buffer is the whole registry today; a `Vec`
/// because a second device with a user-mapped DMA region would be a second
/// entry, not a redesign.
#[cfg(target_os = "none")]
static PINNED: critical_section::Mutex<core::cell::RefCell<alloc::vec::Vec<(usize, usize)>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(alloc::vec::Vec::new()));

/// Mark `phys..phys + len` as device-owned for good: the allocator will
/// refuse the range no matter who asks to free it or through which path.
#[cfg(target_os = "none")]
pub fn pin_frames(phys: usize, len: usize) {
    critical_section::with(|cs| {
        PINNED.borrow(cs).borrow_mut().push((
            phys,
            phys.checked_add(len).expect("a pinned range does not wrap"),
        ));
    });
}

/// Whether `phys` lies inside a pinned range. The check is a small walk of a
/// tiny list - it only runs on the free path of an order-0 page, and the
/// answer it protects is a screen's worth of DMA memory.
#[cfg(target_os = "none")]
fn is_pinned(phys: usize) -> bool {
    critical_section::with(|cs| {
        PINNED
            .borrow(cs)
            .borrow()
            .iter()
            .any(|&(start, end)| phys >= start && phys < end)
    })
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
