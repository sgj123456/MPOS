//! Kernel memory management: physical frames first, then everything built on them.
//!
//! The ordering matters. The kernel heap, page tables and task stacks are all
//! clients of the frame allocator, so `init` brings that up and nothing else: the
//! heap needs no setup of its own, it takes frames on demand.

pub mod elf;
/// The number a System V IPC object is known by: slot plus generation, shared
/// by the three families because `ipcs` and a stale id both read it.
pub mod ipc_id;
pub mod pframe;
/// System V shared memory: the segment table and the frames behind it.
pub mod sysv_shm;
pub mod vm;
pub mod vma;

#[cfg(target_os = "none")]
pub mod heap;

/// This module's suite for a test build: the allocators' own checks, written as a
/// child so they read this module's state rather than through doors added for
/// them (`docs/NOTES.md` §15.20). One `pub const SUITE`, listed by the wiring
/// layer's harness (`src/board/kunit.rs`).
#[cfg(all(target_os = "none", feature = "selftest"))]
pub mod kunit;

pub use pframe::{Buddy, MAX_ORDER, PAGE_SHIFT, PAGE_SIZE, Pmm};
pub use vm::{KernelSections, MapFlags, Mapping};
pub use vma::{ImageBounds, VmBook, Vma, VmaKind, VmaTable, vm_book};

#[cfg(target_os = "none")]
use crate::boot::BootInfo;
#[cfg(target_os = "none")]
use core::cell::RefCell;
#[cfg(target_os = "none")]
use critical_section::Mutex;

/// The physical memory manager. A `static` because it must exist before the heap
/// does, and because on a single core `critical_section` is all the exclusion it
/// needs - no lock that could be held across a preemption.
#[cfg(target_os = "none")]
pub static PMM: Mutex<RefCell<Pmm>> = Mutex::new(RefCell::new(Pmm::empty()));

// The direct map the bootloader built lives in `hal` (`crate::hal::phys_to_virt`
// and friends), because the machine layer's own page-table code needs it and
// `arch` cannot name `mm` after it becomes a crate of its own. This module keeps
// no second copy: `init` hands the record's offset to `hal`, and the three
// helpers below are forwards.

#[cfg(target_os = "none")]
#[global_allocator]
static ALLOCATOR: heap::KernelHeap = heap::KernelHeap::empty();

// The per-BAR MMIO window allocator used to live here. It is the *machine's*
// answer to `hal::Paging::map_mmio_window` - "a PCIe BAR has no fixed address,
// so give it a fresh kernel-half mapping and remember where the next one goes"
// - and it moved to `arch/x86_64/mmio.rs` with the rest of that machine's page
// table code. What is left at this seam is one name, `hal::map_mmio_window`,
// which a driver calls and does not have to know which of the two mechanisms
// answers it.

#[cfg(target_os = "none")]
pub fn phys_to_virt(phys: usize) -> usize {
    crate::hal::phys_to_virt(phys)
}

/// The offset [`phys_to_virt`] adds, for callers that need it directly rather
/// than the address it produces.
#[cfg(target_os = "none")]
pub fn hhdm_offset() -> usize {
    crate::hal::direct_map_offset()
}

#[cfg(target_os = "none")]
pub fn virt_to_phys(virt: usize) -> usize {
    crate::hal::virt_to_phys(virt)
}

/// Allocate `2^order` contiguous physical frames, aligned to their own size.
/// A single-page allocation comes back with a reference count of one, already
/// recorded by the zone that handed it out.
#[cfg(target_os = "none")]
pub fn alloc_frames(order: usize) -> Option<usize> {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().alloc(order))
}

