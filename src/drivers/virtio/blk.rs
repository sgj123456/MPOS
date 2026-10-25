//! virtio-blk, through the `virtio-drivers` crate.
//!
//! This used to be a hand-written legacy driver: an I/O-port BAR, three
//! descriptors per request, and a spin on the used ring. It is not any more -
//! the crate brings its own VirtIO 1.0 implementation, with the queue
//! machinery, the feature negotiation and the configuration parsing that a
//! driver has to get right before a single sector can move.
//!
//! What is left here is the part that could not be replaced: which device this
//! kernel talks to, and [`BlockDev`], the interface the rest of the kernel
//! reaches it through. Requests still go one at a time - the crate could queue
//! more, nothing above this asks, and a filesystem that sees a synchronous
//! device cannot tell the difference.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::blk::{BlkReq, BlkResp, VirtIOBlk};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::DeviceFunction;

use crate::blk::BlockDev;
use crate::drivers::pci;

use super::blk_irq;
use super::hal::{HalImpl, pci_root};

const VENDOR_VIRTIO: u16 = 0x1af4;
/// QEMU hands us the transitional device, which answers the legacy id and
/// carries the modern capabilities alongside the legacy registers - and it is
/// the capabilities the transport below reads.
const DEVICE_BLK: u16 = 0x1001;

/// A block, as the device counts them.
const SECTOR: u64 = 512;

/// Sectors a bounce buffer holds. Four of them - 2 KiB - which is what the
/// probing reads that land here actually ask for, and small enough to sit on a
/// 16 KiB kernel stack without a thought.
const BOUNCE_SECTORS: usize = 4;

/// The one thing read and write disagree on - who owns the buffer's
/// mutability - carried with the buffer, so the transfer path holds both
/// directions without casting away a `const` (which is undefined behavior,
/// not a shortcut).
enum Xfer<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}
/// Whether a completion interrupt was armed at bring-up.
static IRQ_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// How long a parked transfer waits for the wire before polling the ring
/// once: four milliseconds, one tick — a lost line, not a slow device. Named
/// against the clock the park answers (§6.24's rule), the same figure and
/// reason the MMIO driver's wait carries.
const WAIT_NS: u64 = 4_000_000;

pub struct VirtioBlk {
    /// The device, behind a lock: a request needs the crate's `&mut`, and
    /// [`BlockDev`] hands out `&self` because a filesystem takes the whole
    /// device and shares it with everyone it mounts it for.
    device: Mutex<RefCell<VirtIOBlk<HalImpl, PciTransport>>>,
    sectors: u64,
}

impl VirtioBlk {
    /// Whether the device can be handed the whole of `buf` as one descriptor.
    ///
    /// [`super::device_reaches`] is the walk; this is the driver asking it. What
    /// it replaced was `ptr >= mm::hhdm_offset()`, and that is a *necessary*
    /// condition rather than a sufficient one: an MMIO window or the kernel
    /// image is mapped in the kernel half and above the offset while *not* being
    /// at it, so `share`'s subtraction would have sent the device to an address
    /// computed from the wrong thing. One page of that class of mistake is a
    /// page; a 128 KiB run of it is a disk's worth of wrong bytes, which is why
    /// the answer is the page tables' and not the pointer's.
    fn reaches(buf: &[u8]) -> bool {
        super::device_reaches(buf)
    }

