//! Where a PCIe BAR's registers go, on a machine whose devices have no fixed
//! address for them.
//!
//! The aarch64 half of the pair `arch/x86_64/mmio.rs` describes, and it exists
//! for the same reason that one does: `hal::map_mmio_window` is the name a
//! driver calls when it has an address the firmware handed it and nowhere to
//! reach it, and neither machine's driver learns which mechanism answered. What
//! differs is only where the virtual addresses come from.
//!
//! **This machine already had a device window, and this is not it.** The 64 MiB
//! the tree describes - PL011, the GIC, the virtio-MMIO frames - is mapped whole
//! at boot and reached by offset through `paging::device_virt`; a device in it
//! has one address, known before the first instruction. A PCIe BAR is the other
//! kind: the address is the firmware's to hand out, and the kernel learns it
//! only when a driver enumerates the bus. So the bump-allocated window starts
//! *above* the fixed one rather than on top of it, and the two never overlap -
//! which is what lets `paging::map_mmio` keep its "already mapped, do nothing"
//! answer for the first and grow a real mapping for the second.
//!
//! Room, because the ECAM walk is the caller that asks for the most: one mebibyte
//! per bus, mapped on first access and kept, so a machine with a handful of
//! populated buses spends a handful of mebibytes of address space. 960 MiB is
//! left between the fixed window and the image, and the allocator refuses rather
//! than wraps when it is gone.

use core::sync::atomic::{AtomicUsize, Ordering};

/// The first byte of the window: one past the fixed device window, which ends at
/// `paging::DEVICE_VIRT + paging::DEVICE_WINDOW`.
///
/// Written as arithmetic on those two constants rather than as a second literal
/// `0xffff_ffff_4400_0000`, because the thing that must stay true is that the
/// two windows do not overlap, and a constant beside a constant says that once
/// while two literals leave it to the reader.
const MMIO_BASE: usize =
    crate::arch::aarch64::paging::DEVICE_VIRT + crate::arch::aarch64::paging::DEVICE_WINDOW;

/// One past the end of the window: where the kernel image is linked, which is
/// the ceiling no mapping may cross.
const MMIO_LIMIT: usize = 0xffff_ffff_8000_0000;

/// The next free byte in that window.
static MMIO_NEXT: AtomicUsize = AtomicUsize::new(MMIO_BASE);

/// Map a device's registers into the kernel half and return the address they can
/// be reached through, or `None` when the window is full.
///
/// Called once per BAR - and once per bus of ECAM - at the moment a driver
/// decides it wants the device. Every mapping goes on the same bump pointer and
/// is never handed back: a driver keeps its registers for the life of the
/// machine, so there is nothing to free and no allocator state to make it worth
/// one.
pub fn window(phys: usize, len: usize) -> Option<usize> {
    let len = mpos_vm::align_up(len, mpos_vm::PAGE_SIZE);
    let virt = MMIO_NEXT.fetch_add(len, Ordering::Relaxed);
    if virt + len > MMIO_LIMIT {
        return None;
    }
    crate::hal::map_mmio(virt, phys, len);
    Some(virt)
}
