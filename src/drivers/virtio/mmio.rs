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
//! The devices driven here are the ones a *stack* holds rather than a check
//! reads: the entropy source, the block device a filesystem mounts, and the
//! network adapter the smoltcp stack sends and receives through. Each gets the
//! same two answers its PCI counterpart has - a handle the caller owns, behind
//! the usual kernel lock - and the checks drive them through those handles, so
//! what a milestone proves and what the subsystem later uses are one device.
//!
//! The block and net devices are also published where the wiring layer can reach
//! them ([`blockdev`] is handed to `Ext4Fs::mount` by the disk-root step,
//! [`net_device`] is handed to `net::init`), because *which* window carries
//! *which* device is the machine's business and the subsystem above must not
//! probe for it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use critical_section::Mutex;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::blk::BlockDev;

use super::NetDevice;
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
/// enough never to be what drops a frame.
const NET_QUEUE_SIZE: usize = 64;
const NET_BUFFER_BYTES: usize = 2048;

/// The virtio-net reached over MMIO, shaped as a [`NetDevice`] so the network
/// stack drives it exactly as it drives the PCI adapter on x86_64: the same
/// smoltcp interface above, the same `poll`/`transmit`/`ack_interrupt` seam, only
/// a different transport underneath.
///
/// This is the *live* device rather than a probe, and the difference is that it
/// stays open. Opening the transport posts the receive ring - sixty-four 2 KiB
/// buffers, allocated through the neutral `Hal` - so a helper that opened the
/// window, read one thing and dropped it would throw that ring away, and a stack
/// that did so between polls would lose every frame that arrived in between. The
/// machine therefore opens the adapter once, at the check that drives its data
/// path ([`init_net_global`]), and hands that same handle to `net::init`.
pub struct MmioNet {
    device: Mutex<RefCell<VirtIONet<HalImpl, MmioTransport<'static>, NET_QUEUE_SIZE>>>,
    mac: [u8; 6],
}

impl MmioNet {
    /// Open the network adapter whose register block is mapped at kernel-virtual
    /// address `virt` and spans `size` bytes.
    ///
    /// `virt` is what the caller got from the MMIO-mapping capability for a
    /// `virtio,mmio` window the tree reported, and `size` is that window's
    /// register block. `None` is an honest refusal - a block the crate will not
    /// accept as a transport, or a transport that is not a network device.
    pub fn open(virt: usize, size: usize) -> Option<Arc<MmioNet>> {
        // SAFETY: `virt` is an address inside the fixed device window the boot
        // mapped whole as Device-nGnRnE and never executable (`device_virt` only
        // answers for a range wholly inside it), `size` is the register block the
        // tree reports, and the header is volatile device memory the transport
        // reads. The `'static` is the machine's promise, not a borrow: the window
        // outlives every use of this handle.
        let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
        let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
        let device =
            VirtIONet::<HalImpl, _, NET_QUEUE_SIZE>::new(transport, NET_BUFFER_BYTES).ok()?;
        let mac = device.mac_address();
        Some(Arc::new(MmioNet {
            device: Mutex::new(RefCell::new(device)),
            mac,
        }))
    }
}

impl NetDevice for MmioNet {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn rx_frames(&self) -> usize {
        NET_QUEUE_SIZE
    }

    /// Take one frame the device has received, if there is one.
    ///
    /// The buffer goes straight back to the device: a buffer the driver kept is a
    /// buffer the device cannot fill, and the queue would shrink by one every time
    /// a frame arrived.
    fn poll(&self) -> Option<Vec<u8>> {
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

    /// Hand one frame to the transmit queue, answering whether the device took it.
    ///
    /// The data path, not the configuration space: building a DMA-reachable buffer,
    /// placing a frame in it and kicking it into the queue through the neutral `Hal`
    /// is what proves the multi-queue MMIO transport moves buffers over the rings -
    /// the same class of proof the block milestone made by reading a real sector
    /// back. It deliberately does **not** claim the frame reached any network (that
    /// is the stack above, and the programs that drive it); `false` is an honest
    /// refusal - a full or absent queue, so the frame is gone as it would be on the
    /// wire.
    fn transmit(&self, frame: &[u8]) -> bool {
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

    /// Read and clear the device's interrupt status.
    ///
    /// This machine drives the adapter by polling (the service task's tick), and
    /// nothing routes its GIC line yet, so this is the same no-op-in-practice the
    /// PCI driver performs when a frame arrived without an interrupt: the status
    /// register is read and cleared so the next arrival can raise the line. It is
    /// here rather than absent because `net` calls it on the seam, and a machine
    /// whose device is polled only answers it honestly.
    fn ack_interrupt(&self) {
        critical_section::with(|cs| {
            let _ = self.device.borrow(cs).borrow_mut().ack_interrupt();
        });
    }
}

/// The adapter the boot opened, if it opened one. Set once by
/// [`init_net_global`]; read by [`net_device`], which `main`'s net wiring hands to
/// `net::init`.
static NET_GLOBAL: Mutex<RefCell<Option<Arc<MmioNet>>>> = Mutex::new(RefCell::new(None));

/// Open the virtio-net window at `virt` (spanning `size`) as the machine's network
/// adapter, and return it - or `None` if the window does not open as one.
///
/// This is the check's call, and the handle it returns is the *live* device: the
/// same object `net::init` will be given through [`net_device`]. The boot check
/// drives its data path (a transmit) while holding this handle, so the frame the
/// marker reports and the frames the stack later sends go through one open ring
/// and one device state, not two.
///
/// A machine whose card never probed leaves this `None`, and `net_device` answers
/// `None` with it - which is the honest `ENETDOWN` the stack reports rather than a
/// device that pretends to answer.
pub fn init_net_global(virt: usize, size: usize) -> Option<Arc<MmioNet>> {
    let dev = MmioNet::open(virt, size)?;
    critical_section::with(|cs| {
        *NET_GLOBAL.borrow(cs).borrow_mut() = Some(dev.clone());
    });
    Some(dev)
}

/// The machine's network adapter, if it has one - what `main`'s net wiring passes
/// to `net::init`.
pub fn net_device() -> Option<Arc<dyn NetDevice>> {
    let dev = critical_section::with(|cs| NET_GLOBAL.borrow(cs).borrow().clone())?;
    Some(dev)
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
    let Ok(transport) = (unsafe { MmioTransport::new(header, size) }) else {
        return false;
    };
    let Ok(mut net) = (VirtIOInput::<HalImpl, _>::new(transport)) else {
        return false;
    };
    // Negotiated. Now read the device's configuration-space identity (`ids()`
    // reads the fixed-size virtio-input `bustype/vendor/product/version`). That is
    // a data read across the mapped transport - the same class of proof the net
    // milestone made by reading the MAC - so the milestone says the MMIO path
    // *moves config bytes* for this device class, not merely opens it.
    net.ids().is_ok()
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
