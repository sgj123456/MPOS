#![no_std]

//! The block layer seam.
//!
//! This is its own leaf crate: the whole of it is a *contract*, and a contract
//! is worth a host test. [`BlockDev`] is all a filesystem gets to know about
//! its storage - read and write whole sectors at a byte offset - and everything
//! above it (ext2 now, and whatever filesystems follow) is written against the
//! trait and unit-tested on the host against an in-memory device, while
//! everything below it implements it over real hardware (virtio-pci on x86_64,
//! virtio-mmio on aarch64 later). The crate also holds the two facts that sit
//! at this seam rather than in any driver: the registry that names every disk
//! the kernel has, and the GPT/MBR decode that finds a filesystem on one
//! ([`part`]). As a leaf it cannot name the kernel, so the downward-only rule
//! is a compile error rather than a convention.
//!
//! The contract is deliberately narrow. Calls are synchronous; sector
//! boundaries are the caller's job; and a device error is a panic rather than
//! a return value, because on this kernel a failing disk means the run is over
//! - the milestone gate asserts on the panic message.

extern crate alloc;

pub mod part;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

/// The largest buffer [`BlockDev::read_at`]/[`BlockDev::write_at`] is handed in
/// one call: the trait's contract says a driver bounces through a single
/// page-sized, physically contiguous buffer, so this is the machine's 4 KiB
/// page as the block layer's *own* transfer bound.
///
/// It is written here rather than read from `mm::PAGE_SIZE` because this crate
/// is a leaf that cannot name the kernel - and because the bound belongs to the
/// trait that states it, not to the frame allocator that happens to size the
/// same hardware fact.
pub const PAGE_SIZE: usize = 4096;

/// One disk the kernel has, named the way userland will name it.
///
/// Everything here except the device itself is a *name*. The kernel's disks are
/// told apart by userland through `/dev`, `/proc/partitions` and
/// `/sys/class/block`, and all three agree on which of them is `vda` because
/// this record says so once.
pub struct Disk {
    /// The name under `/dev`: `vda`, `sda`.
    pub name: String,
    /// The major this name is numbered under - Linux's `VIRTIO_BLK_MAJOR` (254)
    /// for a virtio disk, `SCSI_DISK_MAJOR` (8) for a SATA one. A major is what
    /// tells two drivers' disks apart, so it is a fact about the *driver* and
    /// travels with the disk rather than being derived from the name.
    pub major: u32,
    /// The device-tree prefix this disk and its partitions hang under:
    /// `/devices/virtio/block`, `/devices/ahci/block`. Also a fact about the
    /// driver - a name does not say where its device came from, and a kernel
    /// that guessed would file a SATA disk under the virtio tree.
    pub devpath: &'static str,
    pub dev: Arc<dyn BlockDev>,
}

/// Every disk the kernel has brought up, in the order it found them. The first
/// is the boot disk, whose partitions the early userland mounts.
///
/// A list rather than one slot because this kernel can have more than one disk:
/// the paravirtual block device is what the image builder attaches, and a real
/// SATA controller is what a second driver is for. Nothing above this cares
/// which came first except the boot path, which asks for the boot disk by name.
static DISKS: Mutex<RefCell<Vec<Disk>>> = Mutex::new(RefCell::new(Vec::new()));

/// Record a disk the kernel has, under the name, number and tree position its
/// driver gives it. Called once per disk, by the driver that brought it up.
///
/// The name is asserted unique because it is what userland *opens*: two disks
/// answering to `/dev/vda` would be two devices behind one name, which is a
/// worse failure than a boot that stops.
pub fn add_disk(name: &str, major: u32, devpath: &'static str, dev: Arc<dyn BlockDev>) {
    // The counting wrapper goes on at the door rather than inside any driver:
    // every path to storage passes through a registered `BlockDev`, so this is
    // the one place a count can be taken without touching a trait Linux owns.
    let counted = Counted::new(name, dev.clone());
    critical_section::with(|cs| {
        let mut disks = DISKS.borrow(cs).borrow_mut();
        assert!(
            !disks.iter().any(|disk| disk.name == name),
            "disk {name} was added twice"
        );
        disks.push(Disk {
            name: String::from(name),
            major,
            devpath,
            dev: counted.clone(),
        });
        COUNTED.borrow(cs).borrow_mut().push(counted);
    });
}

/// The disks the kernel has, for the callers that name them to userland: the
/// devfs builder, `/proc/partitions` and `/sys/class/block`.
pub fn disks() -> Vec<Disk> {
    critical_section::with(|cs| DISKS.borrow(cs).borrow().iter().map(clone_disk).collect())
}

fn clone_disk(disk: &Disk) -> Disk {
    Disk {
        name: disk.name.clone(),
        major: disk.major,
        devpath: disk.devpath,
        dev: disk.dev.clone(),
    }
}

/// The disk the kernel booted from: the first one brought up. The boot path
/// looking for its data partition asks here rather than keeping its own copy.
pub fn raw_disk() -> Option<Arc<dyn BlockDev>> {
    critical_section::with(|cs| {
        DISKS
            .borrow(cs)
            .borrow()
            .first()
            .map(|disk| disk.dev.clone())
    })
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

    /// Ask the device to push whatever it has cached to stable media. A
    /// filesystem that promises a commit point - a journal, or even just
    /// "these bytes are on the disk" - says this between the writes whose
    /// order it needs remembered. `false` means the device has no such
    /// concept, and the caller keeps the ordering guarantees the transport
    /// happens to give it; `true` is the answer from a device that either
    /// caches nothing (RAM) or has now flushed what it cached.
    fn flush(&self) -> bool {
        true
    }
}

