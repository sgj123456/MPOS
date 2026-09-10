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
const S_IFMT: u16 = 0xf000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFCHR: u16 = 0x2000;
const INCOMPAT_FILETYPE: u32 = 1 << 1;
/// The directory is htree-indexed: a format this driver does not walk.
const INODE_INDEX_FL: u32 = 1 << 12;
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
            _ => NodeKind::File,
        }
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
}

impl Core {
    fn mount(dev: Arc<dyn BlockDev>) -> Result<Core, &'static str> {
        // The superblock is read before the cache exists: the cache's slot
        // size is one of the things it decides.
        let mut bytes = [0u8; SUPERBLOCK_BYTES];
        dev.read_at(SUPERBLOCK_AT, &mut bytes);
        let sb = Super::parse(&bytes)?;
        let groups = (sb.blocks_count - sb.first_data_block).div_ceil(sb.blocks_per_group);
        let block_size = 1024usize << sb.log_block_size;
        Ok(Core {
            dev,
            sb,
            groups,
            cache: Mutex::new(RefCell::new(Cache::new(block_size))),
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
            free_inodes: le16(&bytes, 16),
        }
    }

    /// Push a change in free block/inode counts to both the group descriptor
    /// and the superblock, so the on-disk bookkeeping is right after every
    /// single allocation rather than at some future flush.
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
                desc_off + 16,
                (le16(page, desc_off + 16) as i32 + inodes) as u16,
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
    fn free_blocks_of(&self, data: &InodeData) {
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
        self.dir_writable(dir)?;
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
        self.dir_writable(dir)?;
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
                    _ => NodeKind::File,
                };
                found.push((String::from(name), kind));
                true
            });
        }
        Ok(found)
    }

    /// Append `name -> child` to a directory, carving slack out of existing
    /// blocks first and growing the directory by one block only when nothing
    /// fits. The caller owns the inode rewrite; `dir` is mutated in place so
    /// size and block count stay truthful.
    fn dir_add(&self, dir: &mut InodeData, name: &str, child: u32, ftype: u8) -> VfsResult<()> {
        self.dir_writable(dir)?;
        assert!(
            !name.is_empty() && name.len() <= 255,
            "ext2 names are 1..=255 bytes"
        );
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
    fn dir_remove(&self, dir: &mut InodeData, name: &str) -> VfsResult<u32> {
        self.dir_writable(dir)?;
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

    fn dir_writable(&self, dir: &InodeData) -> VfsResult<()> {
        if dir.flags & INODE_INDEX_FL != 0 {
            return Err(VfsError::Unsupported);
        }
        Ok(())
    }

    /// A brand-new inode: reserved fields zeroed, mode and links set, and for
    /// a directory a first block holding "." and "..".
    fn new_inode(&self, kind: NodeKind, parent: u32) -> VfsResult<u32> {
        let ino = self.alloc_inode()?;
        let block_size = self.block_size();
        let mut data = InodeData {
            mode: match kind {
                NodeKind::Dir => S_IFDIR | 0o755,
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

// ---- the VFS seam ----

/// An ext2 filesystem mounted through the VFS.
pub struct Ext2Fs {
    core: Arc<Core>,
}

impl FileSystem for Ext2Fs {
    fn root(&self) -> Arc<dyn Inode> {
        self.inode_of(ROOT_INO)
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
        Arc::new(Ext2Ino {
            fs: self.core.clone(),
            ino,
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
        Arc::new(Ext2Ino {
            fs: self.fs.clone(),
            ino,
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
            size: if kind == NodeKind::Dir {
                0
            } else {
                data.size as u64
            },
        }
    }

    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn Inode>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        Ok(self.wrap(child))
    }

    fn create(&self, name: &str, kind: NodeKind) -> VfsResult<Arc<dyn Inode>> {
        if kind == NodeKind::CharDev {
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
        self.require_dir()?;
        let mut dir = self.fs.read_inode(self.ino);
        // Looked up before anything is touched, so a refusal leaves the
        // directory exactly as it was.
        let child = self.fs.dir_lookup(&dir, name)?.ok_or(VfsError::NoEntry)?;
        let mut data = self.fs.read_inode(child);
        if data.kind() == NodeKind::Dir {
            return Err(VfsError::IsDir);
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

    fn read_dir(&self) -> VfsResult<Vec<(String, NodeKind)>> {
        self.require_dir()?;
        let dir = self.fs.read_inode(self.ino);
        self.fs.dir_entries(&dir)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let data = self.fs.read_inode(self.ino);
        if data.kind() != NodeKind::File {
            return Err(VfsError::IsDir);
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
        let mut data = self.fs.read_inode(self.ino);
        if data.kind() != NodeKind::File {
            return Err(VfsError::IsDir);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;

    /// An in-memory disk for host tests: a `Vec<u8>` behind the house lock.
    struct MemDev {
        bytes: Mutex<RefCell<Vec<u8>>>,
    }

    impl MemDev {
        fn new() -> MemDev {
            MemDev {
                bytes: Mutex::new(RefCell::new(vec![0u8; 128 * 1024])),
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
    // 3 block bitmap, 4 inode bitmap, 5..12 inode table (64 inodes at 128
    // bytes), 13 root directory, 14 onward data.
    const TABLE: usize = 5 * 1024;
    const ROOT_DIR_BLOCK: u32 = 13;

    /// A minimal 128-block, 1024-byte, one-group ext2 with an empty root
    /// directory. Enough structure to mount, and to hand-place files into.
    fn format() -> MemDev {
        let dev = MemDev::new();
        critical_section::with(|cs| {
            let mut disk = dev.bytes.borrow(cs).borrow_mut();
            let disk = &mut *disk;
            let sb = 1024;
            put32(disk, sb, 64); // inodes_count
            put32(disk, sb + 4, 128); // blocks_count
            put32(disk, sb + 12, 115); // free_blocks_count
            put32(disk, sb + 16, 62); // free_inodes_count
            put32(disk, sb + 20, 1); // first_data_block
            put32(disk, sb + 24, 0); // log_block_size: 1024
            put32(disk, sb + 28, 0); // fragments == blocks
            put32(disk, sb + 32, 8192); // blocks_per_group
            put32(disk, sb + 36, 8192); // frags_per_group
            put32(disk, sb + 40, 64); // inodes_per_group
            put16(disk, sb + 56, MAGIC);
            put16(disk, sb + 58, 1); // clean
            put32(disk, sb + 76, 1); // rev 1
            put32(disk, sb + 84, 11); // first_ino
            put16(disk, sb + 88, 128); // inode_size
            put32(disk, sb + 96, INCOMPAT_FILETYPE);

            put32(disk, 2 * 1024, 3); // block bitmap
            put32(disk, 2 * 1024 + 4, 4); // inode bitmap
            put32(disk, 2 * 1024 + 8, 5); // inode table
            put16(disk, 2 * 1024 + 12, 115); // free blocks
            put16(disk, 2 * 1024 + 16, 62); // free inodes

            // Blocks 1..=13 are metadata and the root directory. On a 1 KiB
            // filesystem block 0 lives outside every group, so bitmap bit b is
            // block b + 1: bits 0..=12 here.
            disk[3 * 1024] = 0xff;
            disk[3 * 1024 + 1] = 0x1f;
            disk[4 * 1024] = 0b0000_0011; // inodes 1 (bad) and 2 (root)

            let root = TABLE + 128; // inode 2, index 1
            put16(disk, root, S_IFDIR | 0o755);
            put32(disk, root + 4, 1024); // size
            put16(disk, root + 26, 2); // links
            put32(disk, root + 28, 2); // blocks in sectors
            put32(disk, root + 40, ROOT_DIR_BLOCK);

            // "." and "..", the latter owning the rest of the block.
            write_dirent(&mut disk[13 * 1024..], 0, 2, dirent_len(1), ".", FT_DIR);
            write_dirent(
                &mut disk[13 * 1024..],
                dirent_len(1),
                2,
                1024 - dirent_len(1),
                "..",
                FT_DIR,
            );
        });
        dev
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
            // its slack.
            let root = &mut disk[ROOT_DIR_BLOCK as usize * 1024..];
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
        assert!(Ext2Fs::mount(Arc::new(MemDev::new())).is_err());
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
        assert_eq!(dir.metadata().size, 0, "directories report size 0");

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
}
