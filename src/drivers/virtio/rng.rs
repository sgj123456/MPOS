//! virtio-rng, through the `virtio-drivers` crate.
//!
//! The device is an entropy pipe the host fills: QEMU backs it with the
//! host's own random source, so a request for bytes returns bytes no
//! software on this machine could have predicted. The crate owns the queue
//! mechanics; what is left here is which device to open, and the two places
//! the kernel wants the result.
//!
//! The first is the seed the kernel's ChaCha20 stream starts from
//! ([`rand`]'s, mixed in by `rand::seed`), which is why this device is
//! probed *before* `rand::init` runs rather than after it. The second is
//! `/dev/hwrng`, where a userland that wants the raw hardware stream can
//! read it - Linux keeps the two answers apart the same way, `/dev/hwrng`
//! for the device and `/dev/urandom` for the kernel's cooked stream over
//! everything it was ever told.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::pci::PciTransport;

use super::hal::{HalImpl, transport};

/// The transitional device ID; a modern-only device answers 0x1045 instead.
const DEVICE_RNG: &[u16] = &[0x1005, 0x1045];

pub struct VirtioRng {
    /// The device, behind a lock: a request needs the crate's `&mut`, and
    /// the entropy source is shared with every reader of `/dev/hwrng`.
    device: Mutex<RefCell<VirtIORng<HalImpl, PciTransport>>>,
}

impl VirtioRng {
    /// Fill `out` with bytes the host generated, answering how many
    /// landed - which for a healthy device is the whole buffer, and for a
    /// quiet one is fewer than that or none at all. The callers decide
    /// what a short answer means: the seed mixer skips its mix, and
    /// `/dev/hwrng` reports the count as the read it was.
    pub fn fill(&self, out: &mut [u8]) -> usize {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device.request_entropy(out).unwrap_or(0)
        })
    }

    /// Probe the PCI bus and bring up the first virtio-rng device, if any.
    pub fn init() -> Option<Arc<Self>> {
        let device = VirtIORng::<HalImpl, _>::new(transport(DEVICE_RNG)?).ok()?;
        Some(Arc::new(VirtioRng {
            device: Mutex::new(RefCell::new(device)),
        }))
    }
}

/// The device the boot found, if it found one. Set once by [`init_global`],
/// read by the devfs builder and by the random seed.
static GLOBAL: Mutex<RefCell<Option<Arc<VirtioRng>>>> = Mutex::new(RefCell::new(None));

/// Record the device the boot probed, for `/dev/hwrng` and the seed mix.
pub fn init_global() -> Option<Arc<VirtioRng>> {
    let dev = VirtioRng::init()?;
    critical_section::with(|cs| {
        let mut slot = GLOBAL.borrow(cs).borrow_mut();
        *slot = Some(dev.clone());
    });
    Some(dev)
}

/// The device, if the machine has one.
pub fn global() -> Option<Arc<VirtioRng>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// Thirty-two bytes of hardware entropy, or `None` on a machine without the
/// device - the seed mixer treats the absence exactly as a refusal. A
/// short answer is also `None`: a seed half-filled with the machine's own
/// bytes is what the mix beside it already covers.
pub fn entropy() -> Option<[u8; 32]> {
    let dev = global()?;
    let mut bytes = [0u8; 32];
    (dev.fill(&mut bytes) == bytes.len()).then_some(bytes)
}
