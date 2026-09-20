//! The ext2 filesystem, read and write.
//!
//! Everything runs through one design choice: the block cache is
//! *write-through*. Every modification lands on the disk before the call
//! returns, so the cache is the only state an [`Ext2Fs`] owns - dropping it
//! loses nothing, and a second mount over the same device sees every write.
//! That costs one extra write per allocation (counter blocks get rewritten
//! immediately) and buys crash-plain semantics with no dirty tracking, no
//! flush path and no journal - the right trade for a kernel whose storage
//! lives in a QEMU image.
//!
//! Allocation is first-fit: scan the group descriptors for a group with free
//! blocks or inodes, scan that group's bitmap for a zero bit, set it, and
//! push the decremented free counts back to disk at once. Correct on-disk
//! bookkeeping after every operation means the image stays inspectable with
//! the host's own ext2 tools after a run.
//!
//! Block mapping covers direct, single-indirect and double-indirect; a file
//! that would need triple-indirect gets [`VfsError::FileTooBig`] rather than
//! silent truncation. Removal is the mirror of that: `unlink` drops a name
//! and gives the freed blocks and inode straight back to the bitmaps, so a
//! filesystem can be left as it was found. What is deliberately absent:
//! htree-indexed directories (refused rather than misparsed), permission
//! checks (there is no identity to check against) and timestamps (preserved
//! when an inode is rewritten, never updated).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::blk::BlockDev;

use super::{FileSystem, Ino, Inode, Metadata, NodeKind, VfsError, VfsResult};

const MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
/// The superblock is a fixed 1024 bytes at byte 1024, whatever the block size.
const SUPERBLOCK_AT: u64 = 1024;
const SUPERBLOCK_BYTES: usize = 1024;
const DIRECT_BLOCKS: usize = 12;
/// How much of the 128-byte-plus inode we parse and rewrite. i_block ends at
/// byte 100; everything after is metadata this driver does not touch.
const INODE_PARSED: usize = 104;
const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHRDEV: u8 = 3;
const FT_SYMLINK: u8 = 7;
const S_IFMT: u16 = 0xf000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xa000;
const S_IFCHR: u16 = 0x2000;
/// The most a "fast" symbolic link can hold: the target lives in the fifteen
/// four-byte block pointers, so the format's own ceiling is 60 bytes. Alpine's
/// userland is built almost entirely out of links shorter than that -
/// `/bin/sh -> /bin/busybox` is twelve - so the fast form is the common one,
/// not an optimisation.
const FAST_SYMLINK_MAX: u32 = 60;
const INCOMPAT_FILETYPE: u32 = 1 << 1;
/// The directory is htree-indexed: block 0 is an index root rather than a
/// plain list of entries, so inserts have to maintain the tree. Reads are
/// immune - the index bytes hide in the slack of `..` and of whole-block
/// fake dirents, which every linear dirent walker skips - and so is removal,
/// which never moves an entry between leaves. Only insertion cares.
const INODE_INDEX_FL: u32 = 1 << 12;
/// `EXT2_FLAGS_UNSIGNED_HASH` in the superblock: the tree's hashes were made
/// with unsigned character semantics, which renumbers the stored hash
/// version from the signed variants onto the unsigned ones.
const SB_FLAGS_UNSIGNED_HASH: u32 = 1 << 1;
/// The largest hash an index entry can name; `<< 1` is where the clamped
/// ones go.
const HTREE_EOF_32BIT: u32 = 0x7fff_ffff;
/// An htree never grows deeper than one intermediate index level below the
/// root - the shape every ext2/ext4 without the `largedir` feature makes.
const HTREE_MAX_LEVELS: u8 = 1;
const GROUP_DESC_BYTES: usize = 32;
const DIRENT_HEADER: usize = 8;
/// Direct-mapped cache: one slot per possible block modulo this count.
const CACHE_SLOTS: usize = 32;
const NO_BLOCK: u64 = u64::MAX;

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("in-bounds u16"))
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("in-bounds u32"))
}

fn set16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn set32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// The fields of the on-disk superblock this driver keeps.
struct Super {
    inodes_count: u32,
    blocks_count: u32,
    first_data_block: u32,
    log_block_size: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    first_ino: u32,
    inode_size: u16,
    /// The htree hash seed: four words at `s_hash_seed` (offset 236, past
    /// the journal fields the rev-1 layout grew), salt shared by every
    /// directory index on this filesystem.
    hash_seed: [u32; 4],
    /// Whether those indexes were built with unsigned character semantics -
    /// the `s_flags` bit (offset 352) Linux records the first time it mounts
    /// a filesystem whose hash flavor was never written down. A filesystem
    /// that says nothing, like this image's, reads as signed.
    hash_unsigned: bool,
    /// The superblock's own home: block and offset within it. It moves with
    /// the block size, so the free counters can be rewritten in place later.
    sb_block: u64,
    sb_off: usize,
}

impl Super {
    fn parse(bytes: &[u8]) -> Result<Super, &'static str> {
        if le16(bytes, 56) != MAGIC {
            return Err("no ext2 magic at byte 1024");
        }
        let log_block_size = le32(bytes, 24);
        if log_block_size > 2 {
            return Err("ext2 block size above 4096 is unsupported");
        }
        if le32(bytes, 28) != log_block_size {
            return Err("ext2 fragments are unsupported");
        }
        if le32(bytes, 96) & !INCOMPAT_FILETYPE != 0 {
            return Err("ext2 carries incompatible features this driver does not implement");
        }
        let (inode_size, first_ino) = match le32(bytes, 76) {
            0 => (128, 11),
            _ => (le16(bytes, 88) as usize, le32(bytes, 84)),
        };
        if inode_size < 128 {
            return Err("ext2 inode size below 128 bytes is corrupt");
        }
        let first_data_block = le32(bytes, 20);
        let block_size = 1024usize << log_block_size;
        let sb = Super {
            inodes_count: le32(bytes, 0),
            blocks_count: le32(bytes, 4),
            first_data_block,
            log_block_size,
            blocks_per_group: le32(bytes, 32),
            inodes_per_group: le32(bytes, 40),
            first_ino,
            inode_size: inode_size as u16,
            hash_seed: [
                le32(bytes, 236),
                le32(bytes, 240),
                le32(bytes, 244),
                le32(bytes, 248),
            ],
            hash_unsigned: le32(bytes, 352) & SB_FLAGS_UNSIGNED_HASH != 0,
            sb_block: first_data_block as u64,
            sb_off: SUPERBLOCK_AT as usize - first_data_block as usize * block_size,
        };
        if sb.blocks_per_group != 8 * block_size as u32 {
            return Err("ext2 blocks-per-group is not the standard eight per KiB");
        }
        if sb.inodes_per_group == 0 {
            return Err("ext2 superblock claims zero inodes per group");
        }
        Ok(sb)
    }
}

/// One group descriptor: where the group's metadata lives, and how much of it
/// is free. Kept as a value read fresh from disk whenever needed - the disk is
/// the authority, and the cache makes re-reading cheap.
#[derive(Clone, Copy)]
struct Group {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks: u16,
    free_inodes: u16,
}

/// The inode fields this driver reads and writes, decoded out of the table.
#[derive(Clone, Copy)]
struct InodeData {
    mode: u16,
    flags: u32,
    size: u32,
    links: u16,
    /// In 512-byte sectors, as ext2 counts them.
    blocks: u32,
    block: [u32; 15],
}

impl InodeData {
    fn parse(bytes: &[u8]) -> InodeData {
        InodeData {
            mode: le16(bytes, 0),
            flags: le32(bytes, 32),
            size: le32(bytes, 4),
            links: le16(bytes, 26),
            blocks: le32(bytes, 28),
            block: core::array::from_fn(|i| le32(bytes, 40 + 4 * i)),
        }
    }

    /// Write back exactly the fields this driver owns, leaving the rest of the
    /// on-disk inode (times, reserved space) untouched.
    fn store(&self, bytes: &mut [u8]) {
        set16(bytes, 0, self.mode);
        set32(bytes, 4, self.size);
        set32(bytes, 28, self.blocks);
        set16(bytes, 26, self.links);
        set32(bytes, 32, self.flags);
        for (i, value) in self.block.iter().enumerate() {
            set32(bytes, 40 + 4 * i, *value);
        }
    }

    fn kind(&self) -> NodeKind {
        match self.mode & S_IFMT {
            S_IFDIR => NodeKind::Dir,
            S_IFCHR => NodeKind::CharDev,
            // A link is a node in its own right, not a file that happens to
            // hold text: resolution treats it as a name to follow, and the
            // fifteen block pointers hold its target rather than data.
            S_IFLNK => NodeKind::Symlink,
            // Regular files, and the node types this kernel has no use for -
            // fifos, sockets, block devices - which read as plain files until
            // something opens one and finds out otherwise.
            _ => NodeKind::File,
        }
    }

    /// Whether the target of a symbolic link lives inside the inode itself.
    ///
    /// The format's rule: a link that fits in the block-pointer area is stored
    /// there and has no blocks allocated, which is exactly what `i_blocks == 0`
    /// says. A longer target is written into data blocks like a file's, and
    /// `i_blocks` counts them.
    fn is_fast_symlink(&self) -> bool {
        self.size <= FAST_SYMLINK_MAX && self.blocks == 0
    }
}

struct Slot {
    block: u64,
    bytes: Vec<u8>,
}

/// Direct-mapped, write-through block cache: `block % CACHE_SLOTS` slots, a
/// miss loads from the device, every modification is pushed straight back.
struct Cache {
    slots: Vec<Slot>,
}

impl Cache {
    fn new(block_size: usize) -> Cache {
        Cache {
            slots: (0..CACHE_SLOTS)
                .map(|_| Slot {
                    block: NO_BLOCK,
                    bytes: vec![0; block_size],
                })
                .collect(),
        }
    }

    /// The page holding `block`, loaded from `dev` on a miss. The borrow keeps
    /// the slot resident, so callers must finish with the slice before the
    /// next cache access.
    fn slot(&mut self, dev: &dyn BlockDev, block: u64, block_size: usize) -> &mut [u8] {
        let slot = &mut self.slots[(block % CACHE_SLOTS as u64) as usize];
        if slot.block != block {
            dev.read_at(block * block_size as u64, &mut slot.bytes);
            slot.block = block;
        }
        &mut slot.bytes
    }
}

/// The filesystem state shared by every inode of one mount.
struct Core {
    dev: Arc<dyn BlockDev>,
    sb: Super,
    groups: u32,
    cache: Mutex<RefCell<Cache>>,
    inodes: Mutex<RefCell<BTreeMap<u32, Arc<dyn Inode>>>>,
    /// Whether this filesystem was mounted read-only. It lives here, on the
    /// one piece of state every inode of the mount shares, because "read-only"
    /// is a property of the mount and not of a file - and because it has to be
    /// the same answer for every path into it.
    read_only: core::sync::atomic::AtomicBool,
}

