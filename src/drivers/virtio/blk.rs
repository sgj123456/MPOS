//! virtio-blk over the legacy (pre-1.0) PCI interface.
//!
//! The legacy interface is a single I/O-port BAR holding the device header and
//! the block config, which needs no capability parsing and no MMIO mapping -
//! the whole driver is `in`/`out` plus two pages of DMA memory. QEMU presents
//! it when the device is created transitional, which is what mkiso asks for
//! (`-device virtio-blk-pci,disable-modern=on`).
//!
//! Requests are synchronous: the driver owns three descriptor slots (header,
//! data, status), kicks the queue, and spins on the used ring until the device
//! reports the buffer done. The spin runs with interrupts disabled, so on this
//! single core no other task can start a second request into the same slots;
//! INTx is disabled in the PCI command register so completion never raises an
//! interrupt we would have to service.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::blk::BlockDev;
use crate::drivers::pci;
use crate::mm::{self, PAGE_SIZE};

const VENDOR_VIRTIO: u16 = 0x1af4;
const DEVICE_BLK: u16 = 0x1001;

/// Legacy register offsets within BAR0.
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_NUM: u16 = 0x0c;
const REG_QUEUE_SEL: u16 = 0x0e;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
/// Block config: capacity in 512-byte sectors, little-endian u64.
const OFF_CAPACITY: u16 = 0x14;

const ST_ACKNOWLEDGE: u8 = 1;
const ST_DRIVER: u8 = 2;
const ST_DRIVER_OK: u8 = 4;

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

/// Request types: the device reads (T_IN) or writes (T_OUT) the data buffer.
const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_STATUS_OK: u8 = 0;

/// One request is a three-descriptor chain: header, data, status. Synchronous
/// operation means the same three slots serve every request.
const DESC_HEADER: usize = 0;
const DESC_DATA: usize = 1;
const DESC_STATUS: usize = 2;

/// Give up on a request that the device never completed, instead of hanging
/// the boot with interrupts off. QEMU answers in microseconds; this is orders
/// of magnitude past that.
const COMPLETION_SPINS: u64 = 100_000_000;

/// Legacy virtqueue descriptor: address, length, flags, next.
const DESC_SIZE: usize = 16;

pub struct VirtioBlk {
    io: u16,
    queue_len: u16,
    /// Physical address of the two-page virtqueue (desc + avail, then used).
    vring_phys: usize,
    /// One page of bounce memory: the only buffer the device ever DMAs into.
    bounce_phys: usize,
    /// One page holding the 16-byte request header and the 1-byte status.
    aux_phys: usize,
    avail_idx: AtomicU16,
    last_used: AtomicU16,
    sectors: u64,
}

/// Physical layout of one legacy virtqueue, reached through the direct map.
struct Vring {
    base: usize,
    len: u16,
    /// Offset of the used ring, which is page-aligned within the allocation.
    used_off: usize,
}

impl Vring {
    fn new(base: usize, len: u16) -> Vring {
        let avail_off = len as usize * DESC_SIZE;
        let used_off = mm::pframe::align_up(avail_off + 4 + 2 * len as usize, PAGE_SIZE);
        Vring {
            base,
            len,
            used_off,
        }
    }

    fn desc(&self, index: usize) -> usize {
        self.base + index * DESC_SIZE
    }

    fn avail(&self) -> usize {
        self.base + self.len as usize * DESC_SIZE
    }

    fn used(&self) -> usize {
        self.base + self.used_off
    }

    unsafe fn set_desc(&self, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.desc(index) as *mut u8;
        unsafe {
            (d as *mut u64).write_volatile(addr);
            (d.add(8) as *mut u32).write_volatile(len);
            (d.add(12) as *mut u16).write_volatile(flags);
            (d.add(14) as *mut u16).write_volatile(next);
        }
    }

