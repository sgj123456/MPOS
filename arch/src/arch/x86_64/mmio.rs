//! Where a PCIe BAR's registers go, on a machine whose devices have no fixed
//! address for them.
//!
//! A device window is the one thing both machines agree about the *address* of:
//! a hole in the kernel half immediately below the image at
//! `0xffff_ffff_8000_0000`, which both four-level formats have room for. What
//! they do with it is where they part. This machine's BARs are wherever the
//! firmware put them and a driver learns the address only when it enumerates the
//! bus, so each window gets a mapping of its own, allocated here; the second
//! machine's device window is a fixed range its boot maps whole and translates
//! into by offset (`crate::arch::aarch64::paging::device_virt`), so it has
//! nothing to allocate.
//!
//! Both answers reach a driver through the same name - `hal::map_mmio_window`,
//! which is what a `virtio` or `ahci` driver calls - and neither driver learns
//! which mechanism answered. This module is the x86_64 half of that.
//!
//! The allocator used to live in `mm` as `mm::map_mmio`, behind an
//! `mpos_dynamic_mmio_window` capability flag. It is here instead because it is
//! the machine's answer to a machine question, and because it is what
//! `impl hal::Paging for Current`'s `map_mmio_window` returns - a body that must
//! not reach up into the kernel once `arch` is a crate of its own
//! (`NOTES.md` §14.3).

use core::sync::atomic::{AtomicUsize, Ordering};

/// The first byte of the window: a hole in the kernel half left for device
/// registers, `0xffff_ffff_8000_0000` minus 1 GiB.
const MMIO_BASE: usize = 0xffff_ffff_4000_0000;

/// One past the end of the window: 1 GiB of address space, which is room for
/// every BAR a machine could plausibly have.
const MMIO_LIMIT: usize = 0xffff_ffff_8000_0000;

/// The next free byte in that window.
static MMIO_NEXT: AtomicUsize = AtomicUsize::new(MMIO_BASE);

/// Map a device's registers into the kernel half and return the address they
/// can be reached through, or `None` when the window is full.
///
/// Called once per BAR, at the moment a driver decides it wants the device.
/// Every mapping goes on the same bump pointer and is never handed back: a
/// device driver keeps its registers for the life of the machine, so there is
/// nothing to free and no allocator state to make it worth one.
pub fn window(phys: usize, len: usize) -> Option<usize> {
    let len = mpos_vm::align_up(len, mpos_vm::PAGE_SIZE);
    let virt = MMIO_NEXT.fetch_add(len, Ordering::Relaxed);
    if virt + len > MMIO_LIMIT {
        return None;
    }
    crate::hal::map_mmio(virt, phys, len);
    Some(virt)
}
