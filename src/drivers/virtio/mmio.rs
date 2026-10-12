//! Virtio over the memory-mapped transport: the path to a device whose firmware
//! describes it as a window rather than as a PCI function.
//!
//! On `virt` the thirty-two `virtio,mmio` windows are fixed physical ranges the
//! device tree names and the boot maps into the kernel half. The `virtio-drivers`
//! crate drives both transports the same way once it is handed a mapped register
//! block: this module's job is only to turn a device-window address into a
//! transport and open a device on it, so the crate owns the queue mechanics
//! exactly as it does over PCI.
//!
//! Compiled on every machine, because which transport a device is behind is a
//! fact about the machine and not about the build: a machine whose firmware
//! describes itself with ACPI carries no tree, [`windows`] answers with no
//! windows at all, and its devices come up through the PCI front-ends instead.
//! What this module owns is the *walk* (`fdt`) and the register block; what it
//! never owns is the decision of which device the kernel wants - the probe
//! (`super::probe`) asks for a class by its virtio device ID, and a device is
//! handed on as the transport-neutral contract (`NetDevice`, `P9Device`, ...) so
//! nothing above this line learns which bus answered.
//!
//! The devices driven here are the ones a *stack* holds rather than a check
//! reads: the entropy source, the block device a filesystem mounts, the network
//! adapter the smoltcp stack sends and receives through, and the input device
//! the evdev registry publishes and pumps. Each gets the same two answers its
//! PCI counterpart has - a handle the caller owns, behind the usual kernel lock
//! - so what the probe opens and what the subsystem later uses are one device.
//!
//! The block and net devices are also published where the wiring layer can reach
//! them ([`blockdev`] is the disk the block layer registers, `net::install` the
//! adapter `net::init` is handed), because *which* window carries *which* device
//! is the machine's business and the subsystem above must not probe for it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;

use critical_section::Mutex;
use virtio_drivers::device::blk::VirtIOBlk;
use virtio_drivers::device::console::VirtIOConsole;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::device::rng::VirtIORng;
use virtio_drivers::device::socket::VirtIOSocket;
use virtio_drivers::device::sound::VirtIOSound;
use virtio_drivers::device::virtio_9p::VirtIO9p;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use crate::blk::BlockDev;

use super::console::VirtioHvc;
use super::fs9p::Virtio9p;
use super::gpu::VirtioGpuDev;
use super::gpu_ctrl::Gpu;
use super::hal::HalImpl;
use super::sound::{Probe, install_probed};
use super::vsock::VirtioVsock;
use super::{
    ConsolePort, EntropySource, InputDevice, NetDevice, P9Device, SoundCard, VirtioInputDev,
    VsockDevice,
};

/// The virtio-blk reached over MMIO, shaped as a [`BlockDev`] so the filesystem
/// layer can mount it exactly as it mounts the PCI block device on x86_64 - the
/// same `Ext4Fs` driver, the same sector contract, only a different transport.
pub struct MmioBlk {
    device: Mutex<RefCell<VirtIOBlk<HalImpl, MmioTransport<'static>>>>,
    sectors: u64,
}

/// Refuse a buffer this transport cannot be handed whole, naming it.
///
/// The PCI twin stages such a buffer through its own bounce, because the syscalls
/// there hand user pointers down; nothing on this machine reaches a block device
/// that way, so the honest answer is a named panic rather than a request to an
/// address the crate computes by subtraction. It is also the answer this
/// transport already gave, one layer down and with less to read: `mm::virt_to_phys`
/// asserts that the address it was given came out of the direct map, and this says
/// the same thing about the whole buffer instead of its first byte.
fn assert_reachable(buf: &[u8]) {
    assert!(
        super::device_reaches(buf),
        "virtio-blk over MMIO was handed a {}-byte buffer the device cannot reach as one run",
        buf.len()
    );
}

