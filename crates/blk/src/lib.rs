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
            dev,
        });
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