/// Allocate the single frame a process's demand paging needs, or say the machine
/// has none to give it.
///
/// `None` here is a fact the fault path answers with the OOM kill rather than
/// `SIGSEGV` (`docs/notes/` §8.0), so the frames it refuses to hand out are the
/// kernel's own reserve: [`pframe::USER_FRAME_RESERVE`], spent while the process
/// that used the machine up is being ended.
#[cfg(target_os = "none")]
pub fn alloc_user_frame() -> Option<usize> {
    critical_section::with(|cs| PMM.borrow(cs).borrow_mut().alloc_for_user())
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

/// Release a range [`pin_frames`] marked: the frames are the allocator's
/// again, exactly as they were before the pin.
///
/// The one caller is a display resize, which rebuilds the GPU's scanout at the
/// size the host now wants and returns the block the old one lived in
/// (`drivers::video::render`). It exists because the *address of the screen*
/// changes: pinning a framebuffer for the machine's life is right while there
/// is exactly one framebuffer, and wrong once the host can ask for another
/// one - a window drag sends a resize per event, and a pin per resize would
/// strand a screenful of frames each time.
///
/// Only the exact range is dropped, so a caller cannot unpin something it did
/// not pin; nothing else guards the call because there is one pin site and one
/// unpin site, and the geometry both use travels with the framebuffer.
#[cfg(target_os = "none")]
pub fn unpin_frames(phys: usize, len: usize) {
    let end = phys.checked_add(len).expect("a pinned range does not wrap");
    critical_section::with(|cs| {
        PINNED
            .borrow(cs)
            .borrow_mut()
            .retain(|&(start, stop)| !(start == phys && stop == end));
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
    crate::hal::set_direct_map_offset(info.hhdm_offset);
    critical_section::with(|cs| unsafe { PMM.borrow(cs).borrow_mut().init(info) });

    let stats = frame_stats();
    assert!(
        stats.total > 0,
        "the boot memory map contains no usable memory"
    );
    stats
}

/// The line the machine prints when the kernel heap cannot serve a request,
/// before the caller's own panic ends it. **Not allocating itself**, on purpose:
/// this runs when the allocator has nothing, so a `format!` here would ask the
/// failed allocator again. Pure text, so it lives on the host side and is tested
/// there (`docs/notes/` §8.0.2 says why the answer is a line and not a kill).
pub fn exhaustion_line(size: usize, into: &mut [u8]) -> usize {
    const PREFIX: &[u8] = b"mpos: heap exhausted: ";
    const SUFFIX: &[u8] = b" bytes (the machine stops here; docs/notes/ 8.0.2)\n";
    let mut n = 0;
    for &b in PREFIX {
        if n < into.len() {
            into[n] = b;
            n += 1;
        }
    }
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut value = size;
    if value == 0 {
        digits[0] = b'0';
        count = 1;
    }
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
    }
    for i in (0..count).rev() {
        if n < into.len() {
            into[n] = digits[i];
            n += 1;
        }
    }
    for &b in SUFFIX {
        if n < into.len() {
            into[n] = b;
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod exhaustion_tests {
    use super::exhaustion_line;

    /// The death line names the request and never allocates: a heap that has
    /// nothing left may not grow a string to say so.
    #[test]
    fn the_heaps_last_line_names_the_request() {
        let mut buf = [0u8; 96];
        let n = exhaustion_line(4096, &mut buf);
        assert_eq!(
            &buf[..n],
            b"mpos: heap exhausted: 4096 bytes (the machine stops here; docs/notes/ 8.0.2)\n"
        );
        for (size, digits) in [
            (0usize, "0"),
            (1, "1"),
            (999, "999"),
            (123_456_789, "123456789"),
        ] {
            let n = exhaustion_line(size, &mut buf);
            let mut want = [0u8; 96];
            let pfx = b"mpos: heap exhausted: ";
            let sfx = b" bytes (the machine stops here; docs/notes/ 8.0.2)\n";
            let mut k = 0;
            for &b in pfx {
                want[k] = b;
                k += 1;
            }
            for b in digits.as_bytes() {
                want[k] = *b;
                k += 1;
            }
            for &b in sfx {
                want[k] = b;
                k += 1;
            }
            assert_eq!(&buf[..n], &want[..k], "size {size}");
        }
    }
}