    /// Move `buf.len()` bytes at `offset` through the device, in pieces no
    /// larger than the bounce buffer, for a caller whose buffer it cannot be
    /// handed.
    ///
    /// The bounce is a kernel stack array, and that is what makes it a legal
    /// transfer at all: the stack is frames the direct map reaches, and one run
    /// of them for the same reason every other kernel buffer is (see
    /// [`super::device_reaches`]). The syscalls are the callers this exists for:
    /// `read`, `readv` and `pread64` pass the caller's pointer straight down, and
    /// a block *device* is read with no filesystem in between to break the fall -
    /// and the 2 KiB piece is small enough to sit on a 16 KiB kernel stack
    /// without a thought.
    fn read_at_bounced(&self, offset: u64, buf: &mut [u8]) {
        let mut bounce = [0u8; SECTOR as usize * BOUNCE_SECTORS];
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(bounce.len());
            self.read_dma(offset + done as u64, &mut bounce[..chunk]);
            buf[done..done + chunk].copy_from_slice(&bounce[..chunk]);
            done += chunk;
        }
    }

    /// The other direction of [`Self::read_at_bounced`], for the same callers and
    /// with the same bounce.
    fn write_at_bounced(&self, offset: u64, buf: &[u8]) {
        let mut bounce = [0u8; SECTOR as usize * BOUNCE_SECTORS];
        let mut done = 0;
        while done < buf.len() {
            let chunk = (buf.len() - done).min(bounce.len());
            bounce[..chunk].copy_from_slice(&buf[done..done + chunk]);
            self.write_dma(offset + done as u64, &bounce[..chunk]);
            done += chunk;
        }
    }

    fn read_dma(&self, offset: u64, buf: &mut [u8]) {
        self.transfer((offset / SECTOR) as usize, Xfer::Read(buf));
    }

    fn write_dma(&self, offset: u64, buf: &[u8]) {
        self.transfer((offset / SECTOR) as usize, Xfer::Write(buf));
    }

    /// One transfer: claim the device, submit non-blocking, and **park the
    /// task** until the completion interrupt answers.
    ///
    /// The two earlier shapes are this file's history: the crate's synchronous
    /// call spun masked for the device's whole latency (§10.5's mask), and the
    /// first interrupt-driven cut waited by polling unmasked — because the
    /// block layer's callers held the ext4 cache's borrow across the call, and
    /// a parked task let a second walk into it (`RefCell already borrowed`,
    /// measured). The cache parks holding nothing now, so the wait is the
    /// scheduler's again: a parked transfer costs the machine nothing but the
    /// descriptors, and every other task runs for the device's latency.
    ///
    /// The park is [`block_until_uninterruptible`]: the request and response
    /// records sit on this frame and the device's descriptors name them, so a
    /// fatal signal must not end the wait from inside it. A deadline answers
    /// only a *lost wire* — one poll of the used ring per period, the old
    /// shape's recovery, not a slow device's clock. Boot context has no task
    /// to park, and a transport without a line has nothing to wait for: both
    /// keep the synchronous call.
    fn transfer(&self, block: usize, mut xfer: Xfer<'_>) {
        let parked =
            IRQ_ARMED.load(core::sync::atomic::Ordering::Acquire) && crate::sched::started();
        if !parked {
            self.transfer_sync(block, xfer);
            return;
        }
        let me = crate::sched::current();

        // One transfer in flight per device: the mailbox is one flag and one
        // waiter, and a submission under a parked one would clear the flag the
        // first is sleeping on. The loser of the claim parks on the claim
        // itself; `finish_request` wakes the queue when the transfer retires.
        while !blk_irq::begin_request(me) {
            if blk_irq::wait_for_device(me) {
                crate::sched::block_until_uninterruptible(|| !blk_irq::busy());
            }
        }

        let mut req = BlkReq::default();
        let mut resp = BlkResp::default();
        // Submit under the device lock and the critical section, and leave
        // both before waiting: the interrupt that answers never needs the
        // device, so a parked reader holding neither is what lets the handler
        // run at all. The mailbox was cleared with the claim.
        let token = critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            // SAFETY: the buffers are borrowed by the device from here until
            // the matching `complete_*` below; this task parks inside the call
            // holding neither reference, which is the crate's own contract.
            let result = match &mut xfer {
                Xfer::Read(buf) => unsafe {
                    device.read_blocks_nb(block, &mut req, buf, &mut resp)
                },
                Xfer::Write(buf) => unsafe {
                    device.write_blocks_nb(block, &mut req, buf, &mut resp)
                },
            };
            result.ok()
        });
        let Some(token) = token else {
            // The ring was full: nothing is in flight, so the synchronous call
            // cannot collide with a parked one. It retires the request by
            // polling, the way this driver always did, and only then is the
            // claim worth releasing.
            self.transfer_sync(block, xfer);
            blk_irq::finish_request();
            return;
        };

        // The wait: parked, uninterruptible, ended by the completion
        // interrupt — with one poll per period for a wire that lost the wake.
        let mut deadline = crate::hal::since_boot_nanos() + WAIT_NS;
        while !blk_irq::completed() {
            crate::sched::block_until_uninterruptible(|| {
                blk_irq::completed() || crate::hal::since_boot_nanos() >= deadline
            });
            if blk_irq::completed() {
                break;
            }
            let done = critical_section::with(|cs| {
                let mut device = self.device.borrow(cs).borrow_mut();
                if device.peek_used() == Some(token) {
                    // The wire lost the wake but the work is done: mark the
                    // mailbox and finish now.
                    blk_irq::complete_irq();
                    true
                } else {
                    false
                }
            });
            if done {
                break;
            }
            deadline = crate::hal::since_boot_nanos() + WAIT_NS;
        }

        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            while device.peek_used() != Some(token) {
                core::hint::spin_loop();
            }
            // SAFETY: the buffers were borrowed by the device at submit and
            // have not been touched since - this task parked inside the call -
            // and the token is this request's own.
            let done = match xfer {
                Xfer::Read(buf) => unsafe {
                    device
                        .complete_read_blocks(token, &req, buf, &mut resp)
                        .is_ok()
                },
                Xfer::Write(buf) => unsafe {
                    device
                        .complete_write_blocks(token, &req, buf, &mut resp)
                        .is_ok()
                },
            };
            if !done {
                panic!("virtio-blk reported a completion error");
            }
        });
        blk_irq::finish_request();
    }

    /// The synchronous transfer, exactly as this driver shipped: one masked
    /// spin until the ring answers. Kept for the two callers that cannot park
    /// (boot context, an unarmed line) and for the ring-full fallback.
    fn transfer_sync(&self, block: usize, xfer: Xfer<'_>) {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            let result = match xfer {
                Xfer::Read(buf) => device.read_blocks(block, buf),
                Xfer::Write(buf) => device.write_blocks(block, buf),
            };
            result.expect("virtio-blk reported a transfer error");
        });
    }
}