impl Core {
    /// Refuse a write to a filesystem mounted read-only.
    ///
    /// `mount -o ro` is a promise the filesystem keeps, not a hint: an
    /// initramfs mounts the root it has not yet trusted this way, and a
    /// kernel that accepted the flag and then wrote anyway would be lying
    /// about the one thing the caller asked for.
    fn writable(&self) -> VfsResult<()> {
        if self.read_only.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(VfsError::ReadOnly);
        }
        Ok(())
    }

    fn set_read_only(&self, read_only: bool) {
        self.read_only
            .store(read_only, core::sync::atomic::Ordering::Relaxed);
    }

    fn mount(dev: Arc<dyn BlockDev>) -> Result<Core, &'static str> {
        // The superblock is read before the cache exists: the cache's slot
        // size is one of the things it decides.
        let mut bytes = [0u8; SUPERBLOCK_BYTES];
        dev.read_at(SUPERBLOCK_AT, &mut bytes);
        let sb = Super::parse(&bytes)?;
        let groups = (sb.blocks_count - sb.first_data_block).div_ceil(sb.blocks_per_group);
        let block_size = 1024usize << sb.log_block_size;
        Ok(Core {
            read_only: core::sync::atomic::AtomicBool::new(false),
            dev,
            sb,
            groups,
            cache: Mutex::new(RefCell::new(Cache::new(block_size))),
            inodes: Mutex::new(RefCell::new(BTreeMap::new())),
        })
    }

    fn block_size(&self) -> usize {
        1024 << self.sb.log_block_size
    }

    // ---- cache primitives ----

    fn read_bytes(&self, block: u64, off: usize, dst: &mut [u8]) {
        let block_size = self.block_size();
        assert!(
            off + dst.len() <= block_size,
            "read of {} bytes at {} runs past block {}",
            dst.len(),
            off,
            block
        );
        // A block number at or past the end of the filesystem is what a
        // misextracted pointer looks like, and the device below answers it
        // with an I/O error that names nothing. Refuse it here, where the
        // number still has an identity.
        assert!(
            block < self.sb.blocks_count as u64,
            "ext2 block {block} is past the end of the filesystem ({} blocks)",
            self.sb.blocks_count
        );
        critical_section::with(|cs| {
            let mut cache = self.cache.borrow(cs).borrow_mut();
            let page = cache.slot(&*self.dev, block, block_size);
            dst.copy_from_slice(&page[off..off + dst.len()]);
        });
    }

    /// Run `f` over a cached block; write back through the device only if `f`
    /// reports a modification. Write-through in one step: the disk never
    /// trails the cache.
    fn update_block(&self, block: u64, f: impl FnOnce(&mut [u8]) -> bool) -> bool {
        let block_size = self.block_size();
        critical_section::with(|cs| {
            let mut cache = self.cache.borrow(cs).borrow_mut();
            let dev = self.dev.clone();
            let page = cache.slot(&*self.dev, block, block_size);
            if f(page) {
                dev.write_at(block * block_size as u64, page);
                true
            } else {
                false
            }
        })
    }

    fn write_bytes(&self, block: u64, off: usize, src: &[u8]) {
        self.update_block(block, |page| {
            page[off..off + src.len()].copy_from_slice(src);
            true
        });
    }

    // ---- groups and allocation ----

    fn group(&self, g: u32) -> Group {
        assert!(g < self.groups, "ext2 group {g} out of range");
        let block_size = self.block_size();
        let at = g as usize * GROUP_DESC_BYTES;
        let block = self.sb.first_data_block as u64 + 1 + (at / block_size) as u64;
        let mut bytes = [0u8; GROUP_DESC_BYTES];
        self.read_bytes(block, at % block_size, &mut bytes);
        Group {
            block_bitmap: le32(&bytes, 0) as u64,
            inode_bitmap: le32(&bytes, 4) as u64,
            inode_table: le32(&bytes, 8) as u64,
            free_blocks: le16(&bytes, 12),
            free_inodes: le16(&bytes, 14),
        }
    }

    /// Push a change in free block/inode counts to both the group descriptor
    /// and the superblock, so the on-disk bookkeeping is right after every
    /// single allocation rather than at some future flush.
    ///
    /// The two structures carry the same two numbers at *different* offsets,
    /// and the easy mistake is to carry one over to the other:
    ///
    /// * a group descriptor is `u32, u32, u32, u16, u16, u16` - free blocks at
    ///   12, free inodes at **14**, used directories at 16;
    /// * the superblock is `u32, u32, u32, u32, u32` - free blocks at 12, free
    ///   inodes at **16**.
    ///
    /// Writing the inode delta at 14 in the superblock does not fail loudly:
    /// it straddles the two counters, so the free-block u32 ends up with a
    /// stray 0xffff in its high half and `statfs` reports an absurd number.
    fn bump_free(&self, g: u32, blocks: i32, inodes: i32) {
        let block_size = self.block_size();
        let at = g as usize * GROUP_DESC_BYTES;
        let desc_block = self.sb.first_data_block as u64 + 1 + (at / block_size) as u64;
        let desc_off = at % block_size;
        self.update_block(desc_block, |page| {
            set16(
                page,
                desc_off + 12,
                (le16(page, desc_off + 12) as i32 + blocks) as u16,
            );
            set16(
                page,
                desc_off + 14,
                (le16(page, desc_off + 14) as i32 + inodes) as u16,
            );
            true
        });
        self.update_block(self.sb.sb_block, |page| {
            let at = self.sb.sb_off;
            set32(
                page,
                at + 12,
                (le32(page, at + 12) as i64 + blocks as i64) as u32,
            );
            set32(
                page,
                at + 16,
                (le32(page, at + 16) as i64 + inodes as i64) as u32,
            );
            true
        });
    }

    fn alloc_block(&self) -> VfsResult<u64> {
        let per_group = self.sb.blocks_per_group as u64;
        for g in 0..self.groups {
            let group = self.group(g);
            if group.free_blocks == 0 {
                continue;
            }
            let first = self.sb.first_data_block as u64 + g as u64 * per_group;
            let mut found = None;
            self.update_block(group.block_bitmap, |page| {
                for (byte_i, byte) in page.iter_mut().enumerate() {
                    if *byte == 0xff {
                        continue;
                    }
                    let bit = byte.trailing_ones();
                    *byte |= 1 << bit;
                    let block = first + (byte_i * 8 + bit as usize) as u64;
                    if block < self.sb.blocks_count as u64 {
                        found = Some(block);
                    }
                    break;
                }
                found.is_some()
            });
            if let Some(block) = found {
                self.bump_free(g, -1, 0);
                // A freshly allocated block reads as zeros, whoever gets it:
                // ext2 promises nothing about old contents, and callers rely
                // on holes and unwritten tails being zero.
                self.update_block(block, |page| {
                    page.fill(0);
                    true
                });
                return Ok(block);
            }
        }
        Err(VfsError::NoSpace)
    }

    fn alloc_inode(&self) -> VfsResult<u32> {
        let per_group = self.sb.inodes_per_group as u64;
        for g in 0..self.groups {
            let group = self.group(g);
            if group.free_inodes == 0 {
                continue;
            }
            // Group 0's bitmap starts with the reserved inodes; allocation
            // begins past them at s_first_ino.
            let start = if g == 0 {
                self.sb.first_ino as usize - 1
            } else {
                0
            };
            let mut found = None;
            self.update_block(group.inode_bitmap, |page| {
                for bit in start..self.sb.inodes_per_group as usize {
                    if page[bit / 8] & (1 << (bit % 8)) == 0 {
                        page[bit / 8] |= 1 << (bit % 8);
                        found = Some(g as u64 * per_group + bit as u64 + 1);
                        break;
                    }
                }
                found.is_some()
            });
            if let Some(ino) = found {
                assert!(
                    ino <= self.sb.inodes_count as u64,
                    "ext2 inode bitmap offered inode {ino} past the end"
                );
                self.bump_free(g, 0, -1);
                let (block, off) = self.inode_pos(ino as u32);
                let size = self.sb.inode_size as usize;
                self.update_block(block, |page| {
                    page[off..off + size].fill(0);
                    true
                });
                return Ok(ino as u32);
            }
        }
        Err(VfsError::NoSpace)
    }

    /// Give one block back to the group it came from: clear its bitmap bit,
    /// put the free count back. The contents are left alone - ext2 promises
    /// nothing about them, and [`Self::alloc_block`] zeroes a block before
    /// anyone gets it.
    fn free_block(&self, block: u64) {
        let per_group = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;
        let g = ((block - first) / per_group) as u32;
        let bit = (block - first) % per_group;
        let group = self.group(g);
        self.update_block(group.block_bitmap, |page| {
            page[(bit / 8) as usize] &= !(1u8 << (bit % 8));
            true
        });
        self.bump_free(g, 1, 0);
    }

    /// Free every block `data` maps - the direct ones, then whatever the two
    /// levels of indirection reach. The indirect blocks themselves go last,
    /// once nothing points through them. A zero pointer is a hole: nothing
    /// was ever allocated for it.
    ///
    /// A fast symlink has no blocks to free and must not be walked: its
    /// "pointers" are the *text* of the name it stands for, and a target that
    /// reads as a number would take blocks out of somebody else's file -
    /// or, when it does not read as a plausible one at all, walk off the end
    /// of the group table and take the kernel with it.
    fn free_blocks_of(&self, data: &InodeData) {
        if data.is_fast_symlink() {
            return;
        }
        let per_block = self.block_size() / 4;
        for entry in &data.block[..DIRECT_BLOCKS] {
            if *entry != 0 {
                self.free_block(*entry as u64);
            }
        }
        // Block 12 is single-indirect, block 13 double: same walk, one level
        // apart, which is what the flag says.
        for (double, indirect) in [(false, data.block[12]), (true, data.block[13])] {
            if indirect != 0 {
                self.free_indirect(indirect as u64, per_block, double);
            }
        }
    }

    /// Free what one indirect block points at, then the block itself. A
    /// double-indirect points at indirect blocks rather than data, so its
    /// contents are freed one level down: the same walk, one step deeper.
    fn free_indirect(&self, block: u64, per_block: usize, double: bool) {
        for i in 0..per_block {
            let Some(target) = self.ptr_at(block, i) else {
                continue;
            };
            if double {
                self.free_indirect(target, per_block, false);
            } else {
                self.free_block(target);
            }
        }
        self.free_block(block);
    }

    /// Release an inode whose last name is gone: its data blocks first, then
    /// its bitmap bit. Each block puts its own count back as it goes, so
    /// only the inode is counted here.
    fn free_inode(&self, ino: u32) {
        self.free_blocks_of(&self.read_inode(ino));
        let g = (ino - 1) / self.sb.inodes_per_group;
        let index = (ino - 1) % self.sb.inodes_per_group;
        let group = self.group(g);
        self.update_block(group.inode_bitmap, |page| {
            page[(index / 8) as usize] &= !(1u8 << (index % 8));
            true
        });
        self.bump_free(g, 0, 1);
    }

    /// Block and offset of inode `ino` inside its group's inode table.
    fn inode_pos(&self, ino: u32) -> (u64, usize) {
        let index = (ino - 1) % self.sb.inodes_per_group;
        let group = (ino - 1) / self.sb.inodes_per_group;
        let table = self.group(group).inode_table;
        let off = index as usize * self.sb.inode_size as usize;
        (
            table + (off / self.block_size()) as u64,
            off % self.block_size(),
        )
    }

    fn read_inode(&self, ino: u32) -> InodeData {
        let (block, off) = self.inode_pos(ino);
        let mut bytes = [0u8; INODE_PARSED];
        self.read_bytes(block, off, &mut bytes);
        InodeData::parse(&bytes)
    }

    fn write_inode(&self, ino: u32, data: &InodeData) {
        let (block, off) = self.inode_pos(ino);
        let mut bytes = [0u8; INODE_PARSED];
        data.store(&mut bytes);
        self.write_bytes(block, off, &bytes);
    }

    // ---- block mapping ----

    /// Pointer `i` of an indirect block; `None` for zero, i.e. unmapped.
    fn ptr_at(&self, block: u64, i: usize) -> Option<u64> {
        let mut bytes = [0u8; 4];
        self.read_bytes(block, i * 4, &mut bytes);
        let value = le32(&bytes, 0);
        (value != 0).then_some(value as u64)
    }

    fn put_ptr(&self, block: u64, i: usize, value: u32) {
        let mut bytes = [0u8; 4];
        set32(&mut bytes, 0, value);
        self.write_bytes(block, i * 4, &bytes);
    }

    /// Which disk block holds logical block `l` of a file with inode `d`.
    /// `None` is a hole or a block past what the triple of indirections maps.
    fn bmap_lookup(&self, d: &InodeData, l: u64) -> Option<u64> {
        let per_block = (self.block_size() / 4) as u64;
        let direct = DIRECT_BLOCKS as u64;
        if l < direct {
            (d.block[l as usize] != 0).then_some(d.block[l as usize] as u64)
        } else if l < direct + per_block {
            let indirect = (d.block[12] != 0).then_some(d.block[12] as u64)?;
            self.ptr_at(indirect, (l - direct) as usize)
        } else if l < direct + per_block + per_block * per_block {
            let indirect = (d.block[13] != 0).then_some(d.block[13] as u64)?;
            let rest = l - direct - per_block;
            let mid = self.ptr_at(indirect, (rest / per_block) as usize)?;
            self.ptr_at(mid, (rest % per_block) as usize)
        } else {
            None
        }
    }

    /// Map logical block `l` for writing, allocating anything missing - the
    /// data block, and any indirect block on the path to it. Returns the disk
    /// block and whether anything new was allocated, for i_blocks accounting.
    fn bmap_ensure(&self, d: &mut InodeData, l: u64) -> VfsResult<(u64, bool)> {
        let per_block = (self.block_size() / 4) as u64;
        let direct = DIRECT_BLOCKS as u64;
        let mut allocated = false;
        let target = if l < direct {
            if d.block[l as usize] == 0 {
                d.block[l as usize] = self.alloc_block()? as u32;
                allocated = true;
            }
            d.block[l as usize] as u64
        } else if l < direct + per_block {
            if d.block[12] == 0 {
                d.block[12] = self.alloc_block()? as u32;
                allocated = true;
            }
            self.ensure_slot(d.block[12] as u64, (l - direct) as usize, &mut allocated)?
        } else if l < direct + per_block + per_block * per_block {
            if d.block[13] == 0 {
                d.block[13] = self.alloc_block()? as u32;
                allocated = true;
            }
            let rest = l - direct - per_block;
            let mid = self.ensure_slot(
                d.block[13] as u64,
                (rest / per_block) as usize,
                &mut allocated,
            )?;
            self.ensure_slot(mid, (rest % per_block) as usize, &mut allocated)?
        } else {
            return Err(VfsError::FileTooBig);
        };
        Ok((target, allocated))
    }

    fn ensure_slot(&self, indirect: u64, i: usize, allocated: &mut bool) -> VfsResult<u64> {
        match self.ptr_at(indirect, i) {
            Some(block) => Ok(block),
            None => {
                let block = self.alloc_block()?;
                self.put_ptr(indirect, i, block as u32);
                *allocated = true;
                Ok(block)
            }
        }
    }

    // ---- directories ----

    fn dir_lookup(&self, dir: &InodeData, name: &str) -> VfsResult<Option<u32>> {
        let block_size = self.block_size();
        for l in 0..dir.size as u64 / block_size as u64 {
            let block = self
                .bmap_lookup(dir, l)
                .expect("a directory block was never mapped");
            let mut page = vec![0u8; block_size];
            self.read_bytes(block, 0, &mut page);
            if let Some(ino) = dir_find(&page, name) {
                return Ok(Some(ino));
            }
        }
        Ok(None)
    }

    /// Every entry of a directory, name and file type, in on-disk order.
    /// The readdir behind `ls`: same block walk as `dir_lookup`, but nothing
    /// stops early.
    fn dir_entries(&self, dir: &InodeData) -> VfsResult<Vec<(String, NodeKind)>> {
        let block_size = self.block_size();
        let mut found = Vec::new();
        for l in 0..dir.size as u64 / block_size as u64 {
            let block = self
                .bmap_lookup(dir, l)
                .expect("a directory block was never mapped");
            let mut page = vec![0u8; block_size];
            self.read_bytes(block, 0, &mut page);
            dir_walk(&page, |_ino, ftype, name| {
                // A name that is not text names nothing we can list; skip it
                // rather than fail the whole readdir.
                let Ok(name) = core::str::from_utf8(name) else {
                    return true;
                };
                let kind = match ftype {
                    FT_DIR => NodeKind::Dir,
                    FT_SYMLINK => NodeKind::Symlink,
                    FT_CHRDEV => NodeKind::CharDev,
                    _ => NodeKind::File,
                };
                found.push((String::from(name), kind));
                true
            });
        }
        Ok(found)
    }

    /// Append `name -> child` to a directory. The caller owns the inode
    /// rewrite; `dir` is mutated in place so size, block count and flags stay
    /// truthful.
    ///
    /// An indexed directory takes the htree path: the entry has to land in
    /// the leaf the index names for its hash, and every leaf the tree gains
    /// has to be registered in the index, or the tree stops describing the
    /// entries and a Linux that later mounts the image walks a lie. A linear
    /// directory just carves slack; its flag stays unset, and a Linux that
    /// wants it indexed converts it on its own.
    fn dir_add(&self, dir: &mut InodeData, name: &str, child: u32, ftype: u8) -> VfsResult<()> {
        assert!(
            !name.is_empty() && name.len() <= 255,
            "ext2 names are 1..=255 bytes"
        );
        if dir.flags & INODE_INDEX_FL != 0 {
            self.dir_add_indexed(dir, name, child, ftype)
        } else {
            self.dir_add_linear(dir, name, child, ftype)
        }
    }

    /// The linear insert: carve slack out of existing blocks first and grow
    /// the directory by one block only when nothing fits.
    fn dir_add_linear(
        &self,
        dir: &mut InodeData,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let blocks = dir.size as u64 / block_size as u64;
        for l in 0..blocks {
            let block = self
                .bmap_lookup(dir, l)
                .expect("a directory block was never mapped");
            if self.update_block(block, |page| dir_try_insert(page, name, child, ftype)) {
                return Ok(());
            }
        }
        let (block, _allocated) = self.bmap_ensure(dir, blocks)?;
        self.update_block(block, |page| {
            write_dirent(page, 0, child, page.len(), name, ftype);
            true
        });
        dir.size += block_size as u32;
        dir.blocks += (block_size / 512) as u32;
        Ok(())
    }

    /// Drop `name` from `dir` and report the inode it named. The entry's
    /// space goes to the one before it, so the block stays walkable without a
    /// hole in its middle; an entry with no predecessor is emptied instead,
    /// which is the zero inode a walk skips and a later insert reuses.
    ///
    /// No index bookkeeping: a removal never moves an entry between leaves
    /// and never changes a hash, so the index keeps describing the tree
    /// exactly as it did - which is what Linux's own delete does. The linear
    /// walk finds the entry wherever it lives (index blocks hide behind
    /// whole-block fake dirents, and block 0 of an indexed directory holds
    /// only "." and "..").
    fn dir_remove(&self, dir: &mut InodeData, name: &str) -> VfsResult<u32> {
        let block_size = self.block_size();
        for l in 0..dir.size as u64 / block_size as u64 {
            let block = self
                .bmap_lookup(dir, l)
                .expect("a directory block was never mapped");
            let mut found = None;
            self.update_block(block, |page| dir_take(page, name, &mut found));
            if let Some(ino) = found {
                return Ok(ino);
            }
        }
        Err(VfsError::NoEntry)
    }

    /// A brand-new inode: reserved fields zeroed, mode and links set, and for
    /// a directory a first block holding "." and "..".
    fn new_inode(&self, kind: NodeKind, parent: u32) -> VfsResult<u32> {
        let ino = self.alloc_inode()?;
        let block_size = self.block_size();
        let mut data = InodeData {
            mode: match kind {
                NodeKind::Dir => S_IFDIR | 0o755,
                // A link's permissions are not consulted by anything - Linux
                // ignores them - but every real ext2 link carries 0777, and a
                // filesystem tool that reads this back should see what it
                // expects.
                NodeKind::Symlink => S_IFLNK | 0o777,
                _ => S_IFREG | 0o644,
            },
            flags: 0,
            size: 0,
            links: 1,
            blocks: 0,
            block: [0; 15],
        };
        if kind == NodeKind::Dir {
            let block = self.alloc_block()?;
            data.block[0] = block as u32;
            data.size = block_size as u32;
            data.blocks = (block_size / 512) as u32;
            data.links = 2;
            self.update_block(block, |page| dir_init(page, ino, parent));
        }
        self.write_inode(ino, &data);
        Ok(ino)
    }
}

