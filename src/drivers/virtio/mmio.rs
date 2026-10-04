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
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::blk::BlockDev;

use super::hal::HalImpl;

/// The virtio-blk reached over MMIO, shaped as a [`BlockDev`] so the filesystem
/// layer can mount it exactly as it mounts the PCI block device on x86_64 - the
/// same `Ext4Fs` driver, the same sector contract, only a different transport.
pub struct MmioBlk {
    device: Mutex<RefCell<VirtIOBlk<HalImpl, MmioTransport<'static>>>>,
    sectors: u64,
}

impl BlockDev for MmioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            // A read error means the transport did not complete the request and a
            // filesystem cannot proceed without the sector; the same answer the PCI
            // `BlockDev` gives, which is why the trait carries no Result.
            device
                .read_blocks((offset / SECTOR as u64) as usize, buf)
                .expect("virtio-blk over MMIO reported a read error");
        });
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device
                .write_blocks((offset / SECTOR as u64) as usize, buf)
                .expect("virtio-blk over MMIO reported a write error");
        });
    }

    fn flush(&self) -> bool {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device.flush().is_ok()
        })
    }
}

/// Open the virtio-blk window at `virt` as a [`BlockDev`], or `None` if it is not
/// a block device. The boot hands this to `Ext4Fs::mount` so a filesystem reads
/// through the memory-mapped transport end to end (`PLAN.md` §9.1's disk root).
pub fn blockdev(virt: usize, size: usize) -> Option<Arc<dyn BlockDev>> {
    // SAFETY: `virt` is inside the mapped, non-executable device window and `size`
    // is the tree-reported register block; the window outlives the handle.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
    let sectors = device.capacity();
    Some(Arc::new(MmioBlk {
        device: Mutex::new(RefCell::new(device)),
        sectors,
    }))
}

/// Receive/transmit queue depth and buffer size the net device is opened with -
/// the same shape the PCI driver uses, so the ring the crate posts is large
/// enough never to be what drops a frame. A boot check reads only the MAC, but
/// the device is constructed the way the stack would.
const NET_QUEUE_SIZE: usize = 64;
const NET_BUFFER_BYTES: usize = 2048;

/// Read the MAC of the virtio-net device whose register block is mapped at
/// `virt` (spanning `size`), or `None` if the window does not open as a network
/// device. This is the drivers stage's third MMIO device, pinned the same way as
/// the others: a real config read through the memory-mapped transport, not a
/// claim of traffic. It allocates the device's receive ring via the neutral
/// `Hal`, so it also re-proves DMA over MMIO for a multi-queue device.
pub fn read_mac(virt: usize, size: usize) -> Option<[u8; 6]> {
    // SAFETY: `virt` is an address inside the fixed device window the boot mapped
    // as Device-nGnRnE and never executable, `size` is the register block the tree
    // reports, and the header is volatile device memory; `'static` is the machine's
    // promise that the window outlives this borrow of it.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let mut net = VirtIONet::<HalImpl, _, NET_QUEUE_SIZE>::new(transport, NET_BUFFER_BYTES).ok()?;
    Some(net.mac_address())
}

/// Drive one frame through the transmit virtqueue of the virtio-net device whose
/// register block is mapped at `virt` (spanning `size`), and report whether the
/// device took the buffer.
///
/// This is the MMIO net *data path* - build a DMA-reachable transmit buffer,
/// place a frame in it, and kick it into the queue through the neutral `Hal` -
/// where [`read_mac`] only touched the device's configuration space. A `true`
/// says the multi-queue MMIO transport moved a buffer to the device over the
/// rings, the same class of proof the block milestone made by reading a real
/// sector back. It deliberately does **not** claim the frame reached any network
/// (that is the stack, which this machine has not been given yet); it reports
/// only that the device accepted the transmit. `false` is the honest refusal:
/// the window did not open as a net device, the queue had no free TX slot, or
/// the device rejected the buffer.
pub fn tx_probe(virt: usize, size: usize) -> bool {
    // SAFETY: `virt` is an address inside the fixed device window the boot mapped
    // as Device-nGnRnE and never executable, `size` is the register block the tree
    // reports, and the header is volatile device memory; `'static` is the machine's
    // promise that the window outlives this borrow of it.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let Ok(transport) = (unsafe { MmioTransport::new(header, size) }) else {
        return false;
    };
    let Ok(mut net) = (VirtIONet::<HalImpl, _, NET_QUEUE_SIZE>::new(transport, NET_BUFFER_BYTES))
    else {
        return false;
    };
    if !net.can_send() {
        return false;
    }
    // A bare minimum Ethernet frame (60 bytes) of zeros - the shape a real frame
    // has, not its content. The test is the queue, not the packet.
    let mut buffer = net.new_tx_buffer(60);
    buffer.packet_mut().copy_from_slice(&[0u8; 60]);
    net.send(buffer).is_ok()
}