impl BlockDev for VirtioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    /// The width the two virtio transports answer, and why the crate's
    /// one-descriptor-per-buffer shape does not narrow it.
    ///
    /// A descriptor carries a single physical address, and `HalImpl::share`
    /// answers the crate with `virt_to_phys` - a subtraction against the direct
    /// map. So what a wide buffer needs is not contiguity in the abstract but
    /// membership of the direct map, and a kernel buffer is in it by
    /// construction: the direct map is affine, so a range of it is one physical
    /// run, and the heap hands every allocation past the slab's largest class out
    /// as a contiguous frame block (`mm::heap`'s `alloc_pages`) - which is what
    /// `vfs/ext4/cache.rs`'s run scratch is. §10.5 first named a driver-owned
    /// *contiguous bounce* as the prerequisite for this disk; the scratch and the
    /// walk [`Self::reaches`] makes showed the prerequisite was already met, and a
    /// bounce would have copied every read of the busiest disk for nothing.
    fn max_transfer(&self) -> usize {
        super::BLK_MAX_TRANSFER
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        assert!(
            offset.is_multiple_of(SECTOR) && buf.len().is_multiple_of(SECTOR as usize),
            "virtio-blk read {offset:#x}+{} breaks the sector contract",
            buf.len()
        );
        // The device answers an out-of-range request with a bare I/O error
        // that says nothing about which part of it was wrong. Refuse it here,
        // where the window is still visible.
        let first = offset / SECTOR;
        let count = buf.len() as u64 / SECTOR;
        assert!(
            first + count <= self.sectors,
            "virtio-blk read of {count} sectors at {first} runs past the {}-sector device",
            self.sectors
        );
        if !Self::reaches(buf) {
            self.read_at_bounced(offset, buf);
            return;
        }
        self.read_dma(offset, buf);
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        assert!(
            offset.is_multiple_of(SECTOR) && buf.len().is_multiple_of(SECTOR as usize),
            "virtio-blk write {offset:#x}+{} breaks the sector contract",
            buf.len()
        );
        if !Self::reaches(buf) {
            self.write_at_bounced(offset, buf);
            return;
        }
        self.write_dma(offset, buf);
    }

    fn flush(&self) -> bool {
        // The Flush request carries no data; an error answer (or a device
        // that never advertised VIRTIO_BLK_F_FLUSH) says the transport
        // promises no ordering beyond what it already gave the writes.
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            device.flush().is_ok()
        })
    }
}

impl VirtioBlk {
    /// Probe the PCI bus and bring up the first virtio-blk device, if any.
    pub fn init() -> Option<Arc<dyn BlockDev>> {
        let func = pci::find(VENDOR_VIRTIO, DEVICE_BLK)?;
        // Decode memory and let the device reach main memory. The transport
        // talks to the device through an MMIO BAR, so memory decoding is not
        // optional the way it was for the port-based driver this replaced.
        func.command_on(
            pci::COMMAND_IO_SPACE
                | pci::COMMAND_MEMORY_SPACE
                | pci::COMMAND_BUS_MASTER
                | pci::COMMAND_INTX_DISABLE,
        );

        let (bus, device, function) = func.bdf();
        let mut root = pci_root();
        let transport = PciTransport::new::<HalImpl, _>(
            &mut root,
            DeviceFunction {
                bus,
                device,
                function,
            },
        )
        .ok()?;

        let device = VirtIOBlk::<HalImpl, _>::new(transport).ok()?;
        let sectors = device.capacity();
        assert!(sectors > 0, "virtio-blk reports a zero-capacity disk");

        // The device is this driver's from here, which is what `lspci -k`
        // reads back. Recorded after the transport and the device both came
        // up, so a claim means a driver really bound rather than merely
        // matched.
        pci::claim(&func, super::PCI_DRIVER);

        // The completion interrupt: MSI-X entry 0 pointed at the block
        // vector, every queue bound to it - the same two steps the network
        // adapter runs (`msix`), and the same failure shape: `false` from
        // either leaves the flag down and this driver keeps the synchronous
        // call, which is what it shipped with. The command register above
        // disabled INTX on purpose - a line this driver never asked for - so
        // MSI-X is the only wire a completion could travel on.
        #[cfg(mpos_msi)]
        let armed = {
            crate::hal::init_message_signalled();
            super::msix::enable(&func, crate::hal::blk_msi_vector())
                && super::msix::set_queue_vectors(&func)
        };
        #[cfg(not(mpos_msi))]
        let armed = false;
        IRQ_ARMED.store(armed, core::sync::atomic::Ordering::Release);
        if armed {
            blk_irq::arm();
        }

        Some(Arc::new(VirtioBlk {
            device: Mutex::new(RefCell::new(device)),
            sectors,
        }))
    }
}