impl BlockDev for MmioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    /// The same width the PCI transport answers, and for the same reason: the
    /// crate's one-descriptor-per-buffer shape belongs to the crate and not to
    /// the bus, so the limit is one answer for both transports
    /// ([`super::BLK_MAX_TRANSFER`]).
    fn max_transfer(&self) -> usize {
        super::BLK_MAX_TRANSFER
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        assert_reachable(buf);
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
        assert_reachable(buf);
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
/// through the memory-mapped transport end to end (`NOTES.md` §9.1's disk root).
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
/// Open the virtio-net window at `virt` (spanning `size`) as a network adapter,
/// or `None` if the window does not open as one.
///
/// The handle that comes back is the live device: the same transport-neutral
/// contract the PCI front-end answers with, so the probe publishes it in one
/// place (`super::net::install`) whichever bus found it, and `net::init` is
/// handed it without learning which.
pub fn net(virt: usize, size: usize) -> Option<Arc<dyn NetDevice>> {
    Some(MmioNet::open(virt, size)?)
}

/// The device-specific configuration window inside a virtio-mmio register
/// block: everything from here up is the *device's* config, and below it is the
/// transport's. One constant, because a virtio-mmio window is one register
/// block - where PCI has to walk a function's capabilities to find the same
/// space (`NOTES.md` §13.26).
const DEVICE_CONFIG_OFFSET: usize = 0x100;

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's virtio-gpu card, or `None` if it does not open as one.
///
/// The caller gets the buffer view `/dev/fb0` reads through *and* the device,
/// which the render task owns - the same pair the first machine's PCI probe
/// hands back, so everything above this line is one copy. The configuration
/// window is recorded on the way, because the display's own events
/// (`VIRTIO_GPU_EVENT_DISPLAY`, a window resize) arrive there and nothing else
/// in the kernel can see one.
pub fn gpu_device(
    virt: usize,
    size: usize,
) -> Option<(Arc<VirtioGpuDev>, Gpu<MmioTransport<'static>>)> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow. The `'static` is the machine's promise, not a
    // borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = Gpu::new(transport).ok()?;
    VirtioGpuDev::install(device, Some(virt + DEVICE_CONFIG_OFFSET))
}

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's sound card, and answer it - or `None` if the window does not open
/// as one, or as a card that will play.
///
/// The same crate driver the first machine opens on its PCI bus, brought up
/// through the same `VirtioSnd::new`: the stream listing, the rate intersection,
/// the format check, the initial `SET_PARAMS`/`prepare`/`start` and the whole OSS
/// face above them are one copy (`NOTES.md` §13.27). `Probe::Absent` is a window
/// that is not a sound card at all, which is a quieter boot rather than a failed
/// one; the caller gets `None` for both and no line.
pub fn sound_device(virt: usize, size: usize) -> Option<Arc<dyn SoundCard>> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let Ok(transport) = (unsafe { MmioTransport::new(header, size) }) else {
        return None;
    };
    let opened = VirtIOSound::<HalImpl, _>::new(transport).map_err(|_| Probe::Init);
    install_probed(opened)
}

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's virtio console port, or `None` if it does not open as one.
///
/// The caller hands the result to `console::install`, and from there it is the
/// machine's `/dev/hvc0`: the devfs builder publishes it, the stream face reads
/// and writes through it. This is the same `VirtIOConsole` the first machine
/// opens on its PCI bus - only the transport underneath differs, which is the
/// whole of what this function adds (`NOTES.md` §13.25).
///
/// `None` is an honest refusal: a block the crate will not accept as a
/// transport, or a transport that is not a console device. A machine whose
/// window never opened then has no `/dev/hvc0` rather than a node whose reads
/// and writes go nowhere.
pub fn console_device(virt: usize, size: usize) -> Option<Arc<dyn ConsolePort>> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow. The `'static` is the machine's promise, not a
    // borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = VirtIOConsole::<HalImpl, _>::new(transport).ok()?;
    Some(Arc::new(VirtioHvc::new(device)))
}

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's virtio-vsock device, or `None` if it does not open as one.
///
/// The device is *handed over*, not probed: its caller gives it to
/// `net::vsock::init`, and from there it is the wire underneath `AF_VSOCK` - the
/// family's phases, its ports and its accept queue are `src/net/vsock.rs`'s, and
/// the pump that keeps its events moving is the shared one. This is the same
/// `VirtIOSocket` the first machine probes on its PCI bus and wraps in the same
/// [`VirtioVsock`] - only the transport underneath differs, which is the whole of
/// what this function adds (`NOTES.md` §13.28).
///
/// `None` is an honest refusal - a block the crate will not accept as a
/// transport, or a transport that is not a socket device - and a machine with no
/// window behind it leaves the family down, so `socket(AF_VSOCK)` answers the
/// `EAFNOSUPPORT` a family the kernel does not speak gives.
pub fn vsock_device(virt: usize, size: usize) -> Option<Arc<dyn VsockDevice>> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow. The `'static` is the machine's promise, not a
    // borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let driver = VirtIOSocket::<HalImpl, _>::new(transport).ok()?;
    Some(Arc::new(VirtioVsock::new(driver)))
}

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's virtio-9p device, or `None` if it does not open as one.
///
/// The device is *handed over*, not probed: the caller gives it to
/// `fs9p::install`, and from there it is a host share the mount table and
/// `mount -t 9p` can name - the fid table, the `Twalk`/`Tread` encoding and the
/// `Inode` mapping above it are `src/vfs/p9.rs`'s one copy (`NOTES.md` §13.29).
/// This is the same `VirtIO9p` the first machine probes on its PCI bus and wraps
/// in the same [`Virtio9p`], only the transport underneath differing.
///
/// `None` is an honest refusal - a block the crate will not accept as a
/// transport, or a transport that is not a 9p device. A window that is not one
/// is skipped by the caller, and a machine with no window has no share to mount
/// rather than a mount that answers nothing.
pub fn p9_device(virt: usize, size: usize) -> Option<Arc<dyn P9Device>> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow. The `'static` is the machine's promise, not a
    // borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = VirtIO9p::<HalImpl, _>::new(transport).ok()?;
    Some(Arc::new(Virtio9p::new(device)))
}