/// Report whether the memory-mapped transport at `virt` (spanning `size`)
/// negotiates as a virtio-input device. This runs the same MMIO open + queue
/// setup + feature negotiation the rng/blk/net milestones use against a *fourth*
/// device class - so the aarch64 virtio transport is proven to reach input, not
/// just the three devices already driven. It deliberately claims nothing about
/// events arriving (that is the input stack, a later cut); `true` means only that
/// the device answered the input probe and opened its queues over MMIO.
pub fn probe_input(virt: usize, size: usize) -> bool {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    match unsafe { MmioTransport::new(header, size) } {
        Ok(transport) => VirtIOInput::<HalImpl, _>::new(transport).is_ok(),
        Err(_) => false,
    }
}

/// Read the first `out.len() / 512` sectors of the virtio-blk device whose
/// register block is mapped at `virt` (spanning `size`) into `out`.
///
/// `out`'s length must be a whole number of 512-byte sectors - that is the
/// device's contract, and a caller that hands a partial sector would ask the
/// crate to do something no block device does. `None` is an honest refusal: the
/// block did not open as a transport, or the read failed. This is the boot
/// milestone's probe (read a known sector back and compare), not a mounted
/// filesystem - that is the later disk-root step - so it leaves no global behind.
///
/// The crate shares the caller's buffer with the device, which is why `out` must
/// live in the direct map (a kernel stack buffer does); the caller passes that,
/// so no bounce is needed here.
pub fn read_blocks(virt: usize, size: usize, out: &mut [u8]) -> Option<()> {
    // SAFETY: `virt` is an address inside the fixed device window the boot mapped
    // whole as Device-nGnRnE and never executable (`device_virt` answers only for
    // a range wholly inside it), `size` is the register block the tree reports,
    // and the header is volatile device memory. `'static` is the machine's promise
    // that the window outlives this borrow of it.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let mut blk = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
    blk.read_blocks(0, out).ok()?;
    Some(())
}

/// A block, as the device counts them - the sector size `virtio-blk` fixes.
pub const SECTOR: usize = 512;

/// The block count the virtio-blk device at `virt` reports, or `None` if the
/// window does not open as a block device. This is the device's geometry - a
/// configuration-space read, distinct from the data read in [`read_blocks`] - so
/// asserting it against a known image proves the transport reads the device's
/// own capacity, not just its sectors.
pub fn capacity(virt: usize, size: usize) -> Option<u64> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window and `size` is the tree-reported register block.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let blk = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
    Some(blk.capacity())
}

/// Write `data` to sector `start`, flush, and read it back over MMIO, returning
/// `Some(())` only if what comes back equals what went in.
///
/// [`read_blocks`] and [`capacity`] prove the read and geometry paths; this proves
/// the *write* path - the request the device must copy our buffer out of memory,
/// land it on the image, and answer a flush. Round-tripping the same sector and
/// comparing is the honest evidence the MMIO write virtqueue moves data to the
/// device, the way the block read milestone proved the read path. `data` and
/// `out` must each be a whole number of sectors from the direct map (no bounce).
pub fn write_readback(virt: usize, size: usize, start: usize, data: &[u8]) -> Option<()> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let mut blk = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
    blk.write_blocks(start, data).ok()?;
    blk.flush().ok()?;
    // A fresh open for the read is the honest read-back: it goes through the
    // device, not a buffer we still hold.
    drop(blk);
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let mut blk = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
    let mut out = [0u8; SECTOR];
    if out.len() != data.len() {
        return None;
    }
    blk.read_blocks(start, &mut out).ok()?;
    (out == data).then_some(())
}

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
