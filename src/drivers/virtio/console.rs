//! virtio-console, through the `virtio-drivers` crate.
//!
//! The virtio console is a second serial line, one the *host* drives: a
//! character device on a `virtio-serial` card whose far end is whatever
//! QEMU was told to wire it to - a file, a socket, another program. The
//! kernel's own console stays on the machine's UART; this device is
//! userland's, under the name Linux gives a hypervisor console: `/dev/hvc0`.
//!
//! The crate queues one receive buffer ahead of time and harvests it a byte
//! at a time; what is left here is the device the boot opened and the stream
//! face the `CharDevice` contract wants - read until the buffer is empty,
//! block while it stays that way, write straight through. Reads poll rather
//! than wait on an interrupt, for the same reason the network device's do:
//! nothing is wired to the line yet, and a poll per tick costs less than the
//! machinery to be told to look.
//!
//! What the boot opened is a [`ConsolePort`], not a transport: the crate's
//! driver is built over `PciTransport` on the first machine ([`init_global`])
//! and over the MMIO transport on the second (`super::mmio::console_device`),
//! and both wrap it in the same [`VirtioHvc`] - whose `size`/`drain`/`send` are
//! one implementation over the one crate driver. So the stream face, the device
//! node and the blocking read below are one copy on both machines, and the only
//! line that names a bus is the probe that opens it.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::console::VirtIOConsole;
use virtio_drivers::transport::Transport;

use super::ConsolePort;
use super::hal::HalImpl;
#[cfg(target_arch = "x86_64")]
use super::hal::transport;

/// The transitional device ID; a modern-only device answers 0x1043 instead.
/// QEMU exposes the console as a `virtio-serial` card with a `virtconsole`
/// port on it - one PCI function, which is what this probes for. A device that
/// arrives over an MMIO window is found by its own `DeviceID` (`3`) instead,
/// which is the machine layer's read, not this module's.
#[cfg(target_arch = "x86_64")]
const DEVICE_CONSOLE: &[u16] = &[0x1003, 0x1043];

/// A virtio console port over the transport it arrived on.
pub struct VirtioHvc<T: Transport> {
    /// The device, behind a lock: every operation the crate offers takes
    /// `&mut`, and the console is one device however many processes have
    /// `/dev/hvc0` open.
    device: Mutex<RefCell<VirtIOConsole<HalImpl, T>>>,
}

impl<T: Transport> VirtioHvc<T> {
    pub fn new(device: VirtIOConsole<HalImpl, T>) -> Self {
        VirtioHvc {
            device: Mutex::new(RefCell::new(device)),
        }
    }
}

impl<T: Transport + Send> ConsolePort for VirtioHvc<T> {
    /// The console's size, as the device reports it - columns and rows, for a
    /// host that wired the port to something with a shape. `None` when the
    /// device does not know or the read failed.
    fn size(&self) -> Option<(u16, u16)> {
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
}

/// The port the boot found, if it found one. Set once by [`install`], read by
/// the devfs builder.
static GLOBAL: Mutex<RefCell<Option<Arc<dyn ConsolePort>>>> = Mutex::new(RefCell::new(None));

/// Make `port` the machine's console port, and hand it back.
///
/// The one door in, and it takes the contract rather than a bus: the first
/// machine arrives through [`init_global`], the second with the port its MMIO
/// window opened, and everything above - the devfs node, the stream face - is
/// the same either way.
pub fn install(port: Arc<dyn ConsolePort>) -> Arc<dyn ConsolePort> {
    critical_section::with(|cs| {
        *GLOBAL.borrow(cs).borrow_mut() = Some(port.clone());
    });
    port
}

/// The port, if the machine has one.
pub fn global() -> Option<Arc<dyn ConsolePort>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// Probe the PCI bus for the virtio console port and record it under the name
/// userland will open.
///
/// The first machine's door into [`install`]; a machine that reaches the same
/// device over an MMIO window opens it there and installs it the same way.
#[cfg(target_arch = "x86_64")]
pub fn init_global() -> Option<Arc<dyn ConsolePort>> {
    let device = VirtIOConsole::<HalImpl, _>::new(transport(DEVICE_CONSOLE)?).ok()?;
    Some(install(Arc::new(VirtioHvc::new(device))))
}

/// The `CharDevice` face of the port, for the devfs builder: a stream that
/// blocks on read the way every serial line does, and writes through.
///
/// Blocking is a poll loop with a tick of sleep, not a waiter list: the
/// bytes arrive from the host, and no interrupt will name this task when
/// they do - the network device's `receive` blocks the same way for the same
/// reason.
pub struct HvcDev(Arc<dyn ConsolePort>);

impl HvcDev {
    pub fn new(dev: Arc<dyn ConsolePort>) -> Self {
        HvcDev(dev)
    }
}

impl crate::vfs::devfs::CharDevice for HvcDev {
    fn read(&self, buf: &mut [u8]) -> crate::vfs::VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let got = self.0.drain(buf);
            if got > 0 {
                return Ok(got);
            }
            // A signal the caller can catch ends the wait with `EINTR`, not
            // with 0: on this door 0 *is* end of input, and a program told its
            // hypervisor console had closed would stop reading instead of
            // retrying after its handler ran.
            if crate::sched::has_deliverable_signal() {
                return Err(crate::vfs::VfsError::Interrupted);
            }
            crate::sched::sleep_ticks(1);
        }
    }

    fn write(&self, buf: &[u8]) -> usize {
        if self.0.send(buf) { buf.len() } else { 0 }
    }
}