/// Open the memory-mapped transport at `virt` (spanning `size`) as this
/// machine's virtio-input device, or `None` if it does not open as one.
///
/// The device is *handed over*, not probed. Its caller gives it to
/// `input::register`, and from there it is the machine's keyboard: the evdev
/// registry holds it, the devfs build publishes `/dev/input/eventN` for it and
/// the service task drains its ring. The procedure the earlier milestone used -
/// open the window, negotiate, read the device's configuration-space identity,
/// drop it - still happens, but as `register`'s first act, because
/// `register` cannot build a node without the capability picture that read
/// produces. What was a proof that the transport reaches the class is now the
/// class itself (`NOTES.md` §15.2).
///
/// `None` is an honest refusal - a block the crate will not accept as a
/// transport, or a transport that is not an input device - and a machine whose
/// window never opened has an honestly empty `/dev/input` rather than a node
/// that opens onto nothing.
pub fn input_device(virt: usize, size: usize) -> Option<Arc<dyn InputDevice>> {
    // SAFETY: as in [`read_blocks`] - `virt` is inside the mapped, non-executable
    // device window, `size` is the tree-reported register block, and the window
    // outlives this borrow. The `'static` is the machine's promise, not a
    // borrow.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = VirtIOInput::<HalImpl, _>::new(transport).ok()?;
    Some(Arc::new(VirtioInputDev::new(device)))
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

/// Open the virtio-rng whose register block is mapped at kernel-virtual
/// address `virt` and spans `size` bytes, install it as the machine's entropy
/// source, and return it. `None` if the window does not open as one - a block
/// the crate will not accept as a transport, or a transport that is not an
/// entropy device - which leaves [`super::rng::entropy`] answering `None` too,
/// and `seed` treats that as a machine with no hardware source: the honest
/// skip, not a failure.
///
/// The same `VirtioRng<T>` the first machine builds over its PCI function;
/// only the transport underneath differs, and the contract above it
/// ([`super::EntropySource`]) is one copy (`NOTES.md` §15.2).
pub fn rng_device(virt: usize, size: usize) -> Option<Arc<dyn EntropySource>> {
    // SAFETY: `virt` is an address inside the fixed device window the boot
    // mapped whole as Device-nGnRnE and never executable (`device_virt` only
    // answers for a range wholly inside it), `size` is the register block the
    // tree reports, and the header is volatile device memory the transport
    // reads. The `'static` is the machine's promise, not a borrow: the window
    // outlives every use of this handle.
    let header = unsafe { NonNull::new_unchecked(virt as *mut VirtIOHeader) };
    let transport = unsafe { MmioTransport::new(header, size) }.ok()?;
    let device = VirtIORng::<HalImpl, _>::new(transport).ok()?;
    super::rng::install_probed(Some(device))
}

// ---------------------------------------------------------------------------
// Which windows carry which device: the walk over the record's device tree.
// ---------------------------------------------------------------------------

/// The four identity words a virtio-mmio transport answers before any driver has
/// touched it, at their fixed offsets in the register block.
///
/// They are the whole reason a machine can be asked *which* of its windows carry
/// a device: `virt` publishes thirty-two `virtio,mmio` nodes whether or not
/// anything is attached to them, and that is measured rather than assumed (the
/// tree decompiled with and without `-device virtio-rng-device` differs only in
/// the firmware's own random seeds, byte for byte). So the tree cannot say, and
/// the transport's own registers can.
pub const VIRTIO_MMIO_MAGIC: usize = 0x000;
/// `Version`: which register layout this transport speaks.
pub const VIRTIO_MMIO_VERSION: usize = 0x004;
/// `DeviceID`: zero when no device is attached to the transport.
pub const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
/// `VendorID`: whose device it is, which the specification leaves to the
/// implementation.
pub const VIRTIO_MMIO_VENDOR_ID: usize = 0x00c;

/// `MagicValue`: the four ASCII letters `virt`, read as one little-endian word.
pub const VIRTIO_MAGIC: u32 = 0x7472_6976;

/// `Version`: 2 is the modern (1.0) transport, which is the only register layout
/// the driver crate reads. Version 1 is the legacy one, whose registers are at
/// different offsets and which has no device-feature words at all.
pub const VIRTIO_VERSION_MODERN: u32 = 2;

/// One identity word of the transport mapped at `virt`.
///
/// # Safety
///
/// `virt` must be a window `hal::map_mmio_window` answered for - that is what
/// makes the read reachable device memory rather than an arbitrary address - and
/// `offset` one of the four words above.
pub unsafe fn read_identity(virt: usize, offset: usize) -> u32 {
    // SAFETY: the caller's contract. The kernel's own callers read only the four
    // words above, inside the 512-byte stride a virtio-mmio transport owns.
    unsafe { core::ptr::read_volatile((virt + offset) as *const u32) }
}

/// Every `virtio,mmio` window this record describes whose `DeviceID` is `want`,
/// as `(kernel-virtual header, size)`.
///
/// A machine whose firmware describes itself another way carries no device tree
/// and answers with no windows - which is the honest answer rather than a
/// failure, because such a machine reaches its devices over another transport.
/// A tree that will not walk is likewise no windows: what this function cannot
/// read it cannot claim, and a caller that finds nothing says so in its own line
/// rather than being handed a wrong window.
///
/// The order is the tree's, and callers that need *one* device of a class pick by
/// what the window answers to a driver probe rather than by position, because
/// QEMU assigns the MMIO slots in an order nothing can rely on.
pub fn windows(info: &mpos_bootinfo::BootInfo, want: u32) -> Vec<(usize, usize)> {
    let Some(bytes) = info.dtb else {
        return Vec::new();
    };
    let Ok(tree) = mpos_fdt::Fdt::new(bytes) else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    let _ = tree.each(
        |node| node.is_compatible("virtio,mmio"),
        |node| {
            for (address, size) in node.reg() {
                let (address, size) = (address as usize, size as usize);
                let Some(virt) = crate::hal::map_mmio_window(address, size) else {
                    continue;
                };
                // SAFETY: `map_mmio_window` answered for this range, which is the
                // machine saying the window is mapped device memory; the two
                // offsets are the identity words above.
                let (magic, id) = unsafe {
                    (
                        read_identity(virt, VIRTIO_MMIO_MAGIC),
                        read_identity(virt, VIRTIO_MMIO_DEVICE_ID),
                    )
                };
                if magic == VIRTIO_MAGIC && id == want {
                    windows.push((virt, size));
                }
            }
            true
        },
    );
    windows
}
