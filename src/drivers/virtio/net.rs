//! virtio-net, through the `virtio-drivers` crate.
//!
//! The crate owns the hard parts - the two queues, the receive buffers it
//! posts and recycles, the ten- or twelve-byte header in front of every frame.
//! What is left here is which device to open and the four words the rest of
//! the kernel needs from it: what address it answers to, how to send a frame,
//! how to get one, and how to acknowledge the interrupt a frame raises.
//!
//! The device is driven both ways. A background task polls once a tick and
//! the socket paths poll inline, so progress never depends on the interrupt;
//! and when the adapter can be armed for MSI-X ([`msix`]), a frame that lands
//! between polls interrupts immediately, the handler polls and wakes the
//! waiters, and the tick is only a safety net instead of the discovery
//! mechanism. The polling is what a kernel without the interrupt machinery
//! falls back to, and what keeps it honest once the machinery is there.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::DeviceFunction;

use crate::drivers::pci;

use super::NetDevice;
use super::hal::{HalImpl, pci_root};
#[cfg(mpos_msi)]
use super::msix;

const VENDOR_VIRTIO: u16 = 0x1af4;
/// The transitional device id for a network adapter; a modern-only device
/// answers 0x1041 instead.
const DEVICE_NET: u16 = 0x1000;

/// The adapter this machine has, whichever transport the probe opened it over.
///
/// One slot for the machine's network device, and it exists because two steps of
/// the boot cannot be the same step: the device is *found* as soon as the kernel
/// can reach device memory, while the *stack* over it (`net::init` and the
/// service task) needs a running scheduler. So the probe publishes here and the
/// bring-up reads here, and neither has to know which bus answered or when the
/// other ran. A machine whose card never probed leaves this `None`, and the
/// socket calls answer `ENETDOWN` - which is what a `None` from [`device`] means
/// to `net::init`.
static ADAPTER: Mutex<RefCell<Option<Arc<dyn NetDevice>>>> = Mutex::new(RefCell::new(None));

/// Publish `dev` as this machine's network adapter. Called by the probe.
pub fn install(dev: Arc<dyn NetDevice>) {
    critical_section::with(|cs| {
        *ADAPTER.borrow(cs).borrow_mut() = Some(dev);
    });
}

/// The adapter the probe published, if it found one.
pub fn device() -> Option<Arc<dyn NetDevice>> {
    critical_section::with(|cs| ADAPTER.borrow(cs).borrow().clone())
}

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

    /// Acknowledge the device's interrupt, if the adapter raised one.
    ///
    /// The interrupt handler's second half: the first (the stack's poll)
    /// drained what the frames there was to drain, and this clears the
    /// interrupt status so the next arrival can raise its vector again.
    pub fn ack_interrupt(&self) {
        critical_section::with(|cs| {
            let _ = self.device.borrow(cs).borrow_mut().ack_interrupt();
        });
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

        // Mutable only where there is an interrupt to enable: on a machine with
        // no MSI controller the arming below does not exist and nothing writes
        // through the binding again.
        #[cfg_attr(not(mpos_msi), allow(unused_mut))]
        let mut device = VirtIONet::<HalImpl, _, QUEUE_SIZE>::new(transport, BUFFER_BYTES).ok()?;
        let mac = device.mac_address();

        // Arm the message-signalled interrupt, where this machine has an MSI
        // controller to arm: the vector table entry first, then the queues'
        // binding to it - the order the virtio spec demands, entry ready before
        // the queue names it. Either step failing leaves the machine exactly as
        // it was, polled, which is what it ran as before any of this existed.
        // The APIC's EOI register is mapped first as well: the handler's last
        // write goes there, and an unmapped write from interrupt context is a
        // panic nothing recovers from.
        //
        // Gated on the capability, not on a machine: a port whose devices are
        // wired to a GIC has no message-signalled delivery at all, so there is
        // nothing to arm and the adapter stays polled - which is the same state
        // this code leaves behind when the arming fails.
        #[cfg(mpos_msi)]
        {
            crate::hal::init_message_signalled();
            if msix::enable(&func, crate::hal::net_msi_vector()) && msix::set_queue_vectors(&func) {
                // With the queues told whom to interrupt, the notification
                // suppression flags come off. (A no-op while `VIRTIO_F_EVENT_IDX`
                // is negotiated - the crate re-arms that way - and the honest
                // spelling of the same request the day it is not.)
                device.enable_interrupts();
            }
        }

        // The device is this driver's from here, which is what `lspci -k`
        // reads back.
        pci::claim(&func, super::PCI_DRIVER);

        Some(Arc::new(VirtioNet {
            device: Mutex::new(RefCell::new(device)),
            mac,
        }))
    }
}

/// The PCI transport's implementation of the seam `net` holds. Each method is
/// the inherent one above - this block exists so the stack can name the
/// *contract* and never `virtio-pci`, which is what lets a machine reach the
/// same family over MMIO without a second stack.
impl NetDevice for VirtioNet {
    fn mac(&self) -> [u8; 6] {
        self.mac()
    }

    fn rx_frames(&self) -> usize {
        self.rx_frames()
    }

    fn poll(&self) -> Option<Vec<u8>> {
        self.poll()
    }

    fn transmit(&self, frame: &[u8]) -> bool {
        self.transmit(frame)
    }

    fn ack_interrupt(&self) {
        self.ack_interrupt()
    }
}
