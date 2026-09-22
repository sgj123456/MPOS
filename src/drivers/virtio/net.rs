//! virtio-net, through the `virtio-drivers` crate.
//!
//! The crate owns the hard parts - the two queues, the receive buffers it
//! posts and recycles, the ten- or twelve-byte header in front of every frame.
//! What is left here is which device to open and the three words the rest of
//! the kernel needs from it: what address it answers to, how to send a frame,
//! and how to get one.
//!
//! The device is polled rather than interrupted. There is no interrupt handler
//! wired to it yet, and on one core a look at the receive queue costs less
//! than the machinery to be told to look.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::DeviceFunction;

use crate::drivers::pci;

use super::hal::{HalImpl, pci_root};

const VENDOR_VIRTIO: u16 = 0x1af4;
/// The transitional device id for a network adapter; a modern-only device
/// answers 0x1041 instead.
const DEVICE_NET: u16 = 0x1000;

/// Slots in each queue. The receive side keeps this many buffers posted, so a
/// burst that arrives while the kernel is busy is not dropped for want of
/// somewhere to put it.
///
/// This number is not free: it is what the stack advertises as its receive
/// window (see [`VirtioNet::rx_frames`]). Sixty-four 2 KiB buffers are 128 KiB
/// of ring, which is more than a 64 KiB socket buffer can ever hand back - so
/// the ring is never what loses a packet, and the window is not what caps the
/// transfer. Sixteen was small enough that both were true at once.
const QUEUE_SIZE: usize = 64;

/// The size of one receive buffer: a frame and its header, with room to spare.
const BUFFER_BYTES: usize = 2048;

pub struct VirtioNet {
    /// The device, behind a lock: every operation the crate offers needs
    /// `&mut`, and the network stack holds the device by `Arc` and shares it.
    device: Mutex<RefCell<VirtIONet<HalImpl, PciTransport, QUEUE_SIZE>>>,
    mac: [u8; 6],
}

impl VirtioNet {
    /// Which address this device answers to.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// How many frames the receive ring can hold at once.
    ///
    /// The stack's TCP window is computed from this, because it is the answer
    /// to the only question that matters for a window: how much can arrive
    /// before something has to be dropped. Advertising more than the ring
    /// holds loses packets the peer has every reason to think arrived.
    pub fn rx_frames(&self) -> usize {
        QUEUE_SIZE
    }

    /// Take one frame the device has received, if there is one.
    ///
    /// The buffer goes straight back to the device: a buffer the driver kept
    /// is a buffer the device cannot fill, and the queue would shrink by one
    /// every time a frame arrived.
    pub fn poll(&self) -> Option<Vec<u8>> {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            if !device.can_recv() {
                return None;
            }
            match device.receive() {
                Ok(received) => {
                    let frame = Vec::from(received.packet());
                    let _ = device.recycle_rx_buffer(received);
                    Some(frame)
                }
                Err(_) => None,
            }
        })
    }

    /// Hand one frame to the device.
    pub fn transmit(&self, frame: &[u8]) -> bool {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            if !device.can_send() {
                return false;
            }
            let mut buffer = device.new_tx_buffer(frame.len());
            buffer.packet_mut().copy_from_slice(frame);
            device.send(buffer).is_ok()
        })
    }

    /// Take one frame, giving the rest of the system a turn when there is none.
    pub fn receive(&self) -> Vec<u8> {
        loop {
            if let Some(frame) = self.poll() {
                return frame;
            }
            crate::sched::sleep_ticks(1);
        }
    }

    /// Probe the PCI bus and bring up the first virtio-net device, if any.
    pub fn init() -> Option<Arc<VirtioNet>> {
        let func = pci::find(VENDOR_VIRTIO, DEVICE_NET)?;
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

        let device = VirtIONet::<HalImpl, _, QUEUE_SIZE>::new(transport, BUFFER_BYTES).ok()?;
        let mac = device.mac_address();

        Some(Arc::new(VirtioNet {
            device: Mutex::new(RefCell::new(device)),
            mac,
        }))
    }
}
