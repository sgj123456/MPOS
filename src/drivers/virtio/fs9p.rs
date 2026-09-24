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

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::virtio_9p::VirtIO9p;
use virtio_drivers::transport::pci::PciTransport;

use super::hal::{HalImpl, transport_all};

/// The transitional device ID; a modern-only device answers 0x1049 instead.
/// QEMU's `-virtfs` exports answer the legacy ID, carrying the modern
/// capabilities alongside the legacy registers - the same shape as the
/// block device this kernel already probes.
const DEVICE_9P: &[u16] = &[0x1009, 0x1049];

pub struct Virtio9p {
    /// The device, behind a lock: one request may be in flight, and the
    /// crate's interface takes `&mut` for even the read-only parts.
    device: Mutex<RefCell<VirtIO9p<HalImpl, PciTransport>>>,
    mount_tag: String,
}

impl Virtio9p {
    /// The tag the host exported this share under - the name `mount -t 9p`
    /// is pointed at, and the `aname` a client that wants the host to take
    /// it seriously attaches with.
    pub fn mount_tag(&self) -> &str {
        &self.mount_tag
    }

    /// One round trip: send `req`, wait, and return how many bytes of
    /// `resp` the answer filled. The caller sizes `resp` for the largest
    /// answer it can accept, which is the whole of the flow control this
    /// transport has.
    pub fn request(&self, req: &[u8], resp: &mut [u8]) -> bool {
        critical_section::with(|cs| {
            self.device
                .borrow(cs)
                .borrow_mut()
                .request(req, resp)
                .is_ok()
        })
    }

    /// Probe the PCI bus and open every 9p device it finds, tag and all.
    /// Every one is kept: a host may export several shares, and each is a
    /// filesystem waiting for its tag to be named.
    pub fn init_all() -> Vec<Arc<Virtio9p>> {
        let mut found = Vec::new();
        for transport in transport_all(DEVICE_9P) {
            if let Ok(device) = VirtIO9p::<HalImpl, _>::new(transport) {
                let mount_tag = String::from(device.mount_tag());
                found.push(Arc::new(Virtio9p {
                    device: Mutex::new(RefCell::new(device)),
                    mount_tag,
                }));
            }
        }
        found
    }
}

/// The devices the boot found, tags and all.
static DEVICES: Mutex<RefCell<Vec<Arc<Virtio9p>>>> = Mutex::new(RefCell::new(Vec::new()));

/// Probe the bus and record what answered. The mount layer asks for the
/// devices by tag; nothing else needs them, which is why there is no
/// device-level `init_global` returning one `Arc` the way the card-shaped
/// drivers have.
pub fn init_global() -> Vec<Arc<Virtio9p>> {
    let found = Virtio9p::init_all();
    critical_section::with(|cs| {
        DEVICES
            .borrow(cs)
            .borrow_mut()
            .extend(found.iter().cloned());
    });
    found
}

/// Every 9p device on the bus, in probe order.
pub fn devices() -> Vec<Arc<Virtio9p>> {
    critical_section::with(|cs| DEVICES.borrow(cs).borrow().clone())
}

/// The device exported under `tag`, or the only device there is when the
/// caller named no tag - the `mount(2)` the boot itself makes.
pub fn by_tag(tag: &str) -> Option<Arc<Virtio9p>> {
    let devices = devices();
    if tag.is_empty() {
        return devices.into_iter().next();
    }
    devices.into_iter().find(|dev| dev.mount_tag() == tag)
}
