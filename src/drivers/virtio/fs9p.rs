//! virtio-9p, the transport half, through the `virtio-drivers` crate.
//!
//! The crate's 9p device is a wire, not a filesystem: it carries a request
//! buffer to the host's 9p server and brings the answer back, and says
//! nothing about what a `Twalk` is. The protocol that rides this wire - the
//! 9P2000 dialect, the fid table, the mapping onto the [`Inode`] contract -
//! lives in `src/vfs/p9.rs`, because that is where every other filesystem
//! in this kernel lives. What stays here is what stayed with blk and net:
//! which device to open, and the lock discipline around the one queue the
//! host answers on.
//!
//! The lock is the whole of the multiplexing story. A 9p server is free to
//! answer requests out of order, but this device has one virtqueue and the
//! crate's `request` posts, waits and pops in a single call - so one
//! request in flight at a time, and a reader blocked on its answer does not
//! hold the lock the next requester needs, because the lock is held only
//! around the request itself. The protocol client above makes each of its
//! round trips one `request`, which is what makes that discipline enough.
//!
//! What the boot opened is a [`P9Device`], not a transport: the crate's driver is
//! built over `PciTransport` on the first machine ([`init_global`]) and over the
//! MMIO transport on the second (`super::mmio::p9_device`), and both wrap it in the
//! same [`Virtio9p`] - whose two methods are one implementation over the one crate
//! driver. So the mount table, the boot's own mount and `mount -t 9p` above it are
//! one copy on both machines, and the only lines that name a bus are the two
//! probes that open it (`PLAN.md` §13.29).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::virtio_9p::VirtIO9p;
use virtio_drivers::transport::Transport;

use super::P9Device;
use super::hal::HalImpl;
#[cfg(target_arch = "x86_64")]
use super::hal::transport_all;

/// The transitional device ID; a modern-only device answers 0x1049 instead.
/// QEMU's `-virtfs` exports answer the legacy ID, carrying the modern
/// capabilities alongside the legacy registers - the same shape as the
/// block device this kernel already probes. The MMIO side finds its window by
/// this device's own `DeviceID` (`9`) instead, which is the machine layer's
/// read, not this module's.
#[cfg(target_arch = "x86_64")]
const DEVICE_9P: &[u16] = &[0x1009, 0x1049];

/// A virtio-9p device over the transport it arrived on - `PciTransport` on the
/// first machine, the MMIO transport on the second.
///
/// The crate's operations need `&mut` and the mount table holds its device by
/// `Arc`, so the lock is what turns the two into the `&self` methods
/// [`P9Device`] promises. The two of them are one implementation over the one
/// crate driver, which is the point: a second transport does not get a second
/// copy of them or of the protocol above, it gets its own `Transport`.
pub struct Virtio9p<T: Transport> {
    /// The device, behind a lock: one request may be in flight, and the
    /// crate's interface takes `&mut` for even the read-only parts.
    device: Mutex<RefCell<VirtIO9p<HalImpl, T>>>,
    mount_tag: String,
}

impl<T: Transport> Virtio9p<T> {
    /// Wrap the crate's driver as the mount layer's device, reading the tag once.
    ///
    /// The tag has to be read here rather than on `mount_tag`'s every call
    /// because the crate's accessor borrows the driver, which the lock owns from
    /// this point on.
    pub fn new(device: VirtIO9p<HalImpl, T>) -> Self {
        let mount_tag = String::from(device.mount_tag());
        Virtio9p {
            device: Mutex::new(RefCell::new(device)),
            mount_tag,
        }
    }
}

/// The device half of the seam `vfs/p9.rs` holds. The client names this contract
/// and never `virtio-pci`, so a machine reaching the same device over MMIO is a
/// second transport for these two methods rather than a second 9P client.
impl<T: Transport + Send> P9Device for Virtio9p<T> {
    fn mount_tag(&self) -> &str {
        &self.mount_tag
    }

    fn request(&self, req: &[u8], resp: &mut [u8]) -> bool {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .request(req, resp)
                .is_ok()
        })
    }
}

/// The devices the boot found, tags and all. Set by [`install`]; read by
/// [`devices`] and [`by_tag`], which the mount layer asks.
static DEVICES: Mutex<RefCell<Vec<Arc<dyn P9Device>>>> = Mutex::new(RefCell::new(Vec::new()));

/// Record every share a machine opened, and hand them back.
///
/// The one door in, and it takes the contract rather than a bus: the first
/// machine arrives through [`init_global`], the second with the devices its MMIO
/// windows opened, and everything above - the mount table, the boot's own mount,
/// `mount -t 9p` - is the same either way. Every device is kept rather than one,
/// because a host may export several shares and each is a filesystem waiting for
/// its tag to be named.
pub fn install(devices: Vec<Arc<dyn P9Device>>) -> Vec<Arc<dyn P9Device>> {
    critical_section::with(|cs| {
        DEVICES
            .borrow(cs)
            .borrow_mut()
            .extend(devices.iter().cloned());
    });
    devices
}

/// Probe the PCI bus and record every 9p device it finds, tag and all.
///
/// The first machine's door into [`install`]; a machine that reaches the same
/// device over an MMIO window opens it there and installs it the same way. There
/// is deliberately no device-level `init_global` returning one `Arc` the way the
/// card-shaped drivers have: a share is a filesystem, and a machine may have
/// several.
#[cfg(target_arch = "x86_64")]
pub fn init_global() -> Vec<Arc<dyn P9Device>> {
    let mut found: Vec<Arc<dyn P9Device>> = Vec::new();
    for transport in transport_all(DEVICE_9P) {
        if let Ok(device) = VirtIO9p::<HalImpl, _>::new(transport) {
            found.push(Arc::new(Virtio9p::new(device)));
        }
    }
    install(found)
}

/// Every 9p device on the bus, in probe order.
pub fn devices() -> Vec<Arc<dyn P9Device>> {
    critical_section::with(|cs| DEVICES.borrow(cs).borrow().clone())
}

/// The device exported under `tag`, or the only device there is when the
/// caller named no tag - the `mount(2)` the boot itself makes.
pub fn by_tag(tag: &str) -> Option<Arc<dyn P9Device>> {
    let devices = devices();
    if tag.is_empty() {
        return devices.into_iter().next();
    }
    devices.into_iter().find(|dev| dev.mount_tag() == tag)
}
