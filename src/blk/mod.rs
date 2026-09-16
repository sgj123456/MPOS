//! The block layer seam.
//!
//! [`BlockDev`] is all a filesystem gets to know about its storage: read and
//! write whole sectors at a byte offset. Everything above it - ext2 now, and
//! whatever filesystems follow - is written against this trait and unit-tested
//! on the host against an in-memory device; everything below it implements it
//! over real hardware (virtio-pci on x86_64, virtio-mmio on aarch64 later).
//!
//! The contract is deliberately narrow. Calls are synchronous; sector
//! boundaries are the caller's job; and a device error is a panic rather than
//! a return value, because on this kernel a failing disk means the run is over
//! - the milestone gate asserts on the panic message.

pub mod part;

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;

/// The disk the kernel booted from, as the block layer sees it. Set once at
/// boot; everything that needs to name the disk to userland - the devfs
/// builder above all - asks here rather than keeping its own copy.
static RAW: Mutex<RefCell<Option<Arc<dyn BlockDev>>>> = Mutex::new(RefCell::new(None));

/// Record the boot disk. A second call is ignored: the boot disk does not
/// change, and whichever caller won keeps the name.
pub fn set_raw_disk(dev: Arc<dyn BlockDev>) {
    critical_section::with(|cs| {
        let mut raw = RAW.borrow(cs).borrow_mut();
        if raw.is_none() {
            *raw = Some(dev);
        }
    });
}

/// The boot disk, if the block layer has one yet. The devfs builder asks
/// here for the names it publishes as `/dev/vda*`.
/// How many partitions the boot disk's table holds. The device names
/// (`/dev/vda1`...), their `/sys` numbering and the uevents that announce
/// them all count off this.
pub fn partition_count() -> usize {
    match raw_disk() {
        Some(raw) => part::partitions(raw.as_ref()).len(),
        None => 0,
    }
}

pub fn raw_disk() -> Option<Arc<dyn BlockDev>> {
    critical_section::with(|cs| RAW.borrow(cs).borrow().clone())
}

pub trait BlockDev: Send + Sync {
    /// Device capacity, in 512-byte sectors.
    fn capacity_sectors(&self) -> u64;

    /// Read whole sectors into `buf`. The offset and length are multiples of
    /// 512, and `buf` is at most one 4 KiB page - drivers bounce through a
    /// single page-sized, physically contiguous buffer.
    fn read_at(&self, offset: u64, buf: &mut [u8]);

    /// Write whole sectors from `buf`, under the same contract as [`read_at`].
    fn write_at(&self, offset: u64, buf: &[u8]);
}