    unsafe fn set_avail_entry(&self, slot: usize, head: u16) {
        let entry = (self.avail() + 4 + 2 * slot) as *mut u16;
        unsafe { entry.write_volatile(head) };
    }

    unsafe fn set_avail_idx(&self, value: u16) {
        let idx = (self.avail() + 2) as *mut u16;
        unsafe { idx.write_volatile(value) };
    }

    unsafe fn used_idx(&self) -> u16 {
        let idx = (self.used() + 2) as *const u16;
        unsafe { idx.read_volatile() }
    }

    /// The head descriptor index the device reports as used in `slot`.
    unsafe fn used_id(&self, slot: usize) -> u32 {
        let id = (self.used() + 4 + 8 * slot) as *const u32;
        unsafe { id.read_volatile() }
    }
}

impl BlockDev for VirtioBlk {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        self.transfer(BLK_T_IN, offset, buf.len());
        let bounce = mm::phys_to_virt(self.bounce_phys) as *const u8;
        let dst = buf.as_mut_ptr();
        unsafe { core::ptr::copy_nonoverlapping(bounce, dst, buf.len()) };
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        let bounce = mm::phys_to_virt(self.bounce_phys) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), bounce, buf.len()) };
        self.transfer(BLK_T_OUT, offset, buf.len());
    }
}

impl VirtioBlk {
    /// Issue one request and wait for it. `len` is already in the bounce
    /// buffer (writes) or will be copied out of it (reads) by the caller.
    fn transfer(&self, kind: u32, offset: u64, len: usize) {
        assert!(
            offset.is_multiple_of(512) && len.is_multiple_of(512) && len <= PAGE_SIZE,
            "virtio-blk request {offset:#x}+{len} breaks the sector contract"
        );
        assert!(
            offset / 512 < self.sectors,
            "virtio-blk request past end of disk"
        );

        // The whole request is one interrupts-off window: the three descriptor
        // slots and the ring indices are shared state, and on this core
        // disabling interrupts is the only exclusion there is.
        critical_section::with(|_| {
            let vring = Vring::new(mm::phys_to_virt(self.vring_phys), self.queue_len);
            let aux = mm::phys_to_virt(self.aux_phys) as *mut u8;

            unsafe {
                // virtio_blk_outhdr: type, ioprio, sector.
                (aux as *mut u32).write_volatile(kind);
                (aux.add(4) as *mut u32).write_volatile(0);
                (aux.add(8) as *mut u64).write_volatile(offset / 512);

                let data_flags = if kind == BLK_T_IN {
                    VRING_DESC_F_NEXT | VRING_DESC_F_WRITE
                } else {
                    VRING_DESC_F_NEXT
                };
                vring.set_desc(
                    DESC_HEADER,
                    self.aux_phys as u64,
                    16,
                    VRING_DESC_F_NEXT,
                    DESC_DATA as u16,
                );
                vring.set_desc(
                    DESC_DATA,
                    self.bounce_phys as u64,
                    len as u32,
                    data_flags,
                    DESC_STATUS as u16,
                );
                vring.set_desc(
                    DESC_STATUS,
                    self.aux_phys as u64 + 512,
                    1,
                    VRING_DESC_F_WRITE,
                    0,
                );

                let avail = self.avail_idx.load(Ordering::Relaxed);
                vring.set_avail_entry(avail as usize % self.queue_len as usize, DESC_HEADER as u16);
                // The index write must be visible to the device before the
                // kick; x86 orders stores, the fence orders the compiler.
                vring.set_avail_idx(avail.wrapping_add(1));
                core::sync::atomic::fence(Ordering::SeqCst);

                self.notify();

                let mut spins = 0;
                while vring.used_idx() == self.last_used.load(Ordering::Relaxed) {
                    spins += 1;
                    if spins == COMPLETION_SPINS {
                        panic!("virtio-blk request never completed");
                    }
                    core::hint::spin_loop();
                }

                let slot =
                    self.last_used.load(Ordering::Relaxed) as usize % self.queue_len as usize;
                assert_eq!(
                    vring.used_id(slot),
                    DESC_HEADER as u32,
                    "virtio-blk reported a buffer we did not submit"
                );
                self.last_used.store(
                    self.last_used.load(Ordering::Relaxed) + 1,
                    Ordering::Relaxed,
                );
                self.avail_idx
                    .store(avail.wrapping_add(1), Ordering::Relaxed);

                let status = aux.add(512).read_volatile();
                assert_eq!(
                    status, BLK_STATUS_OK,
                    "virtio-blk reported I/O error {status}"
                );
            }
        });
    }

