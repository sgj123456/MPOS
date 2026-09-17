//! virtio-blk, through the `virtio-drivers` crate.
//!
//! This used to be a hand-written legacy driver: an I/O-port BAR, three
//! descriptors per request, and a spin on the used ring. It is not any more -
//! the crate brings its own VirtIO 1.0 implementation, with the queue
//! machinery, the feature negotiation and the configuration parsing that a
//! driver has to get right before a single sector can move.
//!
//! What is left here is the part that could not be replaced: which device this
//! kernel talks to, and [`BlockDev`], the interface the rest of the kernel
//! reaches it through. Requests still go one at a time - the crate could queue
//! more, nothing above this asks, and a filesystem that sees a synchronous
//! device cannot tell the difference.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::DeviceFunction;

use crate::blk::BlockDev;
use crate::drivers::pci;

use super::hal::{HalImpl, pci_root};

const VENDOR_VIRTIO: u16 = 0x1af4;
/// QEMU hands us the transitional device, which answers the legacy id and
/// carries the modern capabilities alongside the legacy registers - and it is
/// the capabilities the transport below reads.
const DEVICE_BLK: u16 = 0x1001;

/// A block, as the device counts them.
const SECTOR: u64 = 512;

/// Sectors a bounce buffer holds. Four of them - 2 KiB - which is what the
/// probing reads that land here actually ask for, and small enough to sit on a
/// 16 KiB kernel stack without a thought.
const BOUNCE_SECTORS: usize = 4;

pub struct VirtioBlk {
    /// The device, behind a lock: a request needs the crate's `&mut`, and
    /// [`BlockDev`] hands out `&self` because a filesystem takes the whole
    /// device and shares it with everyone it mounts it for.
    device: Mutex<RefCell<VirtIOBlk<HalImpl, PciTransport>>>,
    sectors: u64,
}

impl VirtioBlk {
    /// Whether `ptr` names memory a request can hand to the device.
    ///
    /// A descriptor carries a *physical* address, and only the direct map
    /// turns a virtual one into that, by subtraction. Anything else is not
    /// merely unmappable but unshareable: a user buffer is scattered across
    /// physical pages that were handed out one at a time, so even an address
    /// that translated would send the device to the wrong place. The syscalls
    /// do hand user buffers down - `read`, `readv` and `pread64` pass the
    /// caller's pointer straight to the filesystem, and a block *device* is
    /// read with no filesystem in between to break the fall.
    fn dma_safe(ptr: *const u8) -> bool {
        ptr as usize >= crate::mm::hhdm_offset()
    }

    /// Move `buf.len()` bytes at `offset` through the device, in pieces no
    /// larger than the bounce buffer, for a caller whose memory the device
    /// cannot reach.
    fn read_at_bounced(&self, offset: u64, buf: &mut [u8]) {
        let mut bounce = [0u8; SECTOR as usize * BOUNCE_SECTORS];
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(bounce.len());
            self.read_dma(offset + done as u64, &mut bounce[..chunk]);
            buf[done..done + chunk].copy_from_slice(&bounce[..chunk]);
            done += chunk;
        }
    }

    fn write_at_bounced(&self, offset: u64, buf: &[u8]) {
        let mut bounce = [0u8; SECTOR as usize * BOUNCE_SECTORS];
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(bounce.len());
            bounce[..chunk].copy_from_slice(&buf[done..done + chunk]);
            self.write_dma(offset + done as u64, &bounce[..chunk]);
            done += chunk;
        }
    }

    fn read_dma(&self, offset: u64, buf: &mut [u8]) {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device
                .read_blocks((offset / SECTOR) as usize, buf)
                .expect("virtio-blk reported a read error");
        });
    }

    fn write_dma(&self, offset: u64, buf: &[u8]) {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device
                .write_blocks((offset / SECTOR) as usize, buf)
                .expect("virtio-blk reported a write error");
        });
    }
}

impl BlockDev for VirtioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset.is_multiple_of(SECTOR) && buf.len().is_multiple_of(SECTOR as usize),
            "virtio-blk read {offset:#x}+{} breaks the sector contract",
            buf.len()
        );
        // The device answers an out-of-range request with a bare I/O error
        // that says nothing about which part of it was wrong. Refuse it here,
        // where the window is still visible.
        let first = offset / SECTOR;
        let count = buf.len() as u64 / SECTOR;
        assert!(
            first + count <= self.sectors,
            "virtio-blk read of {count} sectors at {first} runs past the {}-sector device",
            self.sectors
        );
        if !Self::dma_safe(buf.as_ptr()) {
            self.read_at_bounced(offset, buf);
            return;
        }
        self.read_dma(offset, buf);
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        assert!(
            offset.is_multiple_of(SECTOR) && buf.len().is_multiple_of(SECTOR as usize),
            "virtio-blk write {offset:#x}+{} breaks the sector contract",
            buf.len()
        );
        if !Self::dma_safe(buf.as_ptr()) {
            self.write_at_bounced(offset, buf);
            return;
        }
        self.write_dma(offset, buf);
    }
}

impl VirtioBlk {
    /// Probe the PCI bus and bring up the first virtio-blk device, if any.
    pub fn init() -> Option<Arc<dyn BlockDev>> {
        let func = pci::find(VENDOR_VIRTIO, DEVICE_BLK)?;
        // Decode memory and let the device reach main memory. The transport
        // talks to the device through an MMIO BAR, so memory decoding is not
        // optional the way it was for the port-based driver this replaced.
        func.command_on(
            pci::COMMAND_IO_SPACE
                | pci::COMMAND_MEMORY_SPACE
                | pci::COMMAND_BUS_MASTER
                | pci::COMMAND_INTX_DISABLE,
        );

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

        let device = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
        let sectors = device.capacity();
        assert!(sectors > 0, "virtio-blk reports a zero-capacity disk");

        Some(Arc::new(VirtioBlk {
            device: Mutex::new(RefCell::new(device)),
            sectors,
        }))
    }
}