/// Reads and writes counted, per disk.
///
/// §10.5's instrument needs one number the machine can answer: *how many
/// accesses did this workload make the device do*. The cache tests already
/// measure exactly that against a mock (`ext4/cache.rs`'s `Counting`), and a
/// mock cannot say anything about pipelining or a real driver's cost - so the
/// same count has to exist over the real device. The seam is where it goes:
/// every path to storage - ext4's cache miss, a partition window, a `dd` on
/// `/dev/sda` - passes through a `BlockDev`, so wrapping the registered device
/// counts everything above without any of them knowing.
///
/// The reads are **calls**, not bytes: one call is one transfer the driver
/// issues, which is the quantity a cache policy and a future readahead change
/// move. It is also why the unit is honest across the window - a partition's
/// counters are its own calls, and the disk's total stays the disk's because
/// the window wraps the *counted* device below it.
///
/// Plain `Relaxed` atomics: the count is read by a measurement, never used to
/// decide anything, and an access lost to a race between two cores' increments
/// would change a measured total by one - which is noise a uniprocessor kernel
/// does not even produce.
pub struct Counted {
    inner: Arc<dyn BlockDev>,
    reads: core::sync::atomic::AtomicU64,
    writes: core::sync::atomic::AtomicU64,
    name: String,
}

impl Counted {
    pub fn new(name: &str, inner: Arc<dyn BlockDev>) -> Arc<Counted> {
        Arc::new(Counted {
            inner,
            reads: core::sync::atomic::AtomicU64::new(0),
            writes: core::sync::atomic::AtomicU64::new(0),
            name: String::from(name),
        })
    }

    pub fn reads(&self) -> u64 {
        self.reads.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn writes(&self) -> u64 {
        self.writes.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl BlockDev for Counted {
    fn capacity_sectors(&self) -> u64 {
        self.inner.capacity_sectors()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        self.reads
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.inner.read_at(offset, buf);
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        self.writes
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.inner.write_at(offset, buf);
    }

    fn flush(&self) -> bool {
        self.inner.flush()
    }
}

/// Every registered disk's counting wrapper, in registration order.
///
/// [`add_disk`] wraps each device on the way in, so this always covers the
/// whole set, and the only readers are the door that reports the counts
/// (`uname(2)`'s private `release` word - see `abi::DISKSTAT_OSRELEASE_MAGIC`)
/// and a host test checking the shape.
static COUNTED: Mutex<RefCell<Vec<Arc<Counted>>>> = Mutex::new(RefCell::new(Vec::new()));

/// The disks the kernel has, with their counters: `(name, reads, writes)`, in
/// registration order.
pub fn io_stats() -> Vec<(String, u64, u64)> {
    critical_section::with(|cs| {
        COUNTED
            .borrow(cs)
            .borrow()
            .iter()
            .map(|disk| (disk.name.clone(), disk.reads(), disk.writes()))
            .collect()
    })
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A device that remembers nothing and counts nothing of its own - the
    /// thing under the wrapper, so every count asserted below is the
    /// wrapper's and none of it is inherited from a mock that agrees to be
    /// measured.
    struct Plain {
        sectors: u64,
        touched: core::sync::atomic::AtomicU64,
    }

    impl BlockDev for Plain {
        fn capacity_sectors(&self) -> u64 {
            self.sectors
        }
        fn read_at(&self, _offset: u64, buf: &mut [u8]) {
            self.touched
                .fetch_add(buf.len() as u64, core::sync::atomic::Ordering::Relaxed);
        }
        fn write_at(&self, _offset: u64, buf: &[u8]) {
            self.touched
                .fetch_add(buf.len() as u64, core::sync::atomic::Ordering::Relaxed);
        }
    }

    impl Plain {
        fn new(sectors: u64) -> Arc<Plain> {
            Arc::new(Plain {
                sectors,
                touched: core::sync::atomic::AtomicU64::new(0),
            })
        }
        fn touched(&self) -> u64 {
            self.touched.load(core::sync::atomic::Ordering::Relaxed)
        }
    }

    /// The counterfactual for §10.5's instrument: without the wrapper, no
    /// count moves; with it, each call moves exactly one, and the bytes still
    /// reach the device underneath. A `Counted` that forgot to forward, or
    /// counted the *bytes* instead of the calls, fails here - and a sweep that
    /// reported a constant zero would otherwise pass every claim t-diskbench
    /// makes about "the warm pass reads less than the cold one", because zero
    /// is less than everything.
    #[test]
    fn the_wrapper_counts_calls_and_forwards_every_access() {
        let plain = Plain::new(2048);
        let counted = Counted::new("testda", plain.clone());

        counted.read_at(0, &mut [0u8; 512]);
        counted.read_at(512, &mut [0u8; 512]);
        counted.write_at(1024, &[7u8; 512]);

        assert_eq!(counted.reads(), 2, "two reads counted");
        assert_eq!(counted.writes(), 1, "one write counted");
        assert_eq!(plain.touched(), 1536, "the device underneath saw the bytes");
        assert_eq!(counted.capacity_sectors(), 2048, "capacity forwarded");
    }

    /// A window over a counted device does not double-count and does not
    /// hide: the partition's own accesses are the disk's accesses, which is
    /// what keeps the disk total honest when userland opens `/dev/sda1`.
    #[test]
    fn a_window_over_a_counted_device_shares_the_count() {
        let counted = Counted::new("testdb", Plain::new(4096));
        let win = part::Window::new(counted.clone(), 8, 16);
        win.read_at(0, &mut [0u8; 512]);
        assert_eq!(counted.reads(), 1, "the window's read reached the disk");
    }
}