    fn notify(&self) {
        unsafe {
            x86_64::instructions::port::Port::new(self.io + REG_QUEUE_NOTIFY).write(0u16);
        }
    }

    /// Probe the PCI bus and bring up the first virtio-blk device, if any.
    pub fn init() -> Option<Arc<dyn BlockDev>> {
        let func = pci::find(VENDOR_VIRTIO, DEVICE_BLK)?;
        let io = func.io_bar0().expect("virtio-blk BAR0 is not port I/O");
        // Decode I/O, allow DMA, and keep completion interrupts off: this
        // driver polls the used ring instead of servicing INTx.
        func.command_on(
            pci::COMMAND_IO_SPACE
                | pci::COMMAND_MEMORY_SPACE
                | pci::COMMAND_BUS_MASTER
                | pci::COMMAND_INTX_DISABLE,
        );

        let status = |value: u8| unsafe {
            x86_64::instructions::port::Port::new(io + REG_STATUS).write(value);
        };
        status(0);
        status(ST_ACKNOWLEDGE | ST_DRIVER);
        unsafe {
            x86_64::instructions::port::Port::new(io + REG_GUEST_FEATURES).write(0u32);
        }
        unsafe {
            x86_64::instructions::port::Port::new(io + REG_QUEUE_SEL).write(0u16);
        }
        let queue_len: u16 =
            unsafe { x86_64::instructions::port::Port::new(io + REG_QUEUE_NUM).read() };
        assert!(queue_len > 0, "virtio-blk offered an empty queue");

        // Desc + avail live in the first page region, used starts on the next
        // page boundary; order the allocation to fit both.
        let vring = Vring::new(0, queue_len); // offsets only; base filled below
        let pages = (vring.used_off + 4 + 8 * queue_len as usize).div_ceil(PAGE_SIZE);
        let order = pages.next_power_of_two().trailing_zeros() as usize;
        let vring_phys = mm::alloc_frames(order).expect("out of frames for the virtqueue");
        let bounce_phys = mm::alloc_frames(0).expect("out of frames for the DMA bounce page");
        let aux_phys = mm::alloc_frames(0).expect("out of frames for the request header page");
        unsafe {
            core::ptr::write_bytes(
                mm::phys_to_virt(vring_phys) as *mut u8,
                0,
                PAGE_SIZE << order,
            );
        }

        unsafe {
            x86_64::instructions::port::Port::new(io + REG_QUEUE_PFN)
                .write((vring_phys >> 12) as u32);
        }
        status(ST_ACKNOWLEDGE | ST_DRIVER | ST_DRIVER_OK);

        let lo: u32 = unsafe { x86_64::instructions::port::Port::new(io + OFF_CAPACITY).read() };
        let hi: u32 =
            unsafe { x86_64::instructions::port::Port::new(io + OFF_CAPACITY + 4).read() };
        let sectors = (hi as u64) << 32 | lo as u64;
        assert!(sectors > 0, "virtio-blk reports a zero-capacity disk");

        Some(Arc::new(VirtioBlk {
            io,
            queue_len,
            vring_phys,
            bounce_phys,
            aux_phys,
            avail_idx: AtomicU16::new(0),
            last_used: AtomicU16::new(0),
            sectors,
        }))
    }
}
