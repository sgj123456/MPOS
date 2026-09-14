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

pub struct VirtioBlk {
    /// The device, behind a lock: a request needs the crate's `&mut`, and
    /// [`BlockDev`] hands out `&self` because a filesystem takes the whole
    /// device and shares it with everyone it mounts it for.
    device: Mutex<RefCell<VirtIOBlk<HalImpl, PciTransport>>>,
    sectors: u64,
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
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device
                .read_blocks((offset / SECTOR) as usize, buf)
                .expect("virtio-blk reported a read error");
        });
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        assert!(
            offset.is_multiple_of(SECTOR) && buf.len().is_multiple_of(SECTOR as usize),
            "virtio-blk write {offset:#x}+{} breaks the sector contract",
            buf.len()
        );
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device
                .write_blocks((offset / SECTOR) as usize, buf)
                .expect("virtio-blk reported a write error");
        });
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
