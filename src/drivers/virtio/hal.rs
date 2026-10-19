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
//!
//! [`HalImpl`] is machine-neutral: after the MMIO-mapping seam (`NOTES.md` §9.1)
//! its one x86_64-only call - `crate::mm::map_mmio` - became
//! `crate::hal::map_mmio_window`, so `HalImpl` compiles on aarch64 too and the
//! aarch64 MMIO devices build on it. The PCI probe-and-open helpers below it are
//! x86_64's machine, so they are `#[cfg]`-ed out on aarch64 (which reaches its
//! virtio devices over the memory-mapped transport, not a BAR).

use core::ptr::NonNull;

use virtio_drivers::{BufferDirection, Hal, PhysAddr};

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
        // window of the kernel half mapped on demand, one mapping per BAR. The
        // *machine* answers the kernel-virtual address through `crate::hal` -
        // not `crate::mm::map_mmio` directly, which is x86_64-only - so this
        // module reaches a device's registers the one way every layer above the
        // machine layer is allowed to (`NOTES.md` §9.1's drivers-stage seam).
        let virt = crate::hal::map_mmio_window(paddr as usize, size)
            .expect("a mapped MMIO window is never null");
        NonNull::new(virt as *mut u8).expect("an MMIO window is never null")
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        mm::virt_to_phys(buffer.as_ptr() as *mut u8 as usize) as PhysAddr
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing was copied, so nothing has to be copied back.
    }
}

// ---------------------------------------------------------------------------
// The PCI transport. Every item below reaches a device through a BAR, over the
// configuration space `drivers::pci` reads - by port I/O on one machine and
// through the ECAM window its firmware described on another, which is that
// module's own decision and made once at boot. So this is not one machine's
// code: a machine whose devices hang off a bus compiles it and finds them; a
// machine with no bus finds none (`drivers::pci` answers an empty bus, and a
// machine that describes its devices as memory-mapped windows reaches them
// through `crate::drivers::virtio::mmio` instead). The one part that *is*
// hardware-bound is the message-signalled arming, which needs an MSI controller
// and is gated on `mpos_msi` (`device_config` below, and `net.rs`).

mod pci_transport {
    use super::*;

    use virtio_drivers::transport::pci::PciTransport;
    use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction, PciRoot};

    use crate::drivers::pci;

    /// PCI configuration space, through whatever mechanism the kernel chose.
    ///
    /// The crate ships an MMIO implementation for the enhanced configuration
    /// mechanism, and none for the port-based one that every x86_64 machine has -
    /// so this is the dozen lines that let the crate's PCI transport use it, over
    /// `drivers::pci`'s four accessors. Which of the two mechanisms those reach is
    /// `drivers::pci`'s decision, made once at boot and checked against the other
    /// one there (`docs/NOTES.md` §7.6); nothing here learns which answered.
    pub struct IoCam;

    impl ConfigurationAccess for IoCam {
        fn read_word(&self, df: DeviceFunction, register: u8) -> u32 {
            pci::read32(df.bus, df.device, df.function, u16::from(register))
        }

        fn write_word(&mut self, df: DeviceFunction, register: u8, data: u32) {
            pci::write32(df.bus, df.device, df.function, u16::from(register), data)
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
                pci::claim(&func, super::super::PCI_DRIVER);
                Some(transport)
            })
            .collect()
    }

    /// Declare the first function answering `device_ids` to be the home of a
    /// DRM/KMS card, so `sysfs` hangs the card's `card0`/`renderD128` nodes off
    /// it.
    ///
    /// A display driver calls this for its own device, once the device is live:
    /// it is the driver, not the tree builder, that knows the function behind it
    /// carries the card. The fact is written into the shared `pci` record
    /// (`crate::pci::mark_drm_card`), which `vfs::sysfs` reads - the two never
    /// reach into each other, and the machine's q35 firmware VGA, which no driver
    /// marks, is not mistaken for the card (`NOTES.md` §7.4).
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
    #[cfg(mpos_msi)]
    pub fn device_config(device_ids: &[u16]) -> Option<usize> {
        let func = pci::find_any(VENDOR_VIRTIO, device_ids)
            .into_iter()
            .next()?;
        super::super::msix::device_config_of(&func)
    }
}

/// The virtio *device configuration* window of the first device answering one of
/// `device_ids`, where a driver polls for events the device itself reports.
///
/// A capability, not a machine: the window is the one the MSI-X table shares
/// (`msix::device_config_of`), so a machine with no MSI controller has no such
/// address to hand out and this does not exist for it (`mpos_msi`).
#[cfg(mpos_msi)]
pub use pci_transport::device_config;
pub use pci_transport::{claim_drm_card, pci_root, transport, transport_all};
