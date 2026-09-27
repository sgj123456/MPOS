//! Finding a filesystem on a partitioned disk.
//!
//! The kernel used to be handed a bare image: whatever the block device
//! held *was* the ext2 filesystem, starting at byte zero. It is handed a real
//! disk now - one with a GPT on it, holding a bootloader, a kernel and this
//! filesystem side by side - so it has to read the partition table before it
//! can find the filesystem at all.
//!
//! Only what that needs is implemented. There is no partition enumeration for
//! anyone else to use, no `ioctl` to expose it, and no writing: the table is
//! read once at boot, and the partition that carries an ext2 superblock is
//! wrapped in [`Window`] and handed to the filesystem. A device that already
//! starts with a superblock - an unpartitioned image, which the host tests
//! still build - is accepted as it stands, so both shapes boot.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::blk::BlockDev;

/// Bytes a logical sector, which is the unit every offset here is counted in.
const SECTOR: u64 = 512;

/// Where the ext2 superblock sits inside a filesystem, and the offset of the
/// `s_magic` field within it. Both come from the on-disk format and are the
/// cheapest way to recognise one filesystem among several.
const SUPERBLOCK: u64 = 1024;
const SUPERBLOCK_MAGIC: usize = 56;
const EXT2_MAGIC: u16 = 0xEF53;

/// One GPT partition entry, and the offset of the fields inside it.
const GPT_ENTRY_SIZE: usize = 128;

/// A partition of `inner`: the same sectors, seen from a new origin.
///
/// Every offset a filesystem computes is relative to its own partition, so a
/// window is all the indirection there is between ext2 and the disk.
pub struct Window {
    inner: Arc<dyn BlockDev>,
    /// Byte offset of the partition's first sector on the underlying device.
    base: u64,
    sectors: u64,
}

impl Window {
    pub fn new(inner: Arc<dyn BlockDev>, start_lba: u64, sectors: u64) -> Window {
        Window {
            inner,
            base: start_lba * SECTOR,
            sectors,
        }
    }
}

impl BlockDev for Window {
    fn capacity_sectors(&self) -> u64 {
        self.sectors
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        self.inner.read_at(self.base + offset, buf);
    }

    fn write_at(&self, offset: u64, buf: &[u8]) {
        self.inner.write_at(self.base + offset, buf);
    }

    fn flush(&self) -> bool {
        self.inner.flush()
    }
}

/// The filesystem on `dev`, as a device scoped to whichever partition holds
/// it.
///
/// `dev` is returned unchanged when the superblock is already at the front of
/// the device: an image with no partition table is still a disk this kernel
/// knows how to read, and the milestone that formats one should not have to
/// learn about GPT to pass.
pub fn find_filesystem(dev: Arc<dyn BlockDev>) -> Option<Arc<dyn BlockDev>> {
    if has_ext2(dev.as_ref(), 0) {
        return Some(dev);
    }
    for (start, sectors) in partitions(dev.as_ref()) {
        if start.checked_add(sectors)? > dev.capacity_sectors() {
            continue;
        }
        if has_ext2(dev.as_ref(), start * SECTOR) {
            return Some(Arc::new(Window::new(dev, start, sectors)));
        }
    }
    None
}

/// Find the root filesystem partition (partition 3 in the new layout).
///
/// This is the ext2 partition that contains the real root filesystem.
/// Unlike `find_filesystem` which finds *any* ext2 partition, this specifically
/// targets the rootfs partition for the pivot_root transition.
pub fn find_rootfs(dev: Arc<dyn BlockDev>) -> Option<Arc<dyn BlockDev>> {
    // The rootfs is on partition 3 (1-indexed), which is the third entry
    // in the GPT table. We skip the first two partitions (ESP and boot)
    // and look for the third ext2 partition.
    let parts = partitions(dev.as_ref());
    if parts.len() < 3 {
        return None;
    }
    let (start, sectors) = parts[2]; // 0-indexed, so index 2 is partition 3
    if start.checked_add(sectors)? > dev.capacity_sectors() {
        return None;
    }
    if has_ext2(dev.as_ref(), start * SECTOR) {
        Some(Arc::new(Window::new(dev, start, sectors)))
    } else {
        None
    }
}

