//! virtio-console, through the `virtio-drivers` crate.
//!
//! The virtio console is a second serial line, one the *host* drives: a
//! character device on a `virtio-serial` PCI card whose far end is whatever
//! QEMU was told to wire it to - a file, a socket, another program. The
//! kernel's own console stays on the 16550 UART; this device is userland's,
//! under the name Linux gives a hypervisor console: `/dev/hvc0`.
//!
//! The crate queues one receive buffer ahead of time and harvests it a byte
//! at a time; what is left here is the device the boot opened and the stream
//! face the `CharDevice` contract wants - read until the buffer is empty,
//! block while it stays that way, write straight through. Reads poll rather
//! than wait on an interrupt, for the same reason the network device's do:
//! nothing is wired to the line yet, and a poll per tick costs less than the
//! machinery to be told to look.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::console::VirtIOConsole;
use virtio_drivers::transport::pci::PciTransport;

use super::hal::{HalImpl, transport};

/// The transitional device ID; a modern-only device answers 0x1043 instead.
/// QEMU exposes the console as a `virtio-serial` card with a `virtconsole`
/// port on it - one PCI function, which is what this probes for.
const DEVICE_CONSOLE: &[u16] = &[0x1003, 0x1043];

pub struct VirtioHvc {
    /// The device, behind a lock: every operation the crate offers takes
    /// `&mut`, and the console is one device however many processes have
    /// `/dev/hvc0` open.
    device: Mutex<RefCell<VirtIOConsole<HalImpl, PciTransport>>>,
}

impl VirtioHvc {
    /// Drain whatever the host has written into the port, up to `buf`'s
    /// length. The crate hands bytes over one at a time, so this is a loop
    /// until it answers "nothing more" - each `recv` also harvests the
    /// receive buffer the device filled since the last call.
    fn drain(&self, buf: &mut [u8]) -> usize {
        let mut done = 0;
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            while done < buf.len() {
                match device.recv(true) {
                    Ok(Some(byte)) => {
                        buf[done] = byte;
                        done += 1;
                    }
                    _ => break,
                }
            }
        });
        done
    }

    /// Hand `buf` to the host. The device consumes the chain synchronously,
    /// so the whole buffer goes as one request however large it is.
    fn send(&self, buf: &[u8]) -> bool {
        critical_section::with(|cs| self.device.borrow(cs).borrow_mut().send_bytes(buf).is_ok())
    }

    /// The console's size, as the device reports it - columns and rows, for
    /// a host that wired the port to something with a shape. `None` when the
    /// device does not know or the read failed.
    pub fn size(&self) -> Option<(u16, u16)> {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .size()
                .ok()
                .flatten()
                .map(|size| (size.columns, size.rows))
        })
    }

    /// Probe the PCI bus and bring up the first virtio console port.
    pub fn init() -> Option<Arc<Self>> {
        let device = VirtIOConsole::<HalImpl, _>::new(transport(DEVICE_CONSOLE)?).ok()?;
        Some(Arc::new(VirtioHvc {
            device: Mutex::new(RefCell::new(device)),
        }))
    }
}

/// The port the boot found, if it found one. Set once by [`init_global`],
/// read by the devfs builder.
static GLOBAL: Mutex<RefCell<Option<Arc<VirtioHvc>>>> = Mutex::new(RefCell::new(None));

/// Probe for the device and record it under the name userland will open.
pub fn init_global() -> Option<Arc<VirtioHvc>> {
    let dev = VirtioHvc::init()?;
    critical_section::with(|cs| {
        let mut slot = GLOBAL.borrow(cs).borrow_mut();
        *slot = Some(dev.clone());
    });
    Some(dev)
}

/// The port, if the machine has one.
pub fn global() -> Option<Arc<VirtioHvc>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// The `CharDevice` face of the port, for the devfs builder: a stream that
/// blocks on read the way every serial line does, and writes through.
///
/// Blocking is a poll loop with a tick of sleep, not a waiter list: the
/// bytes arrive from the host, and no interrupt will name this task when
/// they do - the network device's `receive` blocks the same way for the same
/// reason.
pub struct HvcDev(Arc<VirtioHvc>);

impl HvcDev {
    pub fn new(dev: Arc<VirtioHvc>) -> Self {
        HvcDev(dev)
    }
}

impl crate::vfs::devfs::CharDevice for HvcDev {
    fn read(&self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        loop {
            let got = self.0.drain(buf);
            if got > 0 {
                return got;
            }
            if crate::sched::has_deliverable_signal() {
                return 0;
            }
            crate::sched::sleep_ticks(1);
        }
    }

    fn write(&self, buf: &[u8]) -> usize {
        if self.0.send(buf) { buf.len() } else { 0 }
    }
}