// ---- directory entry encoding, pure over a block image ----

/// Full length of an entry with a `name_len`-byte name: header plus name,
/// padded to a 4-byte boundary.
fn dirent_len(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len).next_multiple_of(4)
}

/// Walk the entries of one directory block. Zero-inode slots are holes and
/// are skipped; `visit` returning false stops the walk.
fn dir_walk(page: &[u8], mut visit: impl FnMut(u32, u8, &[u8]) -> bool) {
    let mut at = 0;
    while at < page.len() {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= page.len(),
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino != 0 {
            let name = &page[at + 8..at + 8 + page[at + 6] as usize];
            if !visit(ino, page[at + 7], name) {
                return;
            }
        }
        at += rec;
    }
}

fn dir_find(page: &[u8], want: &str) -> Option<u32> {
    let mut hit = None;
    dir_walk(page, |ino, _type, name| {
        if name == want.as_bytes() {
            hit = Some(ino);
            false
        } else {
            true
        }
    });
    hit
}

fn write_dirent(page: &mut [u8], at: usize, ino: u32, rec: usize, name: &str, ftype: u8) {
    set32(page, at, ino);
    set16(page, at + 4, rec as u16);
    page[at + 6] = name.len() as u8;
    page[at + 7] = ftype;
    page[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
}

/// Try to fit one more entry into a directory block, returning whether it
/// went in. Two sources of room: a hole wide enough for the whole entry, or
/// slack after a live entry wide enough for the entry plus a header's worth
/// of its own hole.
fn dir_try_insert(page: &mut [u8], name: &str, child: u32, ftype: u8) -> bool {
    let needed = dirent_len(name.len());
    let mut at = 0;
    while at < page.len() {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= page.len(),
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino == 0 {
            if rec >= needed {
                let rest = rec - needed;
                let entry_rec = if rest >= DIRENT_HEADER { needed } else { rec };
                write_dirent(page, at, child, entry_rec, name, ftype);
                if entry_rec < rec {
                    // What remains of the hole stays a hole, reachable from
                    // the new entry's end.
                    set32(page, at + entry_rec, 0);
                    set16(page, at + entry_rec + 4, (rec - entry_rec) as u16);
                }
                return true;
            }
        } else {
            let own = dirent_len(page[at + 6] as usize);
            if rec - own >= needed {
                set16(page, at + 4, own as u16);
                write_dirent(page, at + own, child, rec - own, name, ftype);
                return true;
            }
        }
        at += rec;
    }
    false
}

/// Take the entry called `name` out of one directory block, reporting the
/// inode it named through `found`. Its bytes go to the entry in front of it,
/// which is how ext2 keeps a block free of holes; one at the very front has
/// nothing in front of it, so it is emptied in place instead.
fn dir_take(page: &mut [u8], name: &str, found: &mut Option<u32>) -> bool {
    let mut at = 0;
    let mut prev: Option<usize> = None;
    while at < page.len() {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= page.len(),
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino != 0 {
            if &page[at + 8..at + 8 + page[at + 6] as usize] == name.as_bytes() {
                *found = Some(ino);
                match prev {
                    // The predecessor's record grows over the whole gap.
                    Some(p) => set16(page, p + 4, (at + rec - p) as u16),
                    None => set32(page, at, 0),
                }
                return true;
            }
            prev = Some(at);
        }
        at += rec;
    }
    false
}

/// Initialise a fresh directory block with "." and "..", the latter taking
/// the rest of the block.
fn dir_init(page: &mut [u8], dir_ino: u32, parent_ino: u32) -> bool {
    write_dirent(page, 0, dir_ino, dirent_len(1), ".", FT_DIR);
    write_dirent(
        page,
        dirent_len(1),
        parent_ino,
        page.len() - dirent_len(1),
        "..",
        FT_DIR,
    );
    true
}

// ---- the directory hash (htree) ----
//
// An indexed directory sorts its entries by a hash of the name. The hash is
// not a local invention: a Linux that mounts this image later has to agree
// with every value this driver ever wrote into an index, byte for byte, or
// its lookups land in the wrong leaf and names vanish. The transforms below
// are line-for-line the ones in Linux's `fs/ext4/hash.c` - the legacy ext2
// hash, a cut-down MD4 and a TEA - fed by the same seed from the superblock.

/// One round of the cut-down MD4: `a += f(b, c, d) + x`, rotated.
fn md4_round(
    f: impl Fn(u32, u32, u32) -> u32,
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    x: u32,
    s: u32,
) -> u32 {
    a.wrapping_add(f(b, c, d)).wrapping_add(x).rotate_left(s)
}

/// Half of MD4 - rounds 1..3 over eight words, the whole of what the htree
/// needs of the real thing.
fn half_md4_transform(buf: &mut [u32; 4], input: &[u32; 8]) {
    // The round constants of MD4's middle and last rounds.
    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);

    a = md4_round(f, a, b, c, d, input[0], 3);
    d = md4_round(f, d, a, b, c, input[1], 7);
    c = md4_round(f, c, d, a, b, input[2], 11);
    b = md4_round(f, b, c, d, a, input[3], 19);
    a = md4_round(f, a, b, c, d, input[4], 3);
    d = md4_round(f, d, a, b, c, input[5], 7);
    c = md4_round(f, c, d, a, b, input[6], 11);
    b = md4_round(f, b, c, d, a, input[7], 19);

    a = md4_round(g, a, b, c, d, input[1].wrapping_add(K2), 3);
    d = md4_round(g, d, a, b, c, input[3].wrapping_add(K2), 5);
    c = md4_round(g, c, d, a, b, input[5].wrapping_add(K2), 9);
    b = md4_round(g, b, c, d, a, input[7].wrapping_add(K2), 13);
    a = md4_round(g, a, b, c, d, input[0].wrapping_add(K2), 3);
    d = md4_round(g, d, a, b, c, input[2].wrapping_add(K2), 5);
    c = md4_round(g, c, d, a, b, input[4].wrapping_add(K2), 9);
    b = md4_round(g, b, c, d, a, input[6].wrapping_add(K2), 13);

    a = md4_round(h, a, b, c, d, input[3].wrapping_add(K3), 3);
    d = md4_round(h, d, a, b, c, input[7].wrapping_add(K3), 9);
    c = md4_round(h, c, d, a, b, input[2].wrapping_add(K3), 11);
    b = md4_round(h, b, c, d, a, input[6].wrapping_add(K3), 15);
    a = md4_round(h, a, b, c, d, input[1].wrapping_add(K3), 3);
    d = md4_round(h, d, a, b, c, input[5].wrapping_add(K3), 9);
    c = md4_round(h, c, d, a, b, input[0].wrapping_add(K3), 11);
    b = md4_round(h, b, c, d, a, input[4].wrapping_add(K3), 15);

    for (word, sum) in buf.iter_mut().zip([a, b, c, d]) {
        *word = word.wrapping_add(sum);
    }
}

