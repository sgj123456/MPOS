//! Virtio over the memory-mapped transport, the aarch64 machine's path.
//!
//! x86_64 reaches its virtio devices through a PCI BAR; `virt` has no PCI for
//! them - its thirty-two `virtio,mmio` windows are fixed physical ranges the
//! device tree names and the boot maps into the kernel half. The `virtio-drivers`
//! crate drives both the same way once it is handed a mapped register block: this
//! module's job is only to turn a device-window address into a transport and open
//! a device on it, so the crate owns the queue mechanics exactly as it does over
//! PCI.
//!
//! It is aarch64-only: `crate::drivers::virtio` gates this in where `rng`/`blk`/
//! `net` are gated to the PCI path (`#[cfg(target_arch = "x86_64")]`), and the
//! caller is a boot milestone check in `arch`, which owns reading the tree and
//! deciding which window carries which device. The driver is handed an address,
//! never a tree node - so it names neither `crate::boot` nor `crate::fdt`.
//!
//! The one device driven here today is the entropy source. It gets the same two
//! answers the PCI driver has: a global the boot installs once ([`init_global`])
//! so [`entropy`] can mix hardware bytes into `crate::rand`'s seed, and a
//! `request_entropy` the check reads straight through.

use alloc::sync::Arc;
use core::cell::RefCell;
use core::ptr::NonNull;

use critical_section::Mutex;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use super::hal::HalImpl;

/// The virtio-rng, reached over MMIO instead of a BAR.
///
/// A request needs the crate's `&mut`, so the device sits behind the usual
/// kernel lock; the entropy stream is shared with every reader (`crate::rand`'s
/// seed today, `/dev/hwrng` once the aarch64 devfs lands).
pub struct MmioRng {
    device: Mutex<RefCell<VirtIORng<HalImpl, MmioTransport<'static>>>>,
}

impl MmioRng {
    /// Open the entropy device whose register block is mapped at kernel-virtual
    /// address `virt` and spans `size` bytes.
    ///
    /// `virt` is what the caller got from the MMIO-mapping capability for a
    /// `virtio,mmio` window the tree reported; `size` is that window's register
    /// block. `None` is an honest refusal - a block the crate will not accept as
    /// a transport, or a transport that is not an entropy device.
    fn open(virt: usize, size: usize) -> Option<Self> {
        // SAFETY: `virt` is an address inside the fixed device window the boot
        // mapped whole as Device-nGnRnE and never executable (`device_virt` only
        // answers for a range wholly inside it), `size` is the register block the
        // tree reports, and the header is volatile device memory the transport
        // reads. The `'static` is the machine's promise, not a borrow: the window
        // outlives every use of this handle.
        let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
        let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
        let device = VirtIORng::<HalImpl, _>::new(transport).ok()?;
        Some(MmioRng {
            device: Mutex::new(RefCell::new(device)),
        })
    }

    /// Fill `out` with bytes the host generated, answering how many landed - the
    /// whole buffer for a healthy device, fewer or none for a quiet one.
    pub fn fill(&self, out: &mut [u8]) -> usize {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device.request_entropy(out).unwrap_or(0)
        })
    }
}

/// The device the boot found, if it found one. Set once by [`init_global`]; read
/// by the random seed (and, once the aarch64 devfs exists, `/dev/hwrng`).
static GLOBAL: Mutex<RefCell<Option<Arc<MmioRng>>>> = Mutex::new(RefCell::new(None));

/// Install the entropy device at the mapped window `[virt, virt + size)` as the
/// machine's random source, and return it. `None` if the window does not open as
/// a virtio-rng; a `None` leaves [`entropy`] answering `None` too, which `seed`
/// treats as a machine with no hardware source - the honest skip, not a failure.
pub fn init_global(virt: usize, size: usize) -> Option<Arc<MmioRng>> {
    let dev = Arc::new(MmioRng::open(virt, size)?);
    critical_section::with(|cs| {
        *GLOBAL.borrow(cs).borrow_mut() = Some(dev.clone());
    });
    Some(dev)
}

/// The device, if the machine has one.
pub fn global() -> Option<Arc<MmioRng>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// Thirty-two bytes of hardware entropy, or `None` on a machine without the
/// device - the seed mixer treats the absence exactly as a refusal. A short
/// answer is `None` too: a seed half-filled with the machine's own bytes is what
/// the clock mix beside it already covers.
pub fn entropy() -> Option<[u8; 32]> {
    let dev = global()?;
    let mut bytes = [0u8; 32];
    (dev.fill(&mut bytes) == bytes.len()).then_some(bytes)
}