/// Find the boot partition (partition 2 in the new layout).
///
/// This is the ext2 partition that contains the kernel and initramfs.
/// After pivot_root, this can be mounted at /boot.
pub fn find_boot(dev: Arc<dyn BlockDev>) -> Option<Arc<dyn BlockDev>> {
    // The boot partition is on partition 2 (1-indexed), which is the second
    // entry in the GPT table.
    let parts = partitions(dev.as_ref());
    if parts.len() < 2 {
        return None;
    }
    let (start, sectors) = parts[1]; // 0-indexed, so index 1 is partition 2
    if start.checked_add(sectors)? > dev.capacity_sectors() {
        return None;
    }
    if has_ext2(dev.as_ref(), start * SECTOR) {
        Some(Arc::new(Window::new(dev, start, sectors)))
    } else {
        None
    }
}

/// Find the data partition (partition 4 in the new layout).
///
/// This is the small ext2 partition containing seed files, mounted at /mnt.
pub fn find_data(dev: Arc<dyn BlockDev>) -> Option<Arc<dyn BlockDev>> {
    let parts = partitions(dev.as_ref());
    if parts.len() < 4 {
        return None;
    }
    let (start, sectors) = parts[3]; // 0-indexed, so index 3 is partition 4
    if start.checked_add(sectors)? > dev.capacity_sectors() {
        return None;
    }
    if has_ext2(dev.as_ref(), start * SECTOR) {
        Some(Arc::new(Window::new(dev, start, sectors)))
    } else {
        None
    }
}

/// Whether an ext2 superblock begins `at` bytes into the device.
fn has_ext2(dev: &dyn BlockDev, at: u64) -> bool {
    let mut buf = [0u8; SECTOR as usize];
    // A device too short to hold the field cannot hold the filesystem either,
    // and reading past the end is a driver panic rather than an error.
    if at + SUPERBLOCK + SUPERBLOCK_MAGIC as u64 + 2 > dev.capacity_sectors() * SECTOR {
        return false;
    }
    dev.read_at(at + SUPERBLOCK, &mut buf);
    u16::from_le_bytes([buf[SUPERBLOCK_MAGIC], buf[SUPERBLOCK_MAGIC + 1]]) == EXT2_MAGIC
}

/// The `(first_lba, sectors)` of every entry in the partition table, in table
/// order, or nothing at all if there is no table to read.
/// Every partition on `dev`, in table order, as `(start_lba, sectors)`.
///
/// The devfs builder publishes one node per entry - `vda1`, `vda2`, ... - so
/// that userland can name the partition it wants to mount.
pub fn partitions(dev: &dyn BlockDev) -> Vec<(u64, u64)> {
    let mut mbr = [0u8; SECTOR as usize];
    if dev.capacity_sectors() < 2 {
        return Vec::new();
    }
    dev.read_at(0, &mut mbr);
    // The signature is what makes the first sector a partition table rather
    // than the first sector of something else.
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Vec::new();
    }

    // A protective MBR - one whose first entry claims the whole disk as type
    // 0xEE - is the marker that a GPT follows.
    let entries = if mbr[446 + 4] == 0xEE {
        gpt_entries(dev)
    } else {
        mbr_entries(&mbr)
    };

    // Every finder above this one checks the range it is about to hand out
    // against the device. This has to as well, or the two halves of the module
    // disagree about what is on the disk: `devfs` builds one node per entry
    // this returns, so an entry the table invented is a `/dev/vda4` that opens
    // and reads, and answers with whatever the driver's own clamp happens to
    // leave in the buffer. A table names partitions; the disk decides whether
    // it has them.
    let capacity = dev.capacity_sectors();
    entries
        .into_iter()
        .filter(|&(start, sectors)| {
            start
                .checked_add(sectors)
                .is_some_and(|end| end <= capacity)
        })
        .collect()
}