/// The TEA round the htree uses as a second mixing function.
fn tea_transform(buf: &mut [u32; 4], input: &[u32; 4]) {
    const DELTA: u32 = 0x9e37_79b9;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (input[0], input[1], input[2], input[3]);
    let mut sum = 0u32;
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(
            ((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)),
        );
        b1 = b1.wrapping_add(
            ((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)),
        );
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// The original ext2 hash, before there was a tree to sort with.
fn dx_hack_hash(name: &[u8], unsigned: bool) -> u32 {
    let (mut hash0, mut hash1) = (0x12a3_fe2d_u32, 0x37ab_e8f9_u32);
    for &byte in name {
        let c = if unsigned {
            byte as u32
        } else {
            byte as i8 as i32 as u32
        };
        let mut hash = hash1.wrapping_add(hash0 ^ c.wrapping_mul(7152373));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// Pack name bytes into `out.len()` words the way the transforms consume
/// them. `unsigned` decides whether a byte above 127 enters the word
/// sign-extended or zero-filled - the one place the two hash flavors differ.
fn str2hashbuf(msg: &[u8], out: &mut [u32], unsigned: bool) {
    let len = msg.len().min(out.len() * 4);
    let mut pad = len as u32 | (len as u32) << 8;
    pad |= pad << 16;
    let mut word = 0;
    let mut at = 0;
    while len - at >= 4 {
        out[word] = (byte_of(msg[at], unsigned) << 24)
            .wrapping_add((byte_of(msg[at + 1], unsigned)) << 16)
            .wrapping_add((byte_of(msg[at + 2], unsigned)) << 8)
            .wrapping_add(byte_of(msg[at + 3], unsigned));
        word += 1;
        at += 4;
    }
    let mut val = pad;
    for b in &msg[at..len] {
        val = byte_of(*b, unsigned).wrapping_add(val << 8);
    }
    if word < out.len() {
        out[word] = val;
        word += 1;
    }
    out[word..].fill(pad);
}

fn byte_of(b: u8, unsigned: bool) -> u32 {
    if unsigned {
        b as u32
    } else {
        b as i8 as i32 as u32
    }
}

/// The hash an index sorts a name by: the algorithm the directory's root
/// block names, salted with the superblock's seed. `None` for a version this
/// driver does not implement (Siphash, the casefolding flavor).
///
/// Stored versions 0..=2 are the signed variants; the superblock's unsigned
/// flag renumbers them onto the unsigned ones (+3). Every hash is even - the
/// low bit is reserved to mark a leaf that continues the previous one.
fn dx_hash_name(name: &[u8], version: u8, seed: [u32; 4], unsigned: bool) -> Option<u32> {
    let version = match version {
        0..=2 if unsigned => version + 3,
        v => v,
    };
    // MD4's initialization vector: what the transforms start from when the
    // superblock never recorded a seed.
    let mut buf = if seed == [0; 4] {
        [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476]
    } else {
        seed
    };
    let hash = match version {
        0 | 3 => dx_hack_hash(name, version == 3),
        1 | 4 => {
            let mut input = [0u32; 8];
            let mut rest = name;
            while !rest.is_empty() {
                str2hashbuf(rest, &mut input, version == 4);
                half_md4_transform(&mut buf, &input);
                rest = &rest[rest.len().min(32)..];
            }
            buf[1]
        }
        2 | 5 => {
            let mut input = [0u32; 4];
            let mut rest = name;
            while !rest.is_empty() {
                str2hashbuf(rest, &mut input, version == 5);
                tea_transform(&mut buf, &input);
                rest = &rest[rest.len().min(16)..];
            }
            buf[0]
        }
        _ => return None,
    };
    let mut hash = hash & !1;
    if hash == HTREE_EOF_32BIT << 1 {
        hash = (HTREE_EOF_32BIT - 1) << 1;
    }
    Some(hash)
}

// ---- the directory index (dx) ----
//
// Block 0 of an indexed directory is the index root: "." then ".." - the
// latter's record length stretched over the whole of what follows - then a
// small info structure at offset 24, then the entries. Each entry pairs a
// hash with the logical block of the leaf (or of the next index level down)
// that owns every name hashing below it and above the entry before. Entry 0
// is not a real entry: its two halves are the count and the limit, and its
// block field names the first leaf. An intermediate index block is the same
// shape a level down, introduced by one fake dirent covering the whole
// block, with its entries right behind the 8-byte dirent header. Leaves are
// ordinary dirent blocks - which is why every linear walk of an indexed
// directory works unchanged.

/// The info structure every index root carries at offset 24.
struct DxRoot {
    hash_version: u8,
    indirect_levels: u8,
    /// Where the entries begin: right after the info block, whose length is
    /// itself recorded (and always 8 in every tree anyone has made).
    entries_at: usize,
}

impl DxRoot {
    /// Parse the root's info block, or `None` for an index this driver does
    /// not walk. The caller falls back to a linear insert and lets the flag
    /// go, which is what Linux's own checksum-less fallback does.
    fn parse(page: &[u8]) -> Option<DxRoot> {
        if le32(page, 24) != 0 {
            return None;
        }
        let hash_version = page[28];
        let info_length = page[29] as usize;
        let indirect_levels = page[30];
        let unused_flags = page[31];
        if info_length != 8 || unused_flags & 1 != 0 {
            return None;
        }
        if hash_version > 2 || indirect_levels > HTREE_MAX_LEVELS {
            return None;
        }
        Some(DxRoot {
            hash_version,
            indirect_levels,
            entries_at: 24 + info_length,
        })
    }
}

/// One level of the path a hash takes through the index: the logical block
/// holding that level's entries and the slot of the entry that owns the
/// hash - the one whose leaf, or whose child index block, the search
/// descends into.
#[derive(Clone, Copy)]
struct DxFrame {
    block: u64,
    entries_at: usize,
    at: usize,
}

/// The path down the index: the root, an optional intermediate level, and
/// the logical block of the leaf that owns the hash.
struct DxPath {
    root: DxFrame,
    node: Option<DxFrame>,
    leaf: u64,
}

fn dx_count(page: &[u8], entries_at: usize) -> usize {
    le16(page, entries_at + 2) as usize
}

fn dx_limit(page: &[u8], entries_at: usize) -> usize {
    le16(page, entries_at) as usize
}

fn dx_entry_hash(page: &[u8], entries_at: usize, i: usize) -> u32 {
    le32(page, entries_at + i * 8)
}

/// The logical block an entry points at. The top four bits are reserved and
/// never part of a block number.
fn dx_entry_block(page: &[u8], entries_at: usize, i: usize) -> u32 {
    le32(page, entries_at + i * 8 + 4) & 0x0fff_ffff
}

fn dx_set_count(page: &mut [u8], entries_at: usize, count: usize) {
    set16(page, entries_at + 2, count as u16);
}

/// How many entries a root can hold: the block less ".", ".." and the info
/// structure, one entry per eight bytes.
fn dx_root_limit(block_size: usize, info_length: usize) -> usize {
    (block_size - dirent_len(1) - dirent_len(2) - info_length) / 8
}

/// How many entries an intermediate index block can hold: the block less the
/// fake dirent that stands in for the header.
fn dx_node_limit(block_size: usize) -> usize {
    (block_size - DIRENT_HEADER) / 8
}

/// Shift the index entries after slot `at` up by one and put
/// `hash -> block` into the gap: the entry a new leaf registers under.
fn dx_insert_entry(page: &mut [u8], entries_at: usize, at: usize, hash: u32, block: u32) {
    let count = dx_count(page, entries_at);
    let from = entries_at + (at + 1) * 8;
    page.copy_within(from..entries_at + count * 8, from + 8);
    set32(page, from, hash);
    set32(page, from + 4, block);
    dx_set_count(page, entries_at, count + 1);
}

impl Core {
    /// Walk the index to the leaf that owns `hash`, validating every level
    /// against the shape it must have. `Err` is "not a tree this driver
    /// walks" - a corrupt or foreign index, which the insert path answers by
    /// dropping the flag and going linear, as Linux does for a tree it
    /// cannot trust and cannot checksum.
    fn dx_probe(&self, dir: &InodeData, hash: u32) -> Result<DxPath, ()> {
        let block_size = self.block_size();
        if dir.size == 0 {
            return Err(());
        }
        let mut page = vec![0u8; block_size];
        let root_disk = self.bmap_lookup(dir, 0).ok_or(())?;
        self.read_bytes(root_disk, 0, &mut page);
        let root = DxRoot::parse(&page).ok_or(())?;
        if dx_limit(&page, root.entries_at) != dx_root_limit(block_size, 8) {
            return Err(());
        }

        let mut frames: Vec<DxFrame> = Vec::new();
        let mut frame = DxFrame {
            block: 0,
            entries_at: root.entries_at,
            at: 0,
        };
        let leaf = loop {
            let count = dx_count(&page, frame.entries_at);
            if count == 0 || count > dx_limit(&page, frame.entries_at) {
                return Err(());
            }
            // Binary search the real entries (1..count) for the last one
            // hashing at or below the target; entry 0 - the count-limit
            // alias - is the floor, and its block names the first leaf.
            let mut p = 1usize;
            let mut q = count - 1;
            while p <= q {
                let m = p + (q - p) / 2;
                if dx_entry_hash(&page, frame.entries_at, m) > hash {
                    q = m - 1;
                } else {
                    p = m + 1;
                }
            }
            frame.at = p - 1;
            let next = dx_entry_block(&page, frame.entries_at, frame.at) as u64;
            frames.push(frame);
            if frames.len() as u8 > root.indirect_levels {
                break next;
            }
            // One more index level to descend - its block must not point
            // back at block 0, which is the cycle a damaged tree makes.
            if next == 0 {
                return Err(());
            }
            let node_disk = self.bmap_lookup(dir, next).ok_or(())?;
            self.read_bytes(node_disk, 0, &mut page);
            frame = DxFrame {
                block: next,
                entries_at: DIRENT_HEADER,
                at: 0,
            };
            if dx_limit(&page, frame.entries_at) != dx_node_limit(block_size) {
                return Err(());
            }
        };
        Ok(DxPath {
            root: frames[0],
            node: frames.get(1).copied(),
            leaf,
        })
    }

    /// Append `name -> child` to an indexed directory.
    fn dir_add_indexed(
        &self,
        dir: &mut InodeData,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let root_disk = match self.bmap_lookup(dir, 0) {
            Some(block) => block,
            None => return self.dir_add_fallback(dir, name, child, ftype),
        };
        let mut root_page = vec![0u8; block_size];
        self.read_bytes(root_disk, 0, &mut root_page);
        let root = match DxRoot::parse(&root_page) {
            Some(root) => root,
            None => return self.dir_add_fallback(dir, name, child, ftype),
        };
        let Some(hash) = dx_hash_name(
            name.as_bytes(),
            root.hash_version,
            self.sb.hash_seed,
            self.sb.hash_unsigned,
        ) else {
            return self.dir_add_fallback(dir, name, child, ftype);
        };

        loop {
            let path = match self.dx_probe(dir, hash) {
                Ok(path) => path,
                Err(()) => return self.dir_add_fallback(dir, name, child, ftype),
            };
            let leaf_disk = self.bmap_lookup(dir, path.leaf).ok_or(VfsError::NoSpace)?;
            let mut leaf_page = vec![0u8; block_size];
            self.read_bytes(leaf_disk, 0, &mut leaf_page);
            if dir_try_insert(&mut leaf_page, name, child, ftype) {
                self.write_bytes(leaf_disk, 0, &leaf_page);
                return Ok(());
            }

            // The leaf is full. Splitting it adds one entry to the index
            // level directly above - the root for a one-level tree, the
            // intermediate node for a two-level one - possible only while
            // that level has a slot. With the level above full too, the
            // answer is the ENOSPC Linux itself gives ("directory index
            // full"); disk space has nothing to do with it.
            let parent = path.node.unwrap_or(path.root);
            let parent_disk = self
                .bmap_lookup(dir, parent.block)
                .ok_or(VfsError::NoSpace)?;
            let mut parent_page = vec![0u8; block_size];
            self.read_bytes(parent_disk, 0, &mut parent_page);
            if dx_count(&parent_page, parent.entries_at) < dx_limit(&parent_page, parent.entries_at)
            {
                return self.dx_split_leaf(
                    dir,
                    name,
                    child,
                    ftype,
                    root.hash_version,
                    hash,
                    path.leaf,
                    parent,
                );
            }
            match path.node {
                // A one-level tree whose root is full is the end of the
                // road: without the `largedir` feature neither this driver
                // nor Linux grows the tree deeper than one node.
                None => return Err(VfsError::NoSpace),
                Some(node) => {
                    // The node is full; only the root can take the sibling,
                    // and it needs a free slot of its own. The root page is
                    // re-read: an earlier pass of this loop may have split
                    // it since.
                    self.read_bytes(root_disk, 0, &mut root_page);
                    if dx_count(&root_page, path.root.entries_at)
                        >= dx_limit(&root_page, path.root.entries_at)
                    {
                        return Err(VfsError::NoSpace);
                    }
                    self.dx_split_node(
                        dir,
                        root_disk,
                        &mut root_page,
                        path.root,
                        parent_disk,
                        &mut parent_page,
                        node,
                    )?;
                    // The tree changed shape under the search; find the
                    // leaf again.
                    continue;
                }
            }
        }
    }

    /// Split a full leaf: append a fresh one at the end of the directory,
    /// deal the entries out by hash so neither half is over half full, and
    /// register the new leaf in the index level above (`parent`) right after
    /// the old one. Then put the new entry into whichever half its hash
    /// belongs to.
    fn dx_split_leaf(
        &self,
        dir: &mut InodeData,
        name: &str,
        child: u32,
        ftype: u8,
        hash_version: u8,
        name_hash: u32,
        leaf_logical: u64,
        parent: DxFrame,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let leaf_disk = self
            .bmap_lookup(dir, leaf_logical)
            .ok_or(VfsError::NoSpace)?;
        let mut leaf_page = vec![0u8; block_size];
        self.read_bytes(leaf_disk, 0, &mut leaf_page);
        let parent_disk = self
            .bmap_lookup(dir, parent.block)
            .ok_or(VfsError::NoSpace)?;
        let mut parent_page = vec![0u8; block_size];
        self.read_bytes(parent_disk, 0, &mut parent_page);

        // The new leaf is the block after every current one.
        let new_logical = dir.size as u64 / block_size as u64;
        let new_disk = self.bmap_ensure(dir, new_logical)?.0;
        dir.size += block_size as u32;
        dir.blocks += (block_size / 512) as u32;
        let mut new_page = vec![0u8; block_size];

        // Map the live entries and sort them by hash.
        let mut map: Vec<(u32, usize, usize)> = Vec::new();
        let mut at = 0;
        while at + DIRENT_HEADER <= leaf_page.len() {
            let rec = le16(&leaf_page, at + 4) as usize;
            assert!(
                rec >= DIRENT_HEADER && at + rec <= leaf_page.len(),
                "corrupt ext2 directory: entry at {at} claims {rec} bytes"
            );
            let ino = le32(&leaf_page, at);
            let name_len = leaf_page[at + 6] as usize;
            if ino != 0 && name_len != 0 {
                let hash = dx_hash_name(
                    &leaf_page[at + 8..at + 8 + name_len],
                    hash_version,
                    self.sb.hash_seed,
                    self.sb.hash_unsigned,
                )
                .unwrap_or(0);
                map.push((hash, at, rec));
            }
            at += rec;
        }
        map.sort_by_key(|entry| entry.0);

        // Take entries from the top while half a block still holds them, so
        // both halves come out with room to spare; if even the first does
        // not fit, fall back to splitting by count.
        let count = map.len();
        let (mut size, mut moved) = (0usize, 0usize);
        for entry in map.iter().rev() {
            if size + entry.2 / 2 > block_size / 2 {
                break;
            }
            size += entry.2;
            moved += 1;
        }
        let split = if moved > 0 { count - moved } else { count / 2 };
        let split = split.max(1);
        let hash2 = map[split].0;
        // When equal hashes straddle the split, the index key is bumped by
        // one: its low bit marks a leaf that *continues* the previous one,
        // and a Linux looking for either half finds both.
        let continued = u32::from(map[split - 1].0 == hash2);

        dx_move_dirents(&mut leaf_page, &mut new_page, &map[split..]);
        dx_pack_dirents(&mut leaf_page);
        dx_insert_entry(
            &mut parent_page,
            parent.entries_at,
            parent.at,
            hash2.wrapping_add(continued),
            new_logical as u32,
        );

        // Which half takes the new entry: the one its hash belongs in. Both
        // halves are at most half full, so the insert cannot fail - but a
        // refused insert tries the other half rather than leave the name off
        // a tree that already lists both leaves.
        let into_new = name_hash >= hash2;
        let mut placed = if into_new {
            dir_try_insert(&mut new_page, name, child, ftype)
        } else {
            dir_try_insert(&mut leaf_page, name, child, ftype)
        };
        if !placed {
            placed = if into_new {
                dir_try_insert(&mut leaf_page, name, child, ftype)
            } else {
                dir_try_insert(&mut new_page, name, child, ftype)
            };
        }
        if !placed {
            return Err(VfsError::NoSpace);
        }
        self.write_bytes(leaf_disk, 0, &leaf_page);
        self.write_bytes(new_disk, 0, &new_page);
        self.write_bytes(parent_disk, 0, &parent_page);
        Ok(())
    }

    /// Split a full intermediate index node. The caller has checked the root
    /// has a slot: the node's upper half of entries moves to a fresh block,
    /// and the root's entry naming the old node gains a sibling for the new
    /// one, keyed by the first hash that moved.
    fn dx_split_node(
        &self,
        dir: &mut InodeData,
        root_disk: u64,
        root_page: &mut [u8],
        root_frame: DxFrame,
        node_disk: u64,
        node_page: &mut [u8],
        node_frame: DxFrame,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let icount = dx_count(node_page, node_frame.entries_at);
        let icount1 = icount / 2;
        let icount2 = icount - icount1;
        let hash2 = dx_entry_hash(node_page, node_frame.entries_at, icount1);

        let new_logical = dir.size as u64 / block_size as u64;
        let new_disk = self.bmap_ensure(dir, new_logical)?.0;
        dir.size += block_size as u32;
        dir.blocks += (block_size / 512) as u32;

        // The new node: one fake dirent covering the whole block, entries
        // behind the 8-byte header. The upper half moves over; entry 0 of
        // the copy is then overwritten by the count-limit pair, its block
        // field left naming the first leaf of the upper half - exactly the
        // alias shape a probe expects to find.
        let mut new_page = vec![0u8; block_size];
        set16(&mut new_page, 4, block_size as u16);
        new_page[DIRENT_HEADER..DIRENT_HEADER + 8 * icount2].copy_from_slice(
            &node_page[node_frame.entries_at + 8 * icount1..node_frame.entries_at + 8 * icount],
        );
        set16(
            &mut new_page,
            DIRENT_HEADER,
            dx_node_limit(block_size) as u16,
        );
        dx_set_count(&mut new_page, DIRENT_HEADER, icount2);
        // The old node keeps the lower half.
        dx_set_count(node_page, node_frame.entries_at, icount1);
        // The root names the new node right after the old one.
        dx_insert_entry(
            root_page,
            root_frame.entries_at,
            root_frame.at,
            hash2,
            new_logical as u32,
        );

        self.write_bytes(node_disk, 0, node_page);
        self.write_bytes(new_disk, 0, &new_page);
        self.write_bytes(root_disk, 0, root_page);
        Ok(())
    }

    /// An index this driver cannot walk - foreign, or corrupt, or a hash
    /// version it has no code for. Drop the flag and insert linearly, the
    /// way Linux's own checksum-less fallback does. The dead index bytes sit
    /// in the slack of `..` (root) or of a fake dirent covering the whole
    /// block (node), which linear walkers skip, so the directory stays
    /// exactly as walkable as before - just no longer sorted.
    fn dir_add_fallback(
        &self,
        dir: &mut InodeData,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        dir.flags &= !INODE_INDEX_FL;
        self.dir_add_linear(dir, name, child, ftype)
    }
}

/// Move the mapped entries into a fresh leaf, packed to their minimal
/// lengths, the last one's record running to the end of the block. Each
/// source slot is emptied - inode zeroed, name wiped - so the compaction of
/// what stays never sees it.
fn dx_move_dirents(from: &mut [u8], to: &mut [u8], map: &[(u32, usize, usize)]) {
    let mut at = 0;
    let last = map.len().saturating_sub(1);
    for (i, &(_hash, offs, size)) in map.iter().enumerate() {
        let own = dirent_len(from[offs + 6] as usize);
        to[at..at + own].copy_from_slice(&from[offs..offs + own]);
        set16(to, at + 4, own as u16);
        if i == last {
            set16(to, at + 4, (to.len() - at) as u16);
        }
        set32(from, offs, 0);
        for b in &mut from[offs + 6..offs + size] {
            *b = 0;
        }
        at += own;
    }
}

/// Compact the live entries of a leaf to their minimal lengths at the front
/// of the block; the last one's record runs to the end, as a walker expects.
fn dx_pack_dirents(page: &mut [u8]) {
    let (mut to, mut last) = (0usize, 0usize);
    let mut at = 0;
    while at + DIRENT_HEADER <= page.len() {
        let rec = le16(page, at + 4) as usize;
        if rec < DIRENT_HEADER || at + rec > page.len() {
            break;
        }
        let ino = le32(page, at);
        let name_len = page[at + 6] as usize;
        if ino != 0 && name_len != 0 {
            let own = dirent_len(name_len);
            page.copy_within(at..at + own, to);
            set16(page, to + 4, own as u16);
            last = to;
            to += own;
        }
        at += rec;
    }
    set16(page, last + 4, (page.len() - last) as u16);
}

// ---- the VFS seam ----

/// An ext2 filesystem mounted through the VFS.
pub struct Ext2Fs {
    core: Arc<Core>,
}

impl FileSystem for Ext2Fs {
    fn root(&self) -> Arc<dyn Inode> {
        self.inode_of(ROOT_INO)
    }

    fn name(&self) -> &'static str {
        "ext2"
    }

    fn read_only(&self) -> bool {
        self.core
            .read_only
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    fn set_read_only(&self, read_only: bool) {
        self.core.set_read_only(read_only);
    }

    fn statfs(&self) -> Option<crate::vfs::FsStats> {
        // The free counters live on the group descriptors, the same place
        // every allocation bumps them (`bump_free`), so a df right after a
        // write storm reads what is actually on the disk.
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        for g in 0..self.core.groups {
            let desc = self.core.group(g);
            free_blocks += desc.free_blocks as u64;
            free_inodes += desc.free_inodes as u64;
        }
        Some(crate::vfs::FsStats {
            bsize: self.core.block_size() as u64,
            blocks: self.core.sb.blocks_count as u64,
            bfree: free_blocks,
            bavail: free_blocks,
            files: self.core.sb.inodes_count as u64,
            ffree: free_inodes,
        })
    }
}

impl Ext2Fs {
    /// Mount the filesystem on `dev`, validating the superblock. The device
    /// must already be quiescent; all state lives on the disk plus the
    /// write-through cache.
    pub fn mount(dev: Arc<dyn BlockDev>) -> Result<Ext2Fs, &'static str> {
        Ok(Ext2Fs {
            core: Arc::new(Core::mount(dev)?),
        })
    }

    pub fn inode_of(&self, ino: u32) -> Arc<dyn Inode> {
        critical_section::with(|cs| {
            let mut inodes = self.core.inodes.borrow(cs).borrow_mut();
            if let Some(cached) = inodes.get(&ino) {
                return cached.clone();
            }
            let inode = Arc::new(Ext2Ino {
                fs: self.core.clone(),
                ino,
            });
            inodes.insert(ino, inode.clone());
            inode
        })
    }
}

/// One inode of one ext2 mount: a pure handle. The on-disk header is read
/// through the cache on every operation, so two handles to the same inode
/// can never disagree, and there is nothing here to go stale.
pub struct Ext2Ino {
    fs: Arc<Core>,
    ino: u32,
}

impl Ext2Ino {
    fn wrap(&self, ino: u32) -> Arc<dyn Inode> {
        critical_section::with(|cs| {
            let mut inodes = self.fs.inodes.borrow(cs).borrow_mut();
            if let Some(cached) = inodes.get(&ino) {
                return cached.clone();
            }
            let inode = Arc::new(Ext2Ino {
                fs: self.fs.clone(),
                ino,
            });
            inodes.insert(ino, inode.clone());
            inode
        })
    }

    fn require_dir(&self) -> VfsResult<()> {
        if self.fs.read_inode(self.ino).kind() == NodeKind::Dir {
            Ok(())
        } else {
            Err(VfsError::NotDir)
        }
    }
}

impl Inode for Ext2Ino {
    fn metadata(&self) -> Metadata {
        let data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        Metadata {
            ino: self.ino as Ino,
            kind,
            // The directory's inode size - the byte length of its dirent
            // blocks - is what Linux's own ext2 reports, and what `ls -l`
            // prints. Zeroing it made every directory read as empty of
            // content as a stat could suggest.
            size: data.size as u64,
        }
    }

    fn mode(&self) -> u32 {
        (self.fs.read_inode(self.ino).mode as u32) & 0o7777
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        Ok(self.wrap(child))
    }

    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        self.fs.writable()?;
        if matches!(kind, NodeKind::CharDev | NodeKind::Symlink) {
            // A character device is a driver's business, and a link is made of
            // a target rather than a kind - neither is a file this filesystem
            // can invent from a name alone.
            return Err(VfsError::Unsupported);
        }
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let ftype = if kind == NodeKind::Dir {
            FT_DIR
        } else {
            FT_REG
        };
        let child = self.fs.new_inode(kind, self.ino)?;

        if kind == NodeKind::Dir {
            // The child's ".." is a link to us, on top of its own ".".
            dir.links += 1;
        }
        self.fs.dir_add(&mut dir, name, child, ftype)?;
        self.fs.write_inode(self.ino, &dir);

        Ok(self.wrap(child))
    }

    /// Remove `name` from this directory, freeing what it named if that was
    /// the last name for it.
    ///
    /// Directories are refused: their link count is not just their name in
    /// us but their own "." and every child's "..", so taking one apart is
    /// the other half of an `rmdir` this filesystem does not have.
    fn unlink(&self, name: &str) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        // Looked up before anything is touched, so a refusal leaves the
        // directory exactly as it was.
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        let mut data = self.fs.read_inode(child);
        if data.kind() == NodeKind::Dir {
            return Err(VfsError::IsDir);
        }
        // A directory entry naming an inode whose link count is already zero is
        // a damaged filesystem: ext2 never writes that state, and something that
        // did not finish - an install killed partway, a tool that wrote the
        // directory before the inode table - left it. Checked here, before
        // anything is touched, for the same reason the refusal above is: this
        // must not take a good directory entry away on its way to saying no.
        //
        // `data.links -= 1` on a zero is not a wrong answer, it is no answer:
        // in a debug build it stops the machine, and in a release build it
        // wraps to 65535 and leaks every block the inode maps - so a caller that
        // could have been told `EIO` instead finds out much later, and through
        // something else's corruption.
        if data.links == 0 {
            return Err(VfsError::Corrupt);
        }
        self.fs.dir_remove(&mut dir, name)?;
        self.fs.write_inode(self.ino, &dir);

        data.links -= 1;
        if data.links == 0 {
            self.fs.free_inode(child);
        } else {
            self.fs.write_inode(child, &data);
        }
        Ok(())
    }

    /// Remove the empty directory `name`.
    ///
    /// The accounting is why this cannot be [`Self::unlink`] with a flag.
    /// A directory's link count is not one name but three kinds of them: its
    /// parent's entry, its own `.`, and each of its children's `..`. So an
    /// empty one still holds two, its parent holds one on its behalf - that is
    /// the `+1` [`Self::create`] gave it - and removing it has to give that
    /// one back. Everything is checked before anything is written, so a
    /// refusal leaves both directories exactly as they were.
    fn rmdir(&self, name: &str) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        let data = self.fs.read_inode(child);
        if data.kind() != NodeKind::Dir {
            return Err(VfsError::NotDir);
        }
        // `.` and `..` are entries of every directory and are not contents.
        // Anything else is something to lose, so the call stops here.
        let holds_something = self
            .fs
            .dir_entries(&data)?
            .iter()
            .any(|(entry, _)| entry != "." && entry != "..");
        if holds_something {
            return Err(VfsError::NotEmpty);
        }

        self.fs.dir_remove(&mut dir, name)?;
        dir.links -= 1;
        self.fs.write_inode(self.ino, &dir);
        // The child's own two links - its name here and its `.` - went with
        // it, so nothing is left to decrement.
        self.fs.free_inode(child);
        Ok(())
    }

    /// Make a symbolic link called `name` holding `target`.
    ///
    /// The target is stored, not resolved: a link may point at a path that does
    /// not exist yet, and POSIX says making one is still allowed. What the
    /// format decides is *where* the text goes - inside the inode when it fits
    /// the footprint of the block pointers, in a data block when it does not -
    /// and that choice is invisible from outside, which is why `readlink` reads
    /// both the same way. Every package in a distribution that ships versioned
    /// libraries is built out of these: `libfoo.so.1` is a link to
    /// `libfoo.so.1.2.0`, and a filesystem that cannot make one cannot install
    /// a package.
    fn symlink(&self, name: &str, target: &str) -> VfsResult<Arc<dyn Inode>> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let child = self.fs.new_inode(NodeKind::Symlink, self.ino)?;
        let mut data = self.fs.read_inode(child);
        let bytes = target.as_bytes();
        if bytes.len() as u32 <= FAST_SYMLINK_MAX {
            // A fast link: the target *is* the block-pointer area, so no block
            // is ever allocated and `i_blocks` stays zero - which is the very
            // thing that says the target lives in the inode.
            let mut raw = [0u8; FAST_SYMLINK_MAX as usize];
            raw[..bytes.len()].copy_from_slice(bytes);
            for (i, ptr) in data.block.iter_mut().enumerate() {
                *ptr = u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().expect("four bytes"));
            }
        } else {
            // Too long for the inode, so it reads like a file's contents - and
            // `readlink` reads it back that way.
            let block = self.fs.alloc_block()?;
            data.block[0] = block as u32;
            data.blocks = (self.fs.block_size() / 512) as u32;
            self.fs.write_bytes(block, 0, bytes);
        }
        data.size = bytes.len() as u32;
        self.fs.write_inode(child, &data);

        self.fs.dir_add(&mut dir, name, child, FT_SYMLINK)?;
        self.fs.write_inode(self.ino, &dir);
        Ok(self.wrap(child))
    }

    /// Hard-link `target` under `name`: the directory gains an entry naming
    /// an inode that already exists, and that inode's link count goes up.
    /// The one rule ext2 enforces where Linux enforces it too: no second
    /// name for a directory, or the tree would grow cycles it cannot walk.
    fn link(&self, name: &str, target: &Arc<dyn Inode>) -> VfsResult<()> {
        self.fs.writable()?;
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        if self.fs.dir_lookup(&dir, name)?.is_some() {
            return Err(VfsError::Exists);
        }
        let child = target.metadata().ino as u32;
        let mut data = self.fs.read_inode(child);
        if data.kind() == NodeKind::Dir {
            return Err(VfsError::Invalid);
        }
        let ftype = match data.kind() {
            NodeKind::File => FT_REG,
            NodeKind::CharDev => FT_CHRDEV,
            NodeKind::Symlink => FT_SYMLINK,
            _ => FT_REG,
        };
        self.fs.dir_add(&mut dir, name, child, ftype)?;
        self.fs.write_inode(self.ino, &dir);
        data.links += 1;
        self.fs.write_inode(child, &data);
        Ok(())
    }

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        self.fs.dir_entries(&dir)
    }

    /// Where this link points, spelled exactly as the filesystem stored it.
    ///
    /// Alpine is built out of these: `/bin`, `/sbin`, `/usr/bin` and `/lib`
    /// hold a few hundred names, all of them links to one busybox or one
    /// loader. Resolution is what follows them; this is what says where.
    fn readlink(&self) -> VfsResult<String> {
        let data = self.fs.read_inode(self.ino);
        if data.kind() != NodeKind::Symlink {
            return Err(VfsError::Invalid);
        }
        let size = data.size as usize;
        if data.is_fast_symlink() {
            // No block was ever allocated: the target is the bytes of
            // i_block, read back as the text it was written as.
            let mut raw = Vec::with_capacity(size);
            for ptr in data.block.iter() {
                raw.extend_from_slice(&ptr.to_le_bytes());
            }
            raw.truncate(size);
            return String::from_utf8(raw).map_err(|_| VfsError::Invalid);
        }
        // A target too long for the inode is in data blocks and reads like a
        // file's contents - but through the block map directly, because
        // `read_at` refuses anything that is not a regular file.
        let block_size = self.fs.block_size() as u64;
        let mut raw = vec![0u8; size];
        let mut pos = 0usize;
        while pos < size {
            let logical = pos as u64 / block_size;
            let in_block = pos % block_size as usize;
            let n = (size - pos).min(block_size as usize - in_block);
            let Some(block) = self.fs.bmap_lookup(&data, logical) else {
                return Err(VfsError::Invalid);
            };
            self.fs.read_bytes(block, in_block, &mut raw[pos..pos + n]);
            pos += n;
        }
        String::from_utf8(raw).map_err(|_| VfsError::Invalid)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        if kind != NodeKind::File {
            // A directory is a directory; a link is a name, and reading
            // through one is a caller that skipped resolution. Neither may
            // reach the block pointers - a link keeps its target *there*, so
            // reading it as data asks the device for a block number spelled
            // out of the link's own text.
            return Err(if kind == NodeKind::Dir {
                VfsError::IsDir
            } else {
                VfsError::Invalid
            });
        }
        let block_size = self.fs.block_size() as u64;
        let end = (data.size as u64).min(offset + buf.len() as u64);
        if offset >= end {
            return Ok(0);
        }
        let mut done = 0;
        let mut pos = offset;
        while pos < end {
            let logical = pos / block_size;
            let in_block = (pos % block_size) as usize;
            let n = ((end - pos) as usize).min(block_size as usize - in_block);
            match self.fs.bmap_lookup(&data, logical) {
                Some(block) => self
                    .fs
                    .read_bytes(block, in_block, &mut buf[done..done + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
            pos += n as u64;
        }
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.fs.writable()?;
        let mut data = self.fs.read_inode(self.ino);
        let kind = data.kind();
        if kind != NodeKind::File {
            // The same refusal as a read, for the same reason: a link's block
            // pointers are its target, and writing "data" there would rewrite
            // where the link points.
            return Err(if kind == NodeKind::Dir {
                VfsError::IsDir
            } else {
                VfsError::Invalid
            });
        }
        if offset + buf.len() as u64 > u32::MAX as u64 {
            return Err(VfsError::FileTooBig);
        }
        let block_size = self.fs.block_size();
        let mut done = 0;
        let mut pos = offset;
        while done < buf.len() {
            let logical = pos / block_size as u64;
            let in_block = (pos % block_size as u64) as usize;
            let n = (block_size - in_block).min(buf.len() - done);
            let (block, allocated) = self.fs.bmap_ensure(&mut data, logical)?;
            let src = &buf[done..done + n];
            self.fs.update_block(block, |page| {
                page[in_block..in_block + n].copy_from_slice(src);
                true
            });
            if allocated {
                data.blocks += (block_size / 512) as u32;
            }
            done += n;
            pos += n as u64;
        }
        data.size = data.size.max((offset + buf.len() as u64) as u32);
        self.fs.write_inode(self.ino, &data);
        Ok(done)
    }

    /// Resize, with the three cases done differently because they are
    /// different jobs.
    ///
    /// Growing changes the size and nothing else. A block that was never
    /// allocated reads as zeros - [`Self::read_at`] fills a hole - so there is
    /// no data to write.
    ///
    /// Shrinking has to give the tail's blocks back to the bitmaps. This
    /// driver can free *all* of an inode's blocks but has no "free the ones
    /// past N", so the prefix is held aside, the whole mapping is dropped, and
    /// the prefix is written back. Wasteful, and obviously correct, which is
    /// the right trade at this size. Zero - the common case, since it is what
    /// `O_TRUNC` and a replacing `rename` ask for - has no prefix to save and
    /// costs nothing extra.
    fn truncate(&self, len: u64) -> VfsResult<()> {
        self.fs.writable()?;
        let mut data = self.fs.read_inode(self.ino);
        match data.kind() {
            NodeKind::File => {}
            NodeKind::Dir => return Err(VfsError::IsDir),
            // A link's block pointers are its target: there is nothing here
            // to resize, and truncating it would rewrite where it points.
            _ => return Err(VfsError::Invalid),
        }
        let size = data.size as u64;
        if len == size {
            return Ok(());
        }
        if len > size {
            data.size = u32::try_from(len).map_err(|_| VfsError::FileTooBig)?;
            self.fs.write_inode(self.ino, &data);
            return Ok(());
        }
        let keep = if len == 0 {
            Vec::new()
        } else {
            let mut prefix = vec![0u8; len as usize];
            self.read_at(0, &mut prefix)?;
            prefix
        };
        self.fs.free_blocks_of(&data);
        data.block = [0; 15];
        data.blocks = 0;
        data.size = 0;
        self.fs.write_inode(self.ino, &data);
        if !keep.is_empty() {
            self.write_at(0, &keep)?;
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    // The host test build links std, which the kernel itself never sees.
    extern crate std;

    /// An in-memory disk for host tests: a `Vec<u8>` behind the house lock.
    struct MemDev {
        bytes: Mutex<RefCell<Vec<u8>>>,
    }

    impl MemDev {
        fn new(blocks: usize) -> MemDev {
            MemDev {
                bytes: Mutex::new(RefCell::new(vec![0u8; blocks * 1024])),
            }
        }
    }

    impl BlockDev for MemDev {
        fn capacity_sectors(&self) -> u64 {
            critical_section::with(|cs| self.bytes.borrow(cs).borrow().len() as u64 / 512)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) {
            critical_section::with(|cs| {
                let bytes = self.bytes.borrow(cs).borrow();
                let at = offset as usize;
                buf.copy_from_slice(&bytes[at..at + buf.len()]);
            });
        }

        fn write_at(&self, offset: u64, buf: &[u8]) {
            critical_section::with(|cs| {
                let mut bytes = self.bytes.borrow(cs).borrow_mut();
                let at = offset as usize;
                bytes[at..at + buf.len()].copy_from_slice(buf);
            });
        }
    }

    fn put16(bytes: &mut [u8], at: usize, v: u16) {
        bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn put32(bytes: &mut [u8], at: usize, v: u32) {
        bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    // Test image layout: block 0 boot, 1 superblock, 2 group descriptors,
    // 3 block bitmap, 4 inode bitmap, 5.. inode table (128-byte inodes),
    // then the root directory, then data. The table's length follows the
    // inode count, so the big-disk fixtures can hold enough inodes to fill
    // a directory index.
    const TABLE: usize = 5 * 1024;

    /// A minimal 1024-byte-block, one-group ext2 with an empty root
    /// directory. Enough structure to mount, and to hand-place files into.
    /// `total_blocks` sizes the disk; the small default keeps the original
    /// 128-block, 64-inode shape, the big ones grow the inode table to match
    /// the tests that fill a directory index.
    fn format_sized(total_blocks: usize) -> MemDev {
        let inodes = if total_blocks <= 128 { 64 } else { 1200 };
        let inode_blocks = inodes * 128 / 1024;
        let root_dir_block = 5 + inode_blocks;
        let used_blocks = root_dir_block; // blocks 1..=root_dir_block
        let dev = MemDev::new(total_blocks);
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let sb = 1024;
            put32(disk, sb, inodes as u32); // inodes_count
            put32(disk, sb + 4, total_blocks as u32); // blocks_count
            put32(disk, sb + 12, (total_blocks - used_blocks) as u32); // free_blocks_count
            put32(disk, sb + 16, (inodes - 2) as u32); // free_inodes_count
            put32(disk, sb + 20, 1); // first_data_block
            put32(disk, sb + 24, 0); // log_block_size: 1024
            put32(disk, sb + 28, 0); // fragments == blocks
            put32(disk, sb + 32, 8192); // blocks_per_group
            put32(disk, sb + 36, 8192); // frags_per_group
            put32(disk, sb + 40, inodes as u32); // inodes_per_group
            put16(disk, sb + 56, MAGIC);
            put16(disk, sb + 58, 1); // clean
            put32(disk, sb + 76, 1); // rev 1
            put32(disk, sb + 84, 11); // first_ino
            put16(disk, sb + 88, 128); // inode_size
            put32(disk, sb + 96, INCOMPAT_FILETYPE);

            put32(disk, 2 * 1024, 3); // block bitmap
            put32(disk, 2 * 1024 + 4, 4); // inode bitmap
            put32(disk, 2 * 1024 + 8, 5); // inode table
            // The three counters are packed u16s at 12, 14 and 16 - free
            // blocks, free inodes, used directories. Getting the middle one
            // wrong does not corrupt anything visibly: the allocator reads a
            // group with "no free inodes", skips it, and every create then
            // answers ENOSPC on a filesystem that is almost empty.
            put16(disk, 2 * 1024 + 12, (total_blocks - used_blocks) as u16); // free blocks
            put16(disk, 2 * 1024 + 14, (inodes - 2) as u16); // free inodes
            put16(disk, 2 * 1024 + 16, 1); // used dirs (the root)

            // Blocks 1..=root_dir_block are metadata and the root directory.
            // On a 1 KiB filesystem block 0 lives outside every group, so
            // bitmap bit b is block b + 1.
            for bit in 0..used_blocks {
                disk[3 * 1024 + bit / 8] |= 1 << (bit % 8);
            }
            disk[4 * 1024] = 0b0000_0011; // inodes 1 (bad) and 2 (root)

            let root = TABLE + 128; // inode 2, index 1
            put16(disk, root, S_IFDIR | 0o755);
            put32(disk, root + 4, 1024); // size
            put16(disk, root + 26, 2); // links
            put32(disk, root + 28, 2); // blocks in sectors
            put32(disk, root + 40, root_dir_block as u32);

            // "." and "..", the latter owning the rest of the block.
            write_dirent(disk, root_dir_block * 1024, 2, dirent_len(1), ".", FT_DIR);
            write_dirent(
                disk,
                root_dir_block * 1024 + dirent_len(1),
                2,
                1024 - dirent_len(1),
                "..",
                FT_DIR,
            );
        });
        dev
    }

    fn format() -> MemDev {
        format_sized(128)
    }

    /// The raw word at `offset` in the on-disk inode `ino`.
    fn inode_word(dev: &MemDev, ino: u32, offset: usize) -> u32 {
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le32(&disk, TABLE + (ino as usize - 1) * 128 + offset)
        })
    }

    /// Convert an empty directory (nothing but "." and "..") into a one-leaf
    /// htree index, the shape a Linux conversion leaves behind: block 0
    /// becomes the index root and a fresh leaf - allocated by the caller at
    /// `leaf_block` - takes the (here: no) entries.
    fn make_indexed(dev: &MemDev, dir_ino: u32, leaf_block: u32) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let at = TABLE + (dir_ino as usize - 1) * 128;
            let base = le32(disk, at + 40) as usize * 1024;
            let parent = le32(disk, base + 12); // ".." keeps its inode

            // Inode: the flag goes on, the leaf joins the directory, and
            // block[1] names it.
            let flags = le32(disk, at + 32) | INODE_INDEX_FL;
            put32(disk, at + 32, flags);
            let size = le32(disk, at + 4) + 1024;
            put32(disk, at + 4, size);
            let sectors = le32(disk, at + 28) + 2;
            put32(disk, at + 28, sectors);
            put32(disk, at + 44, leaf_block);

            // The leaf: one hole covering the whole block.
            let leaf = leaf_block as usize * 1024;
            put16(disk, leaf + 4, 1024);

            // Block 0 becomes the index root.
            write_dirent(disk, base, dir_ino, dirent_len(1), ".", FT_DIR);
            write_dirent(
                disk,
                base + dirent_len(1),
                parent,
                1024 - dirent_len(1),
                "..",
                FT_DIR,
            );
            put32(disk, base + 24, 0); // reserved_zero
            disk[base + 28] = 1; // half_md4
            disk[base + 29] = 8; // info_length
            disk[base + 30] = 0; // indirect_levels
            disk[base + 31] = 0; // unused flags
            put16(disk, base + 32, dx_root_limit(1024, 8) as u16); // limit
            put16(disk, base + 34, 1); // count: the alias only
            put32(disk, base + 36, 1); // entry[0].block = the leaf

            // Bookkeeping: the leaf block is in use now.
            disk[3 * 1024 + (leaf_block as usize - 1) / 8] |= 1 << ((leaf_block - 1) % 8);
            for free in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(disk, free) - 1;
                put16(disk, free, n);
            }
        });
    }

    /// Place a file by hand: inode, data block, both bitmaps, and a root
    /// directory entry carved out of the last entry's slack. Bypasses the
    /// filesystem entirely - the point is to read back host-written bytes.
    fn seed_file(dev: &MemDev, ino: u32, name: &str, data_block: u32, data: &[u8]) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;

            let at = TABLE + (ino as usize - 1) * 128;
            put16(disk, at, S_IFREG | 0o644);
            put32(disk, at + 4, data.len() as u32);
            put16(disk, at + 26, 1);
            put32(disk, at + 28, 2);
            put32(disk, at + 40, data_block);
            disk[data_block as usize * 1024..data_block as usize * 1024 + data.len()]
                .copy_from_slice(data);

            disk[4 * 1024 + (ino as usize - 1) / 8] |= 1 << ((ino - 1) % 8);
            // Bitmap bit b is block b + 1 on this image (see format()).
            disk[3 * 1024 + (data_block as usize - 1) / 8] |= 1 << ((data_block - 1) % 8);
            for at in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(disk, at) - 1;
                put16(disk, at, n);
            }
            for at in [2 * 1024 + 16, 1024 + 16] {
                let n = le16(disk, at) - 1;
                put16(disk, at, n);
            }

            // Walk to the entry that owns the end of the root block and take
            // its slack. The root's block comes from the root inode - the
            // big fixtures lay the table out differently.
            let root_block = le32(disk, TABLE + 128 + 40) as usize * 1024;
            let root = &mut disk[root_block..];
            let mut last = 0;
            loop {
                let rec = le16(root, last + 4) as usize;
                if last + rec >= 1024 {
                    break;
                }
                last += rec;
            }
            let own = dirent_len(root[last + 6] as usize);
            assert!(
                le16(root, last + 4) as usize >= own + dirent_len(name.len()),
                "no slack left for the seed"
            );
            set16(root, last + 4, own as u16);
            let rest = le16(root, last + 4) as usize - own;
            write_dirent(root, last + own, ino, rest, name, FT_REG);
        });
    }

    fn mounted(dev: Arc<dyn BlockDev>) -> Ext2Fs {
        Ext2Fs::mount(dev).expect("the test image mounts")
    }

    fn read_all(inode: &dyn Inode, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let n = inode.read_at(0, &mut buf).expect("read_at works");
        buf.truncate(n);
        buf
    }

    #[test]
    fn mount_rejects_bytes_that_are_not_ext2() {
        assert!(Ext2Fs::mount(Arc::new(MemDev::new(128))).is_err());
    }

    #[test]
    fn a_seeded_file_and_a_hole_read_back_exactly() {
        let dev = Arc::new(format());
        seed_file(
            &dev,
            11,
            "hello.txt",
            14,
            b"hello from the host-written image\n",
        );
        // A sparse second file: first block unmapped, data in the second.
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let at = TABLE + 11 * 128; // inode 12
            put16(disk, at, S_IFREG | 0o644);
            put32(disk, at + 4, 2048);
            put16(disk, at + 26, 1);
            put32(disk, at + 28, 2);
            put32(disk, at + 44, 15); // i_block[1] only: block 0 is a hole
            disk[15 * 1024..15 * 1024 + 4].copy_from_slice(b"tail");
            disk[4 * 1024 + 1] |= 1 << ((12 - 1) % 8);
            disk[3 * 1024 + 14 / 8] |= 1 << (14 % 8); // bit 14 is block 15

            // hello.txt owns the block end; shrink it and insert behind it.
            let root = &mut disk[13 * 1024..];
            set16(root, 24 + 4, dirent_len(9) as u16);
            write_dirent(
                root,
                24 + dirent_len(9),
                12,
                1024 - 24 - dirent_len(9),
                "sparse",
                FT_REG,
            );
        });

        let fs = mounted(dev);
        let root = fs.root();
        let hello = root.lookup("hello.txt").expect("seeded file is found");
        assert_eq!(hello.metadata().size, 34);
        assert_eq!(
            read_all(&*hello, 64),
            b"hello from the host-written image\n".to_vec()
        );

        let sparse = root.lookup("sparse").expect("sparse file is found");
        let bytes = read_all(&*sparse, 2048);
        assert_eq!(bytes.len(), 2048);
        assert!(bytes[..1024].iter().all(|&b| b == 0), "the hole is zeros");
        assert_eq!(&bytes[1024..1028], b"tail");
        assert_eq!(root.lookup("missing").err(), Some(VfsError::NoEntry));
    }

    /// The byte pattern the QEMU gate writes to the real disk, mirrored here.
    fn pattern(i: usize) -> u8 {
        (i * 31 + 7) as u8
    }

    #[test]
    fn writes_span_direct_and_indirect_and_survive_a_remount() {
        const LEN: usize = 20 * 1024; // 12 direct blocks + 8 through an indirect
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let out = root.create("out.bin", NodeKind::File).expect("create");
        let bytes: Vec<u8> = (0..LEN).map(pattern).collect();
        assert_eq!(out.write_at(0, &bytes).unwrap(), LEN);
        // Rewriting a middle chunk in place must not disturb its neighbours.
        assert_eq!(out.write_at(5 * 1024, b"MIDDLE").unwrap(), 6);

        drop(out);
        drop(root);
        drop(fs);

        // A fresh mount over the same device: cold cache, the disk is truth.
        let fs2 = mounted(dev);
        let back = fs2
            .root()
            .lookup("out.bin")
            .expect("the file is on the disk");
        assert_eq!(back.metadata().size as usize, LEN);
        let read_back = read_all(&*back, LEN);
        assert_eq!(&read_back[5 * 1024..5 * 1024 + 6], b"MIDDLE");
        for (i, &b) in read_back.iter().enumerate() {
            if (5 * 1024..5 * 1024 + 6).contains(&i) {
                continue;
            }
            assert_eq!(b, pattern(i), "byte {i} (block {})", i / 1024);
        }
    }

    #[test]
    fn directory_growth_uses_new_blocks_when_slack_runs_out() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        // Names long enough that a handful fill one block: real length
        // align4(8 + 60) = 68, so a block holds ~14 entries before a second
        // directory block must be allocated.
        let names: Vec<String> = (0..40)
            .map(|i| alloc::format!("f{i:02}-{}", "x".repeat(54)))
            .collect();
        for name in &names {
            root.create(name, NodeKind::File)
                .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
        }
        for name in &names {
            root.lookup(name)
                .unwrap_or_else(|e| panic!("lookup {name}: {e:?}"));
        }

        assert_eq!(root.lookup(&names[0]).unwrap().metadata().ino, 11);
        let root_data = fs.core.read_inode(ROOT_INO);
        assert!(root_data.size > 1024, "the directory grew");
        assert!(
            root_data.block[1] != 0,
            "a second directory block was mapped"
        );
    }

    #[test]
    fn truncating_to_zero_leaves_no_tail_and_frees_the_blocks() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root.create("log", NodeKind::File).expect("create");
        // Three blocks' worth, so the tail is real blocks rather than bytes
        // inside the block the head lives in.
        let long = vec![b'x'; 3 * 1024];
        file.write_at(0, &long).unwrap();
        assert_eq!(free_counts(&dev), (115 - 3, 61), "three data blocks");

        file.truncate(0).expect("truncate to zero");
        assert_eq!(file.metadata().size, 0);
        assert_eq!(read_all(&*file, 16).len(), 0, "an empty file reads empty");
        assert_eq!(free_counts(&dev), (115, 61), "the blocks came back");

        // The inode survived - only the contents went - so the same name can
        // be written again without having to be created again.
        assert_eq!(file.write_at(0, b"fresh").unwrap(), 5);
        assert_eq!(read_all(&*file, 16), b"fresh");
    }

    #[test]
    fn truncating_a_prefix_keeps_the_prefix_and_drops_the_rest() {
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let file = root.create("cut", NodeKind::File).expect("create");
        file.write_at(0, &vec![b'a'; 2500]).unwrap();
        file.write_at(2500, b"tail that goes away").unwrap();

        file.truncate(2500).expect("truncate the tail off");
        assert_eq!(file.metadata().size, 2500);
        let bytes = read_all(&*file, 4096);
        assert_eq!(bytes.len(), 2500);
        assert!(bytes.iter().all(|&b| b == b'a'), "the prefix is intact");
        // The tail's blocks went back to the bitmaps, and the prefix's were
        // reallocated: the file still occupies what it holds, no more.
        assert_eq!(free_counts(&dev), (115 - 3, 61));
    }

    #[test]
    fn truncating_upward_grows_a_hole_that_reads_as_zeros() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        let file = root.create("grow", NodeKind::File).expect("create");
        file.write_at(0, b"head").unwrap();
        file.truncate(4096).expect("grow");

        assert_eq!(file.metadata().size, 4096);
        let bytes = read_all(&*file, 8192);
        assert_eq!(bytes.len(), 4096);
        assert_eq!(&bytes[..4], b"head");
        assert!(
            bytes[4..].iter().all(|&b| b == 0),
            "the new space is a hole"
        );
    }

    #[test]
    fn truncate_refuses_what_is_not_a_file() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        let dir = root.create("d", NodeKind::Dir).expect("create dir");
        assert_eq!(dir.truncate(0).err(), Some(VfsError::IsDir));
        assert_eq!(root.truncate(0).err(), Some(VfsError::IsDir));
    }

    #[test]
    fn a_symlink_puts_its_target_where_it_fits_and_reads_it_back() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();

        // A target that fits the inode's block-pointer area lives there, and
        // costs nothing but the inode - which is the format's fast link, and
        // what `libfoo.so.1 -> libfoo.so.1.2.0` is in every package ever built.
        let fast = root.symlink("short", "/bin/busybox").expect("fast link");
        assert_eq!(fast.metadata().kind, NodeKind::Symlink);
        assert_eq!(fast.readlink().unwrap(), "/bin/busybox");
        assert_eq!(fast.metadata().size, 12);
        assert_eq!(free_counts(&dev), (115, 61), "an inode, but no data block");

        // One that does not fit gets a block, and reads back identically - the
        // choice is invisible from outside, which is the point.
        let long = "/usr/libexec/some/very/long/path/that/does/not/fit/anywhere/busybox";
        assert!(long.len() > 60, "the long case has to be long");
        let slow = root.symlink("long", long).expect("slow link");
        assert_eq!(slow.readlink().unwrap(), long);
        assert_eq!(slow.metadata().size, long.len() as u64);
        assert_eq!(free_counts(&dev), (114, 60), "and one block for the target");

        // The name is taken; the target does not have to exist.
        assert_eq!(
            root.symlink("short", "/elsewhere").err(),
            Some(VfsError::Exists)
        );

        // A cold mount is the only witness that this reached the disk.
        drop(fast);
        drop(slow);
        drop(root);
        drop(fs);
        let fs2 = mounted(dev);
        let root2 = fs2.root();
        assert_eq!(
            root2.lookup("short").unwrap().readlink().unwrap(),
            "/bin/busybox"
        );
        assert_eq!(root2.lookup("long").unwrap().readlink().unwrap(), long);

        // And a link is not a file: writing through one, or resizing it, would
        // rewrite where it points.
        let link = root2.lookup("long").unwrap();
        assert_eq!(link.write_at(0, b"x").err(), Some(VfsError::Invalid));
        assert_eq!(link.truncate(0).err(), Some(VfsError::Invalid));
        assert_eq!(
            link.read_at(0, &mut [0u8; 4]).err(),
            Some(VfsError::Invalid)
        );
    }

    #[test]
    fn unlinking_a_symlink_gives_back_only_what_it_took() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();

        // A fast link's "block pointers" are the text of its target. Freeing
        // them as if they were block numbers takes blocks out of whatever else
        // is on the filesystem - or, when the text is not a plausible number,
        // walks off the group table and panics. `/bin/busybox` reads as one.
        root.symlink("fast", "/bin/busybox").expect("fast link");
        assert_eq!(free_counts(&dev), (115, 61), "an inode, and nothing else");
        root.unlink("fast").expect("unlink the fast link");
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "the inode came back and no block was ever touched"
        );

        // The same for a link whose target needed a block: that block *is* its
        // own, and does come back.
        let long = "/usr/libexec/some/very/long/path/that/does/not/fit/in/an/inode/busybox";
        root.symlink("slow", long).expect("slow link");
        assert_eq!(free_counts(&dev), (114, 61), "the block and the inode");
        root.unlink("slow").expect("unlink the slow link");
        assert_eq!(free_counts(&dev), (115, 62), "both came back");
    }

    #[test]
    fn rmdir_takes_an_empty_directory_and_puts_the_books_back() {
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));
        let fs = mounted(dev.clone());
        let root = fs.root();
        let root_links = fs.core.read_inode(ROOT_INO).links;

        let dir = root.create("d", NodeKind::Dir).expect("create dir");
        // A new subdirectory is a name in its parent *and* a `..` pointing
        // back at it, so the parent gains a link as well as the child.
        assert_eq!(fs.core.read_inode(ROOT_INO).links, root_links + 1);
        assert_eq!(free_counts(&dev), (114, 61), "one block and one inode");

        // Not empty, and it must say so rather than take what is inside with
        // it: that is the whole difference between `rmdir` and a recursive
        // delete wearing the wrong name.
        let inner = dir.create("inner", NodeKind::File).expect("create inner");
        inner.write_at(0, b"x").unwrap();
        assert_eq!(root.rmdir("d").err(), Some(VfsError::NotEmpty));
        assert!(root.lookup("d").is_ok(), "the directory is still there");

        // Emptying it is the caller's job, one name at a time.
        dir.unlink("inner").expect("unlink what was inside");
        root.rmdir("d").expect("rmdir the empty directory");
        assert_eq!(
            fs.core.read_inode(ROOT_INO).links,
            root_links,
            "the parent's link came back"
        );
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "and so did the child's block and inode"
        );

        // A file is not a directory, and a name that is not there is not one.
        root.create("f", NodeKind::File).expect("create file");
        assert_eq!(root.rmdir("f").err(), Some(VfsError::NotDir));
        assert_eq!(root.rmdir("gone").err(), Some(VfsError::NoEntry));
    }

    #[test]
    fn a_write_past_eof_leaves_zeros_in_the_gap() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        let file = root.create("sparse", NodeKind::File).expect("create");
        assert_eq!(file.write_at(5000, b"abc").unwrap(), 3);
        assert_eq!(file.metadata().size, 5003);

        let bytes = read_all(&*file, 5010);
        assert_eq!(bytes.len(), 5003);
        assert!(bytes[..5000].iter().all(|&b| b == 0), "the gap is zeros");
        assert_eq!(&bytes[5000..], b"abc");
    }

    #[test]
    fn creating_twice_is_exists_and_directories_get_dot_entries() {
        let dev = Arc::new(format());
        let fs = mounted(dev.clone());
        let root = fs.root();
        let dir = root.create("d", NodeKind::Dir).expect("create dir");
        assert_eq!(
            root.create("d", NodeKind::Dir).err(),
            Some(VfsError::Exists)
        );

        let inner = dir.create("f", NodeKind::File).expect("create in dir");
        assert_eq!(inner.write_at(0, b"nested").unwrap(), 6);
        assert_eq!(dir.metadata().kind, NodeKind::Dir);
        // A directory's size is the byte length of its dirent blocks - what
        // Linux's own ext2 reports and `ls -l` prints. A fresh directory
        // holds "." and ".." in its one first block, so the size is the
        // block size. Zeroing it (as this test once asserted) made every
        // directory read as empty of content as a stat could suggest.
        assert_eq!(dir.metadata().size, fs.core.block_size() as u64);

        // "." and ".." resolve to the right inodes, through the real parser.
        assert_eq!(
            dir.lookup(".").expect(".").metadata().ino,
            dir.metadata().ino
        );
        assert_eq!(
            dir.lookup("..").expect("..").metadata().ino,
            root.metadata().ino
        );

        // The nested file is reachable from the root on a fresh mount, which
        // also proves the directory metadata hit the disk.
        drop(inner);
        drop(dir);
        drop(root);
        drop(fs);
        let fs2 = mounted(dev);
        let found = fs2.root().lookup("d").unwrap().lookup("f").unwrap();
        let mut buf = [0u8; 6];
        assert_eq!(found.read_at(0, &mut buf).unwrap(), 6);
        assert_eq!(&buf, b"nested");
    }

    #[test]
    fn read_dir_walks_every_entry_including_the_dots() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        root.create("note", NodeKind::File).expect("create file");
        root.create("docs", NodeKind::Dir).expect("create dir");

        let listed = root.read_dir().expect("list the root");
        // "." and ".." come with the block, as POSIX readdir reports them.
        assert_eq!(listed.len(), 4, "got {listed:?}");
        assert!(listed.contains(&(String::from("."), NodeKind::Dir)));
        assert!(listed.contains(&(String::from(".."), NodeKind::Dir)));
        assert!(listed.contains(&(String::from("note"), NodeKind::File)));
        assert!(listed.contains(&(String::from("docs"), NodeKind::Dir)));

        // A file lists nothing.
        let file = root.lookup("note").unwrap();
        assert_eq!(file.read_dir().unwrap_err(), VfsError::NotDir);
    }

    /// The image's own free counts, as the superblock reports them.
    fn free_counts(dev: &MemDev) -> (u32, u32) {
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            (le32(&disk, 1024 + 12), le32(&disk, 1024 + 16))
        })
    }

    #[test]
    fn unlink_gives_the_name_the_blocks_and_the_inode_back() {
        const LEN: usize = 20 * 1024; // 12 direct blocks + 8 through one indirect
        let dev = Arc::new(format());
        assert_eq!(free_counts(&dev), (115, 62));

        let fs = mounted(dev.clone());
        let root = fs.root();
        let out = root.create("out.bin", NodeKind::File).expect("create");
        let bytes: Vec<u8> = (0..LEN).map(pattern).collect();
        assert_eq!(out.write_at(0, &bytes).unwrap(), LEN);
        assert_eq!(free_counts(&dev), (115 - 21, 61), "20 data, one indirect");

        root.unlink("out.bin").expect("unlink");
        assert_eq!(
            free_counts(&dev),
            (115, 62),
            "every block and the inode went back to the bitmaps"
        );

        drop(out);
        drop(root);
        drop(fs);
        // A cold mount is the only witness that can tell the disk from the
        // cache of the mount that did the writing.
        let fs2 = mounted(dev);
        assert_eq!(
            fs2.root().lookup("out.bin").err(),
            Some(VfsError::NoEntry),
            "the name left the disk"
        );
    }

    #[test]
    fn a_removed_name_can_be_created_again() {
        let dev = Arc::new(format());
        let fs = mounted(dev);
        let root = fs.root();
        let file = root.create("scratch", NodeKind::File).expect("create");
        assert_eq!(file.write_at(0, b"first").unwrap(), 5);
        root.unlink("scratch").expect("unlink");

        // What a second boot over the same image does: the create must not
        // answer Exists, and the inode it is handed must be a clean one.
        let again = root
            .create("scratch", NodeKind::File)
            .expect("create the same name again");
        assert_eq!(again.metadata().size, 0, "the reused inode starts empty");
        assert_eq!(read_all(&*again, 16), Vec::new());
    }

    #[test]
    fn unlink_refuses_directories_and_unknown_names() {
        let fs = mounted(Arc::new(format()));
        let root = fs.root();
        root.create("d", NodeKind::Dir).expect("create dir");
        assert_eq!(root.unlink("d").err(), Some(VfsError::IsDir));
        assert_eq!(root.unlink("nope").err(), Some(VfsError::NoEntry));

        // Neither refusal touched anything.
        root.lookup("d").expect("the directory is still there");
        assert!(
            root.read_dir()
                .expect("list the root")
                .contains(&(String::from("d"), NodeKind::Dir))
        );
    }

    #[test]
    fn directory_entry_removal_hands_the_space_to_the_entry_before() {
        let mut page = [0u8; 64];
        write_dirent(&mut page, 0, 2, 12, ".", FT_DIR);
        write_dirent(&mut page, 12, 2, 24, "..", FT_DIR);
        write_dirent(&mut page, 36, 7, 28, "gone", FT_REG);

        let mut found = None;
        assert!(dir_take(&mut page, "gone", &mut found));
        assert_eq!(found, Some(7));
        // ".." grew over the gap; "." never moved.
        assert_eq!(le16(&page, 16), 52);
        assert_eq!(le16(&page, 4), 12);
        let mut found = None;
        assert!(!dir_take(&mut page, "gone", &mut found), "it is gone");

        // The first entry of a block has nothing before it, so it becomes a
        // hole: the zero inode a walk skips and a later insert reuses.
        let mut found = None;
        assert!(dir_take(&mut page, ".", &mut found));
        assert_eq!(found, Some(2));
        assert_eq!(le32(&page, 0), 0);
        let mut seen = Vec::new();
        dir_walk(&page, |ino, _ft, name| {
            seen.push((ino, String::from_utf8(name.to_vec()).expect("utf8")));
            true
        });
        assert_eq!(seen, vec![(2u32, String::from(".."))]);
    }

    #[test]
    fn directory_entry_insertion_carves_slack_exactly() {
        // 64 bytes: "." (12) then ".." owning the rest (52). Entries insert
        // into live entries' slack, and each new entry's record spans what
        // is left, exactly as ext2 lays them out.
        let mut page = [0u8; 64];
        write_dirent(&mut page, 0, 2, dirent_len(1), ".", FT_DIR);
        write_dirent(
            &mut page,
            dirent_len(1),
            2,
            64 - dirent_len(1),
            "..",
            FT_DIR,
        );

        assert!(dir_try_insert(&mut page, "newly", 7, FT_REG));
        assert_eq!(le32(&page, 24), 7, "inserted behind ..");
        assert_eq!(le16(&page, 28), 64 - 24, "its record owns the rest");
        assert_eq!(page[30], 5, "name_len");
        assert_eq!(le16(&page, 16), 12, ".. shrank to its real length");

        // A second one shrinks the first inserted entry instead.
        assert!(dir_try_insert(&mut page, "ab", 8, FT_REG));
        assert_eq!(le32(&page, 40), 8);
        assert_eq!(le16(&page, 44), 24, "the new entry owns what remained");
        assert_eq!(le16(&page, 28), 16, "newly shrank to its real length");

        // Nothing left but "ab"'s 12-byte slack: a 20-byte entry cannot fit.
        assert!(!dir_try_insert(&mut page, "toolongname", 9, FT_REG));
        // But a 12-byte one fits exactly, taking the whole remainder.
        assert!(dir_try_insert(&mut page, "cd", 9, FT_REG));
        assert_eq!(le16(&page, 44), 12, "ab shrank to its real length");
        assert_eq!(le32(&page, 52), 9);
        assert_eq!(le16(&page, 56), 12, "cd owns what ab's record gave up");
    }

    /// apk's install, replayed in the order that broke it: a package lays
    /// every file down under a `.apk.<digest>` temp name and commits them
    /// one by one, and within a directory the `.so` symlink sorts before the
    /// real library it points at. So the symlink's rename runs while its
    /// target still lives under its own temp name - resolving the link would
    /// go looking for the target's real name and die with ENOENT, which is
    /// exactly what `apk add eza` showed: every regular file committed,
    /// every symlink failed. A rename must move the name un-followed.
    #[test]
    fn an_apk_style_symlink_commit_resolves() {
        let fs = mounted(Arc::new(format()));
        let vfs = alloc::sync::Arc::new(crate::vfs::mount::Vfs::new());
        vfs.mount_root(&fs);

        let root = vfs.resolve("/").expect("root");
        root.create("usr", NodeKind::Dir).expect("usr");
        let usr = root.lookup("usr").expect("usr");
        usr.create("lib", NodeKind::Dir).expect("lib");
        let lib = usr.lookup("lib").expect("lib");

        let tmpname = ".apk.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let bodytmp = ".apk.1111111111111111111111111111111111111111111111111111111111111111";

        // Extract: the symlink first - dangling, because the library under
        // it has not been laid down yet - then the library's own temp file.
        lib.symlink(tmpname, "libllhttp.so.9.4.1")
            .expect("symlinkat the temp name");
        let body = lib
            .create(bodytmp, NodeKind::File)
            .expect("the real library's temp file");
        crate::vfs::File::new(body.clone())
            .write(&vec![0x42u8; 63088])
            .expect("write the library body");

        // Commit the symlink first, as the db order does. What renameat2
        // must do: look the old name up un-followed, read the link's own
        // target text, and lay the same link under the real name.
        let old = lib.lookup(tmpname).expect("the temp link is there");
        assert_eq!(old.metadata().kind, NodeKind::Symlink);
        assert_eq!(old.readlink().expect("readlink"), "libllhttp.so.9.4.1");
        lib.symlink("libllhttp.so.9.4", "libllhttp.so.9.4.1")
            .expect("rename lays the link under its real name");
        lib.unlink(tmpname).expect("the temp name goes");

        // Commit the library itself.
        lib.link("libllhttp.so.9.4.1", &body)
            .expect("rename the body");
        lib.unlink(bodytmp).expect("the body's temp name goes");

        // The end state apk expects: the library body, plus a link chain
        // that resolves to it.
        let lib9 = lib.lookup("libllhttp.so.9.4").expect("the .so.9.4 link");
        assert_eq!(lib9.metadata().kind, NodeKind::Symlink);
        assert_eq!(lib9.readlink().expect("readlink"), "libllhttp.so.9.4.1");
        let real = lib.lookup("libllhttp.so.9.4.1").expect("the library");
        assert_eq!(real.metadata().kind, NodeKind::File);
        assert_eq!(real.metadata().size, 63088);
    }

    // ---- the directory hash tree ----

    #[test]
    fn dx_hash_matches_the_reference_implementation() {
        // Goldens from e2fsprogs 1.47.4's debugfs `dx_hash`, half_md4 salted
        // with this seed - the one /usr/lib of the real image carries. The
        // third name is full of bytes above 127, the one place the signed and
        // unsigned flavors of the algorithm part ways.
        let seed = [0x0450_54d6, 0x9e4e_2290, 0xe5a0_1da8, 0xbc0d_2838];
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 1, seed, false),
            Some(0x2d8a_cbda)
        );
        assert_eq!(
            dx_hash_name(b"modules-load.d", 1, seed, false),
            Some(0x26d5_4824)
        );
        assert_eq!(
            dx_hash_name(&[0xff, 0x80, 0x7f, 0x01, b'a', b'b'], 1, seed, false),
            Some(0xbbed_4542)
        );
        assert_eq!(
            dx_hash_name(&[0xff, 0x80, 0x7f, 0x01, b'a', b'b'], 1, seed, true),
            Some(0xf8d3_c940)
        );
        // The other two algorithms, from the same source. The legacy hash
        // takes no seed; TEA does.
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 0, [0; 4], false),
            Some(0xd417_b560)
        );
        assert_eq!(
            dx_hash_name(b"libpsx.so.2", 2, seed, false),
            Some(0x9b01_6660)
        );
    }

    /// A directory made indexed by [`make_indexed`], plus its on-disk inode
    /// number and the device, for the raw reads the tree checks need.
    fn indexed_dir(total_blocks: usize) -> (Arc<MemDev>, Ext2Fs, Arc<dyn Inode>, u32, u32) {
        let dev = Arc::new(format_sized(total_blocks));
        let ino = {
            let fs = mounted(dev.clone());
            fs.root()
                .create("d", NodeKind::Dir)
                .expect("mkdir")
                .metadata()
                .ino as u32
        };
        // The leaf goes right after the block the directory already owns -
        // on this little image that first block is always block 14, and a
        // leaf dropped on it would be the root indexing itself.
        let leaf_block = inode_word(&dev, ino, 40) + 1;
        make_indexed(&dev, ino, leaf_block);
        // A fresh mount: the block cache is write-through, and the first
        // mount's slots still hold the blocks the raw surgery just rewrote
        // underneath it.
        let fs = mounted(dev.clone());
        let dir = fs.root().lookup("d").expect("the directory");
        (dev, fs, dir, ino, leaf_block)
    }

    #[test]
    fn an_indexed_directory_takes_an_insert() {
        let (dev, _fs, dir, ino, _leaf_block) = indexed_dir(128);

        // The apk shape that started all this: a versioned library under a
        // symlink chain, laid into /usr/lib the way symlinkat does.
        dir.symlink("libllhttp.so.9.4", "libllhttp.so.9.4.1")
            .expect("insert into the indexed directory");
        let link = dir
            .lookup("libllhttp.so.9.4")
            .expect("the link resolves through the index");
        assert_eq!(link.readlink().expect("readlink"), "libllhttp.so.9.4.1");

        // The entry landed in the leaf the index names, not in the root
        // block's dead slack, and the index still counts its alias only.
        let root_block = inode_word(&dev, ino, 40);
        let leaf = inode_word(&dev, ino, 44) as usize * 1024;
        critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            assert_eq!(
                dir_find(&disk[leaf..leaf + 1024], "libllhttp.so.9.4").is_some(),
                true
            );
            assert_eq!(
                dir_find(
                    &disk[root_block as usize * 1024..root_block as usize * 1024 + 1024],
                    "libllhttp.so.9.4"
                ),
                None
            );
            assert_eq!(le16(&disk, root_block as usize * 1024 + 34), 1); // count
        });
    }

    #[test]
    fn a_growing_indexed_directory_splits_leaves_and_stays_consistent() {
        let (_dev, fs, dir, ino, _leaf) = indexed_dir(512);

        let names: Vec<String> = (0..200)
            .map(|i| alloc::format!("entry-{i:04}-padpadpad"))
            .collect();
        for name in &names {
            dir.create(name, NodeKind::File).expect("insert");
        }
        for name in &names {
            dir.lookup(name).expect("every name is findable");
        }

        // The tree agrees with itself: for every name, the leaf the index
        // probe names for its hash is the leaf the name actually lives in -
        // which is exactly the property a Linux lookup depends on.
        let core = &fs.core;
        let data = core.read_inode(ino);
        let blocks = data.size as u64 / 1024;
        assert!(blocks > 2, "the test must have forced leaf splits");
        for name in &names {
            let hash = dx_hash_name(name.as_bytes(), 1, [0; 4], false).expect("hash");
            let path = core.dx_probe(&data, hash).expect("probe");
            let mut home = None;
            for l in 0..blocks {
                let block = core.bmap_lookup(&data, l).expect("mapped");
                let mut page = vec![0u8; 1024];
                core.read_bytes(block, 0, &mut page);
                if dir_find(&page, name).is_some() {
                    home = Some(l);
                    break;
                }
            }
            assert_eq!(home, Some(path.leaf), "{name} lives outside its leaf");
        }
    }

    #[test]
    fn a_full_root_index_answers_enospc_like_linux() {
        let (_dev, _fs, dir, _ino, _leaf) = indexed_dir(512);

        // 100-byte names: 108-byte entries, nine to a 1 KiB leaf. The root's
        // index limit at this block size is 124 entries, so the tree tops
        // out at 124 leaves and the next split is refused - with ENOSPC,
        // exactly what Linux ("directory index full") answers here.
        let name = |i: usize| alloc::format!("n{:0>99}", i);
        let mut inserted = 0;
        loop {
            match dir.create(&name(inserted), NodeKind::File) {
                Ok(_) => inserted += 1,
                Err(VfsError::NoSpace) => break,
                Err(e) => panic!("unexpected error at {inserted}: {e:?}"),
            }
        }
        assert!(inserted > 500, "only {inserted} entries before ENOSPC");
        // The refusal is clean: everything already inserted is intact, and
        // the refusal repeats.
        for i in 0..inserted {
            dir.lookup(&name(i)).expect("still there");
        }
        assert!(matches!(
            dir.create(&name(inserted), NodeKind::File),
            Err(VfsError::NoSpace)
        ));
    }

    #[test]
    fn removing_from_an_indexed_directory_leaves_the_index_alone() {
        let (dev, _fs, dir, ino, _leaf) = indexed_dir(512);

        let names: Vec<String> = (0..40)
            .map(|i| alloc::format!("victim-{i:03}-padding"))
            .collect();
        for name in &names {
            dir.create(name, NodeKind::File).expect("insert");
        }
        let root_block = inode_word(&dev, ino, 40);
        let count_before = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le16(&disk, root_block as usize * 1024 + 34) as usize
        });
        assert!(count_before > 1, "the inserts must have split leaves");

        // Half the names go. A removal never touches the index - it cannot
        // move an entry between leaves - so the root's count is untouched.
        for name in names.iter().take(20) {
            dir.unlink(name).expect("unlink");
        }
        let count_after = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            le16(&disk, root_block as usize * 1024 + 34) as usize
        });
        assert_eq!(count_after, count_before);
        for name in names.iter().skip(20) {
            dir.lookup(name).expect("survivor");
        }
        for name in names.iter().take(20) {
            assert!(dir.lookup(name).is_err(), "{name} should be gone");
        }
    }

    #[test]
    fn a_corrupt_index_degrades_to_a_linear_directory() {
        let (dev, _fs, _dir, ino, _leaf) = indexed_dir(128);
        // Scramble the index: a hash version nothing implements.
        let root_block = inode_word(&dev, ino, 40) as usize * 1024;
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            disk[root_block + 28] = 9;
        });
        // A fresh mount, so the fs reads the scrambled block rather than the
        // cached copy from before the surgery.
        let dir = {
            let fs = mounted(dev.clone());
            fs.root().lookup("d").expect("the directory")
        };

        dir.create("after", NodeKind::File)
            .expect("a bad index does not stop inserts");
        dir.lookup("after").expect("the name is there");
        // The flag went with the fallback, and the directory is linear now.
        assert_eq!(inode_word(&dev, ino, 32) & INODE_INDEX_FL, 0);
    }

    #[test]
    fn a_full_index_node_splits_when_the_root_has_room() {
        let (dev, _fs, _dir, ino, _leaf) = indexed_dir(512);

        // Hand-build a two-level tree: root -> node -> leaf, the shape ext4
        // with `largedir` makes and the only way an intermediate level comes
        // to exist at all. The node is full: 127 entries, its exact limit.
        // Every entry hashes 0x10 and points at the one leaf, whose eight
        // 120-byte entries leave no slack.
        let dir_block = inode_word(&dev, ino, 40) as usize;
        let (node_block, leaf_block) = (dir_block + 2, dir_block + 3);
        put32_wrapper(&dev, ino, 44, node_block as u32); // block[1] = node
        put32_wrapper(&dev, ino, 48, leaf_block as u32); // block[2] = leaf
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let base = dir_block * 1024;
            // Inode: two more blocks.
            let iat = TABLE + (ino as usize - 1) * 128;
            put32(&mut disk, iat + 4, 3 * 1024);
            put32(&mut disk, iat + 28, 6);
            // Root info: one intermediate level below the root.
            disk[base + 30] = 1;
            // Node: fake dirent over the whole block; limit 127; count 127,
            // entry 0 the alias naming the leaf, 126 real entries all
            // hashing 0x10 and pointing at the leaf.
            let node = node_block * 1024;
            put16(&mut disk, node + 4, 1024);
            put16(&mut disk, node + 8, 127); // limit
            put16(&mut disk, node + 10, 127); // count
            // Index entries hold LOGICAL block numbers: the leaf is
            // block[2], logical 2.
            put32(&mut disk, node + 12, 2); // alias block: the leaf
            for j in 1..127usize {
                put32(&mut disk, node + 8 + j * 8, 0x10); // hash
                put32(&mut disk, node + 8 + j * 8 + 4, 2);
            }
            // Leaf: eight maximal entries, no slack, no holes.
            let leaf = leaf_block * 1024;
            for i in 0..8u32 {
                let at = leaf + i as usize * 128;
                put32(&mut disk, at, 100 + i); // a plausible inode
                put16(&mut disk, at + 4, 128);
                disk[at + 6] = 120;
                disk[at + 7] = FT_REG;
                for (j, b) in disk[at + 8..at + 8 + 120].iter_mut().enumerate() {
                    *b = if j < 3 { b'0' + i as u8 } else { b'x' };
                }
            }
            for b in [node_block, leaf_block] {
                disk[3 * 1024 + (b - 1) / 8] |= 1 << ((b - 1) % 8);
            }
            for free in [2 * 1024 + 12, 1024 + 12] {
                let n = le16(&disk, free) - 1;
                put16(&mut disk, free, n);
            }
        });
        // A fresh mount, past the cached blocks the surgery rewrote.
        let fs = mounted(dev.clone());
        let core = &fs.core;
        let dir = fs.root().lookup("d").expect("the directory");

        // The name hashes past every 0x10 entry of the node, so the probe
        // lands in the last leaf - full - with the node above it full too,
        // and the root (one entry, limit 124) holding the only slot. The
        // insert has to split the node, restart, and split the leaf.
        dir.create("new-name", NodeKind::File)
            .expect("the insert goes through the two splits");
        dir.lookup("new-name").expect("the name is findable");

        // Root gained the sibling node; the node's halves split 63/64; the
        // leaf gained a sibling; the directory grew by two blocks.
        let data = core.read_inode(ino);
        let base = data.block[0] as usize * 1024;
        let (root_count, node_count) = critical_section::with(|cs| {
            let disk = dev.bytes.borrow(cs).borrow();
            (
                le16(&disk, base + 34),
                le16(&disk, data.block[1] as usize * 1024 + 8 + 2),
            )
        });
        assert_eq!(root_count, 2);
        assert_eq!(node_count, 63);
        assert_eq!(data.size, 5 * 1024);
        // And the filler entries of the original leaf are all still there.
        let mut found = 0;
        for l in 1..data.size as u64 / 1024 {
            let block = core.bmap_lookup(&data, l).expect("mapped");
            let mut page = vec![0u8; 1024];
            core.read_bytes(block, 0, &mut page);
            let mut at = 0;
            while at + 8 <= page.len() {
                let rec = le16(&page, at + 4) as usize;
                if rec < 8 {
                    break;
                }
                if le32(&page, at) != 0 && page[at + 6] != 0 {
                    found += 1;
                }
                at += rec;
            }
        }
        assert_eq!(found, 9); // eight fillers plus the new name
    }

    /// The test fixture's disk writes: one raw word into inode `ino`.
    fn put32_wrapper(dev: &MemDev, ino: u32, offset: usize, value: u32) {
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            put32(&mut disk, TABLE + (ino as usize - 1) * 128 + offset, value);
        });
    }
}
