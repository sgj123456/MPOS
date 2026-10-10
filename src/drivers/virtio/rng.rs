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
//!
//! What the device is built *over* is the machine's and nothing above it
//! names one: [`VirtioRng<T>`] takes the crate's `VirtIORng` over whatever
//! transport the machine opened it on, and everything above - the lock, the
//! global, the entropy fold, the devfs node - is one copy reached through
//! the [`EntropySource`] contract (`NOTES.md` §15.2's rule, applied at last
//! to this class).

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::Transport;

use super::EntropySource;
use super::hal::HalImpl;
use super::hal::transport;
use crate::vfs::VfsResult;
use crate::vfs::devfs::{CharDevice, DevFs};

/// The transitional device ID; a modern-only device answers 0x1045 instead.
/// It names the PCI vendor's ID range, so it is read only by the first
/// machine's probe; the second finds the same device by its device-tree
/// `virtio,mmio` window (`super::mmio`).
const DEVICE_RNG: &[u16] = &[0x1005, 0x1045];

/// The entropy pipe over whatever transport the machine opened it on.
///
/// A request needs the crate's `&mut`, and the stream is shared with every
/// reader of `/dev/hwrng`, so the device sits behind the usual kernel lock -
/// the arrangement each transport had when it owned its own wrapper type,
/// stated once.
pub struct VirtioRng<T: Transport> {
    device: Mutex<RefCell<VirtIORng<HalImpl, T>>>,
}

impl<T: Transport> VirtioRng<T> {
    /// Wrap a device the machine's bus already opened.
    pub fn new(device: VirtIORng<HalImpl, T>) -> Self {
        VirtioRng {
            device: Mutex::new(RefCell::new(device)),
        }
    }
}

impl<T: Transport + Send> EntropySource for VirtioRng<T> {
    /// Fill `out` with bytes the host generated, answering how many landed -
    /// which for a healthy device is the whole buffer, and for a quiet one
    /// is fewer than that or none at all. The callers decide what a short
    /// answer means: the seed mixer skips its mix, and `/dev/hwrng` reports
    /// the count as the read it was.
    fn fill(&self, out: &mut [u8]) -> usize {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device.request_entropy(out).unwrap_or(0)
        })
    }
}

/// `/dev/hwrng`: the virtio entropy pipe, as a stream of raw bytes the host
/// generated. Reads ask the device and answer what came back - a refusal
/// reads as end of input, which is the honest shape for a machine whose
/// entropy source went quiet rather than a blocking wait for it. Writes
/// vanish, as they do on Linux.
///
/// The count is the device's own answer: how many bytes the host actually
/// generated, which for a healthy device is the whole buffer and for a quiet one
/// is a short read - the shape every stream answers with, and the one a caller
/// retrying a short read expects.
///
/// One node over [`EntropySource`], the contract that erased the transport:
/// this used to be two `#[cfg]`-defined copies of the same three lines, one
/// closing over the PCI handle and one over the MMIO handle (`NOTES.md` §15.2).
pub struct HwrngDev(Arc<dyn crate::drivers::virtio::EntropySource>);

impl CharDevice for HwrngDev {
    fn read(&self, buf: &mut [u8]) -> VfsResult<usize> {
        Ok(self.0.fill(buf))
    }

    fn write(&self, buf: &[u8]) -> usize {
        buf.len()
    }
}

/// Publish `/dev/hwrng` for a device set built after this driver came up.
///
/// Registered by [`install`] rather than named by `devices::standard_devfs`,
/// so what decides whether the node exists is the probe that opened the
/// device - the arrangement `devices::publish` states.
fn publish_nodes(dev: &DevFs) {
    if let Some(rng) = global() {
        dev.register("hwrng", Arc::new(HwrngDev(rng)));
    }
}

/// The device the boot found, if it found one. Set once by [`install`], read
/// by the devfs builder and by the random seed. The contract erases the
/// transport above this line, so one global serves either bus.
static GLOBAL: Mutex<RefCell<Option<Arc<dyn EntropySource>>>> = Mutex::new(RefCell::new(None));

/// Make `source` the machine's entropy source, and hand it back.
///
/// The one door in: the first machine arrives through [`init_global`], the
/// second with the device its MMIO window opened, and everything above -
/// `/dev/hwrng`, the seed mix, the boot's line - is the same either way.
pub fn install(source: Arc<dyn EntropySource>) -> Arc<dyn EntropySource> {
    critical_section::with(|cs| {
        *GLOBAL.borrow(cs).borrow_mut() = Some(source.clone());
    });
    crate::drivers::devices::publish("virtio-rng", publish_nodes);
    source
}

/// Bring up the device a machine opened and record it, or answer `None` for
/// a machine whose bus gave nothing.
pub fn install_probed<T: Transport + Send + 'static>(
    opened: Option<VirtIORng<HalImpl, T>>,
) -> Option<Arc<dyn EntropySource>> {
    opened.map(|device| install(Arc::new(VirtioRng::new(device))))
}

/// The first machine's door: probe the PCI bus for the device and bring it up.
pub fn init_global() -> Option<Arc<dyn EntropySource>> {
    let opened = transport(DEVICE_RNG).and_then(|t| VirtIORng::<HalImpl, _>::new(t).ok());
    install_probed(opened)
}

/// The device, if the machine has one.
pub fn global() -> Option<Arc<dyn EntropySource>> {
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