/// The four entries of a classic MBR, skipping the empty ones.
fn mbr_entries(mbr: &[u8; SECTOR as usize]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for index in 0..4 {
        let at = 446 + index * 16;
        let start = u32::from_le_bytes(mbr[at + 8..at + 12].try_into().unwrap()) as u64;
        let length = u32::from_le_bytes(mbr[at + 12..at + 16].try_into().unwrap()) as u64;
        if start != 0 && length != 0 {
            out.push((start, length));
        }
    }
    out
}

/// The partition entries the GPT header points at.
///
/// The header lives in the second sector, and it names an array of entries
/// that is usually large enough to need several reads - the trait hands out
/// at most a page at a time - so the array is walked in pages.
fn gpt_entries(dev: &dyn BlockDev) -> Vec<(u64, u64)> {
    let mut header = [0u8; SECTOR as usize];
    dev.read_at(SECTOR, &mut header);
    if &header[0..8] != b"EFI PART" {
        return Vec::new();
    }

    let table_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as u64;
    let stride = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
    // The spec fixes the stride at 128 and requires a power of two no smaller
    // than that; anything else is a table this reader does not understand,
    // and guessing at the layout would read partition boundaries out of the
    // middle of another field.
    if stride < GPT_ENTRY_SIZE || !stride.is_power_of_two() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut page = [0u8; crate::mm::PAGE_SIZE];
    let mut done = 0u64;
    let total = count * stride as u64;
    while done < total {
        let want = core::cmp::min(page.len() as u64, total - done) as usize;
        // Reads are whole sectors, so the last page of a table that does not
        // end on a sector boundary is rounded up rather than short.
        let want = want.div_ceil(SECTOR as usize) * SECTOR as usize;
        let buf = &mut page[..want];
        dev.read_at(table_lba * SECTOR + done, buf);

        for entry in buf.chunks_exact(stride) {
            // An all-zero type GUID is an entry nobody has claimed.
            if entry[0..16].iter().all(|&byte| byte == 0) {
                continue;
            }
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            if first == 0 || last < first {
                continue;
            }
            out.push((first, last - first + 1));
        }
        done += want as u64;
    }
    out
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::vec;
    use core::cell::RefCell;
    use critical_section::Mutex;

    /// An in-memory disk, so a table and the filesystems on it can be laid
    /// out and then read back the way boot reads them.
    struct Fake {
        bytes: Mutex<RefCell<Vec<u8>>>,
        sectors: u64,
    }

    impl Fake {
        fn new(sectors: u64) -> Fake {
            Fake {
                bytes: Mutex::new(RefCell::new(vec![0u8; (sectors * SECTOR) as usize])),
                sectors,
            }
        }

        fn put(&self, at: u64, bytes: &[u8]) {
            critical_section::with(|cs| {
                let mut disk = self.bytes.borrow(cs).borrow_mut();
                let at = at as usize;
                disk[at..at + bytes.len()].copy_from_slice(bytes);
            });
        }

        fn put_ext2(&self, lba: u64) {
            // Just enough of a superblock for the magic to be there.
            let mut sb = [0u8; SECTOR as usize];
            sb[SUPERBLOCK_MAGIC..SUPERBLOCK_MAGIC + 2].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
            self.put(lba * SECTOR + SUPERBLOCK, &sb);
        }
    }

    impl BlockDev for Fake {
        fn capacity_sectors(&self) -> u64 {
            self.sectors
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            critical_section::with(|cs| {
                let disk = self.bytes.borrow(cs).borrow();
                let at = offset as usize;
                buf.copy_from_slice(&disk[at..at + buf.len()]);
            });
        }
        fn write_at(&self, offset: u64, buf: &[u8]) {
            critical_section::with(|cs| {
                let mut disk = self.bytes.borrow(cs).borrow_mut();
                let at = offset as usize;
                disk[at..at + buf.len()].copy_from_slice(buf);
            });
        }
    }

    /// Lay a GPT over `disk`, with one entry per `(start, sectors)`.
    fn write_gpt(disk: &Fake, entries: &[(u64, u64)]) {
        let mut mbr = [0u8; SECTOR as usize];
        // Protective MBR: one entry of type 0xEE covering the disk.
        mbr[446 + 4] = 0xEE;
        mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&(disk.sectors as u32).to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        disk.put(0, &mbr);

        let mut header = [0u8; SECTOR as usize];
        header[0..8].copy_from_slice(b"EFI PART");
        header[72..80].copy_from_slice(&2u64.to_le_bytes()); // entries at LBA 2
        header[80..84].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        header[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
        disk.put(SECTOR, &header);

        for (index, &(start, sectors)) in entries.iter().enumerate() {
            let at = 2 * SECTOR + (index * GPT_ENTRY_SIZE) as u64;
            let mut entry = [0u8; GPT_ENTRY_SIZE];
            entry[0] = 0xAF; // a type GUID, so the entry is not empty
            entry[32..40].copy_from_slice(&start.to_le_bytes());
            entry[40..48].copy_from_slice(&(start + sectors - 1).to_le_bytes());
            disk.put(at, &entry);
        }
    }

    #[test]
    fn an_unpartitioned_image_is_passed_through_untouched() {
        let disk = Fake::new(64);
        disk.put_ext2(0);
        let found = find_filesystem(Arc::new(disk)).expect("the filesystem is at the front");
        assert_eq!(found.capacity_sectors(), 64);
    }

    #[test]
    fn the_partition_holding_the_superblock_is_the_one_that_is_opened() {
        // A boot partition with no filesystem at all, then the data one.
        let disk = Fake::new(4096);
        write_gpt(&disk, &[(1, 2048), (2049, 2047)]);
        disk.put_ext2(2049);

        let found = find_filesystem(Arc::new(disk)).expect("the second partition mounts");
        assert_eq!(found.capacity_sectors(), 2047);

        // Reading through the window lands inside the partition, not at the
        // front of the disk: the superblock the window sees is the one that
        // was written there.
        let mut buf = [0u8; SECTOR as usize];
        found.read_at(SUPERBLOCK, &mut buf);
        assert_eq!(
            u16::from_le_bytes([buf[SUPERBLOCK_MAGIC], buf[SUPERBLOCK_MAGIC + 1]]),
            EXT2_MAGIC
        );
    }

    #[test]
    fn a_table_with_no_filesystem_opens_nothing() {
        let disk = Fake::new(4096);
        write_gpt(&disk, &[(1, 2048)]);
        assert!(find_filesystem(Arc::new(disk)).is_none());
    }

    #[test]
    fn an_mbr_partition_table_is_read_too() {
        let disk = Fake::new(4096);
        let mut mbr = [0u8; SECTOR as usize];
        mbr[446 + 4] = 0x83; // Linux filesystem
        mbr[446 + 8..446 + 12].copy_from_slice(&100u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&500u32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        disk.put(0, &mbr);
        disk.put_ext2(100);

        let found = find_filesystem(Arc::new(disk)).expect("the MBR entry mounts");
        assert_eq!(found.capacity_sectors(), 500);
    }

    #[test]
    fn a_partition_running_past_the_end_is_ignored() {
        let disk = Fake::new(1024);
        write_gpt(&disk, &[(1, 4096)]);
        disk.put_ext2(1);
        // The superblock is where a partition would be, but the entry claims
        // sectors the device does not have - so the whole disk is not ext2
        // either, and nothing mounts.
        assert!(find_filesystem(Arc::new(disk)).is_none());
    }

    #[test]
    fn a_device_with_no_table_and_no_superblock_opens_nothing() {
        let disk = Fake::new(64);
        assert!(find_filesystem(Arc::new(disk)).is_none());
    }

    /// devfs publishes one node per entry - `vda1`, `vda2`, ... - so the
    /// enumeration has to be the table's own order and nothing else, and an
    /// entry nobody claimed (an all-zero type GUID) is not a partition.
    #[test]
    fn every_claimed_entry_is_enumerated_in_table_order() {
        let disk = Fake::new(4096);
        write_gpt(&disk, &[(1, 100), (101, 200), (301, 300)]);
        assert_eq!(partitions(&disk), vec![(1, 100), (101, 200), (301, 300)]);

        // Blanking the middle entry's type GUID unclaims it: the bounds are
        // still there, but nothing has said what the partition is.
        let mut empty = [0u8; GPT_ENTRY_SIZE];
        empty[32..40].copy_from_slice(&101u64.to_le_bytes());
        empty[40..48].copy_from_slice(&300u64.to_le_bytes());
        disk.put(2 * SECTOR + GPT_ENTRY_SIZE as u64, &empty);
        assert_eq!(partitions(&disk), vec![(1, 100), (301, 300)]);
    }

    /// Writing through a window lands inside the partition, which is the
    /// direction the filesystem's own writes take: an offset of 0 is the
    /// partition's first sector, not the disk's.
    #[test]
    fn a_window_writes_inside_its_own_partition() {
        let disk = Arc::new(Fake::new(4096));
        write_gpt(&disk, &[(1, 2048), (2049, 2047)]);
        disk.put_ext2(2049);
        let found = find_filesystem(disk.clone()).expect("the second partition mounts");

        let mut marker = [0u8; 8];
        marker.copy_from_slice(b"written!");
        found.write_at(SUPERBLOCK + 64, &marker);

        // Read back off the raw disk, where those bytes live at the
        // partition's first sector plus the offset written through the
        // window: an offset of 0 is the partition's, not the disk's.
        let mut buf = [0u8; 8];
        disk.read_at(2049 * SECTOR + SUPERBLOCK + 64, &mut buf);
        assert_eq!(buf, marker);
    }

    /// The three named partitions are picked by position, not by content:
    /// rootfs is the third entry, boot the second, data the fourth. That is
    /// the layout `mkdisk.sh` writes, and it is the one assumption a
    /// positional lookup makes that a content search cannot.
    #[test]
    fn the_named_partitions_are_the_ones_at_their_position() {
        let disk = Arc::new(Fake::new(8192));
        write_gpt(
            &disk,
            &[(1, 1024), (1025, 1024), (2049, 1024), (3073, 1024)],
        );
        disk.put_ext2(1025); // boot
        disk.put_ext2(2049); // rootfs
        disk.put_ext2(3073); // data

        let boot = find_boot(disk.clone()).expect("partition 2 is boot");
        assert_eq!(boot.capacity_sectors(), 1024);
        let rootfs = find_rootfs(disk.clone()).expect("partition 3 is the rootfs");
        assert_eq!(rootfs.capacity_sectors(), 1024);
        let data = find_data(disk).expect("partition 4 is the data partition");
        assert_eq!(data.capacity_sectors(), 1024);
    }

    /// A short table leaves the later names absent rather than answering with
    /// whichever partition happens to be there - a two-partition disk has no
    /// rootfs by this rule, however much ext2 is on it.
    #[test]
    fn a_table_shorter_than_the_position_answers_nothing() {
        let disk = Arc::new(Fake::new(4096));
        write_gpt(&disk, &[(1, 1024), (1025, 1024)]);
        disk.put_ext2(1);
        disk.put_ext2(1025);

        assert!(find_boot(disk.clone()).is_some(), "partition 2 is there");
        assert!(find_rootfs(disk.clone()).is_none(), "there is no third");
        assert!(find_data(disk).is_none(), "there is no fourth");
    }

    /// `has_ext2` asks the device two questions at once: is the magic at the
    /// field, and is the device long enough to hold that field. The second is
    /// not decoration - a read past the end is a driver panic, and a short
    /// device cannot hold the filesystem either way.
    #[test]
    fn has_ext2_needs_a_magic_it_can_actually_reach() {
        let disk = Fake::new(64);
        assert!(!has_ext2(&disk, 0), "nothing has been written yet");
        disk.put_ext2(0);
        assert!(has_ext2(&disk, 0));
        assert!(
            !has_ext2(&disk, 2048),
            "the field on a partition that holds nothing"
        );

        // One sector is shorter than the superblock's own magic field.
        let too_short = Fake::new(1);
        assert!(!has_ext2(&too_short, 0));
    }

    /// An MBR's four slots are read in order, and a slot with no start or no
    /// length is not a partition - which is how an empty slot spells itself.
    #[test]
    fn mbr_entries_read_the_four_slots_in_order() {
        let mut mbr = [0u8; SECTOR as usize];
        let set = |mbr: &mut [u8; SECTOR as usize], slot: usize, start: u32, len: u32| {
            let at = 446 + slot * 16;
            mbr[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
            mbr[at + 12..at + 16].copy_from_slice(&len.to_le_bytes());
        };
        set(&mut mbr, 1, 100, 40);
        set(&mut mbr, 2, 300, 0); // a length of zero is not a partition
        set(&mut mbr, 3, 200, 8);
        assert_eq!(mbr_entries(&mbr), vec![(100, 40), (200, 8)]);
    }

    /// A GPT's stride is fixed by the format. Anything narrower than an entry,
    /// or not a power of two, is a table this reader refuses rather than
    /// guesses at - a wrong stride reads partition boundaries out of the
    /// middle of another field.
    #[test]
    fn gpt_entries_refuse_a_stride_the_format_does_not_allow() {
        let disk = Fake::new(4096);
        // A protective MBR, which is what says "a GPT follows".
        let mut mbr = [0u8; SECTOR as usize];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        mbr[446 + 4] = 0xEE;
        disk.put(0, &mbr);

        let header = |stride: u32, count: u32| {
            let mut header = [0u8; SECTOR as usize];
            header[0..8].copy_from_slice(b"EFI PART");
            header[72..80].copy_from_slice(&2u64.to_le_bytes()); // table at LBA 2
            header[80..84].copy_from_slice(&count.to_le_bytes());
            header[84..88].copy_from_slice(&stride.to_le_bytes());
            header
        };

        // A stride the format allows is walked: two entries, each a type GUID
        // (all-zero means "nobody has claimed it") and a first and last LBA.
        disk.put(SECTOR, &header(128, 2));
        let mut table = [0u8; SECTOR as usize];
        table[0..16].fill(0xff); // entry 0 is claimed
        table[32..40].copy_from_slice(&10u64.to_le_bytes());
        table[40..48].copy_from_slice(&29u64.to_le_bytes());
        table[128..144].fill(0xff); // entry 1 is claimed
        table[128 + 32..128 + 40].copy_from_slice(&40u64.to_le_bytes());
        table[128 + 40..128 + 48].copy_from_slice(&59u64.to_le_bytes());
        disk.put(2 * SECTOR, &table);
        assert_eq!(partitions(&disk), vec![(10, 20), (40, 20)]);

        // Anything narrower than an entry, or not a power of two, is no table
        // at all. (The spec allows any power of two from 128 up, so 256 would
        // be walked and is not in this list.)
        for stride in [64u32, 100, 127] {
            disk.put(SECTOR, &header(stride, 2));
            assert!(partitions(&disk).is_empty(), "stride {stride} was walked");
        }

        // And a header without its signature is not a GPT header.
        let mut header = header(128, 2);
        header[0..8].copy_from_slice(b"NOT PART");
        disk.put(SECTOR, &header);
        assert!(partitions(&disk).is_empty());
    }
}
