//! Virtio over the memory-mapped transport, the aarch64 machine's path.
//!
//! x86_64 reaches its virtio devices through a PCI BAR; `virt` has no PCI for
//! them - its thirty-two `virtio,mmio` windows are fixed physical ranges the
//! device tree names and the boot maps into the kernel half. The `virtio-drivers`
//! crate drives both the same way once it is handed a mapped register block: this
//! module's job is only to turn a device-window address into a transport and open
//! a device on it, so the crate owns the queue mechanics exactly as it does over
//! PCI.
//!
//! It is aarch64-only: `crate::drivers::virtio` gates this in where `rng`/`blk`/
//! `net` are gated to the PCI path (`#[cfg(target_arch = "x86_64")]`), and the
//! caller is a boot milestone check in `arch`, which owns reading the tree and
//! deciding which window carries which device. The driver is handed an address,
//! never a tree node - so it names neither `crate::boot` nor `crate::fdt`.

use core::ptr::NonNull;

use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use super::hal::HalImpl;

/// Fill `out` with host-generated entropy from the virtio-rng whose register
/// block is mapped at kernel-virtual address `virt` and spans `size` bytes.
///
/// Returns how many bytes landed - the whole buffer for a healthy device. A zero
/// is an honest "no entropy", not a success: the caller treats a short read as a
/// failed check rather than papering over a device that did not answer. The
/// transport and device are built and dropped within the call, so this leaves no
/// global behind - the `/dev/hwrng` and seed-mix wiring over MMIO come once the
/// device has a stable home like the PCI one has.
pub fn fill_entropy(virt: usize, size: usize, out: &mut [u8]) -> usize {
    // SAFETY: `virt` is an address inside the fixed device window the boot mapped
    // whole as Device-nGnRnE and never executable (`paging::device_virt` only
    // answers for a range wholly inside it), and `size` is the register block the
    // device tree reports for this transport. The header is volatile device
    // memory, which is what `MmioTransport` reads.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    // SAFETY: the caller's contract above - `header` is a valid mapped device
    // register block of at least `size` bytes.
    let transport = match unsafe { MmioTransport::new(header, size) } {
        Ok(transport) => transport,
        Err(_) => return 0,
    };
    let mut device = match VirtIORng::<HalImpl, _>::new(transport) {
        Ok(device) => device,
        Err(_) => return 0,
    };
    device.request_entropy(out).unwrap_or(0)
}
