//! The bridge between the `virtio-drivers` crate and this kernel.
//!
//! The crate is written against a [`Hal`] - a host abstraction layer - because
//! it cannot know how the kernel it lives in hands out memory or reaches a
//! device's registers. Everything it asks for, it asks for here, and every
//! answer is short because of what this kernel already is:
//!
//! * There is a direct map of all physical memory, so a physical address is a
//!   virtual address with a constant added, and one helper in each direction
//!   answers both `mmio_phys_to_virt` and `share`.
//! * There is no IOMMU and no bounce buffering, so a buffer anywhere in the
//!   kernel's own memory is already visible to the device: `share` computes an
//!   address and `unshare` does nothing at all.
//!
//! A kernel with an IOMMU, or one that had to copy buffers into a region a
//! device could reach, would have real work to do in those two. This one does
//! not, and pretending otherwise would only add a layer that cannot fail.

use core::ptr::NonNull;

use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction, PciRoot};
use virtio_drivers::{BufferDirection, Hal, PhysAddr};

use crate::drivers::pci;
use crate::mm::{self, PAGE_SIZE};

/// The `Hal` implementation the virtio devices are built on.
pub struct HalImpl;

unsafe impl Hal for HalImpl {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let phys = mm::alloc_frames(order).expect("out of frames for a virtio DMA allocation");
        let virt = mm::phys_to_virt(phys) as *mut u8;
        // Zeroed, as the crate requires: the descriptor tables the device
        // reads before the driver has written anything are this memory.
        unsafe { core::ptr::write_bytes(virt, 0, pages * PAGE_SIZE) };
        (
            phys as PhysAddr,
            NonNull::new(virt).expect("the direct map is never null"),
        )
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        mm::free_frames(paddr as usize, order);
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
        // A BAR is not memory, so the direct map does not reach it: this is a
        // window of the kernel half mapped on demand, one mapping per BAR.
        NonNull::new(mm::map_mmio(paddr as usize, size) as *mut u8)
            .expect("an MMIO window is never null")
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        mm::virt_to_phys(buffer.as_ptr() as *mut u8 as usize) as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing was copied, so nothing has to be copied back.
    }
}

/// PCI configuration space, through the I/O ports this kernel already speaks.
///
/// The crate ships an MMIO implementation for the enhanced configuration
/// mechanism, and none for the port-based one that every x86_64 machine has
/// and that this kernel's PCI layer was already built on - so this is the
/// dozen lines that let the crate's PCI transport use it.
pub struct IoCam;

impl ConfigurationAccess for IoCam {
    fn read_word(&self, df: DeviceFunction, register: u8) -> u32 {
        pci::read32(df.bus, df.device, df.function, register)
    }

    fn write_word(&mut self, df: DeviceFunction, register: u8, data: u32) {
        pci::write32(df.bus, df.device, df.function, register, data)
    }

    unsafe fn unsafe_clone(&self) -> Self {
        IoCam
    }
}

/// A PCI root complex over [`IoCam`].
pub fn pci_root() -> PciRoot<IoCam> {
    PciRoot::new(IoCam)
}

/// The virtio vendor: every device the crate can drive answers to it.
const VENDOR_VIRTIO: u16 = 0x1af4;

/// The command bits every virtio device is brought up with. Memory decoding
/// is not optional the way it was for a port-mapped device - the transport
/// talks through an MMIO BAR - and bus mastering is what lets the device
/// DMA at all. Interrupts stay masked: every driver here polls, and a
/// completion raising an unhandled line is worse than one nobody sees.
const COMMAND_ON: u16 = pci::COMMAND_IO_SPACE
    | pci::COMMAND_MEMORY_SPACE
    | pci::COMMAND_BUS_MASTER
    | pci::COMMAND_INTX_DISABLE;

/// Probe the PCI bus and hand back a transport for the first virtio device
/// answering one of `device_ids`.
///
/// The virtio vendor owns two ID ranges. The transitional devices answer the
/// legacy IDs (0x1000 upwards): QEMU's `virtio-blk`, `virtio-9p` and their
/// kin, which carry the modern capabilities alongside the legacy registers -
/// and it is the capabilities the transport below reads. The devices that
/// never had a legacy form - the GPU, input, the socket, the sound card -
/// answer the modern range (0x1040 upwards) and nothing else. A driver names
/// the ID it expects, this finds the function behind it.
pub fn transport(device_ids: &[u16]) -> Option<PciTransport> {
    transport_all(device_ids).into_iter().next()
}

/// Every device answering one of `device_ids`, as a transport, in bus
/// order. The input family is the caller this exists for: one PCI ID, any
/// number of devices behind it.
pub fn transport_all(device_ids: &[u16]) -> alloc::vec::Vec<PciTransport> {
    pci::find_any(VENDOR_VIRTIO, device_ids)
        .into_iter()
        .filter_map(|func| {
            func.command_on(COMMAND_ON);
            let (bus, device, function) = func.bdf();
            let mut root = pci_root();
            let transport = PciTransport::new::<HalImpl, _>(
                &mut root,
                DeviceFunction {
                    bus,
                    device,
                    function,
                },
            )
            .ok()?;
            // The function is this driver's from here, which is what
            // `lspci -k` reads back.
            pci::claim(&func, super::PCI_DRIVER);
            Some(transport)
        })
        .collect()
}

/// Declare the first function answering `device_ids` to be the home of a
/// DRM/KMS card, so `sysfs` hangs the card's `card0`/`renderD128` nodes off it.
///
/// A display driver calls this for its own device, once the device is live: it
/// is the driver, not the tree builder, that knows the function it bound
/// carries the card. The fact is written into the shared `pci` record
/// (`crate::pci::mark_drm_card`), which `vfs::sysfs` reads - the two never reach
/// into each other, and the machine's q35 firmware VGA, which no driver marks,
/// is not mistaken for the card (`PLAN.md` §7.4).
pub fn claim_drm_card(device_ids: &[u16]) {
    if let Some(func) = pci::find_any(VENDOR_VIRTIO, device_ids).into_iter().next() {
        pci::mark_drm_card(&func);
    }
}

/// The virtio *device configuration* window of the first device answering one
/// of `device_ids`: the space a driver polls for events the device itself
/// reports.
///
/// The transport reads this space at bring-up and keeps what it read; a driver
/// whose device can be told something *while the machine runs* has to look
/// again later, and this is the address it looks at. `virtio-gpu` is the one
/// caller: the host raises `VIRTIO_GPU_EVENT_DISPLAY` in `events_read` when the
/// display it wants us to drive changes shape, and nothing else in this kernel
/// can see a window resize.
pub fn device_config(device_ids: &[u16]) -> Option<usize> {
    let func = pci::find_any(VENDOR_VIRTIO, device_ids)
        .into_iter()
        .next()?;
    super::msix::device_config_of(&func)
}
