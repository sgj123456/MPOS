use super::*;

impl Core {
    // ---- groups and allocation ----

    /// Whether every pointer a group's descriptor offers is a block the volume
    /// has.
    ///
    /// A descriptor is the one structure whose numbers *everything* else is
    /// read through - the two bitmaps the allocator writes and the inode table
    /// every inode is found in - so a single bad word in it turns each of those
    /// reads into a request for a block outside the filesystem. Refusing the
    /// group is the answer with the smallest blast radius: the allocator moves
    /// on to the next one, and a read of an inode it holds comes back as the
    /// empty record that means "no such file".
    pub(crate) fn group_is_live(&self, g: &Group) -> bool {
        // Each pointer is checked where it starts, not for the run it covers:
        // the last group of a real volume legitimately has a table that is
        // truncated by the end of the disk, and e2fsck leaves it alone. A slot
        // past that end is refused by `inode_pos`, which knows which slot it is
        // asking for.
        self.block_is_live(g.block_bitmap)
            && self.block_is_live(g.inode_bitmap)
            && self.block_is_live(g.inode_table)
    }

    pub(crate) fn group(&self, g: u32) -> Group {
        assert!(g < self.groups, "ext2 group {g} out of range");
        let block_size = self.block_size();
        let desc_size = self.sb.desc_size as usize;
        let at = g as usize * desc_size;
        let block = self.sb.first_data_block as u64 + 1 + (at / block_size) as u64;
        // Read only as much as the descriptor really is: a 32-byte one is not
        // followed by the high halves, and the bytes after it belong to the
        // next group.
        let mut bytes = [0u8; GROUP_DESC_BYTES_MAX];
        self.read_bytes(block, at % block_size, &mut bytes[..desc_size]);
        // The high halves live at 32, 36 and 40 - a word each, sixty-four
        // bits up the descriptor, not the next field along. They exist only
        // in the 64-byte layout; a 32-byte one simply has 32-bit pointers.
        let high = desc_size >= 64;
        let wide = |lo: usize, at: usize| {
            le32(&bytes, lo) as u64
                | if high {
                    (le32(&bytes, at) as u64) << 32
                } else {
                    0
                }
        };
        Group {
            block_bitmap: wide(0, 32),
            inode_bitmap: wide(4, 36),
            inode_table: wide(8, 40),
            free_blocks: le16(&bytes, 12) as u32
                + if desc_size >= 46 {
                    (le16(&bytes, 44) as u32) << 16
                } else {
                    0
                },
            free_inodes: le16(&bytes, 14) as u32
                + if desc_size >= 48 {
                    (le16(&bytes, 46) as u32) << 16
                } else {
                    0
                },
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
    pub(crate) fn bump_free(&self, g: u32, blocks: i32, inodes: i32) {
        let block_size = self.block_size();
        let desc_size = self.sb.desc_size as usize;
        let at = g as usize * desc_size;
        let desc_block = self.sb.first_data_block as u64 + 1 + (at / block_size) as u64;
        let desc_off = at % block_size;
        // The same two numbers, twice, in two shapes: u16 pairs in the
        // descriptor (with a high half once the layout is 64 bytes) and u32s
        // in the superblock (with a high word once block numbers are 64 bits).
        let wide = desc_size >= 64;
        self.update_block(desc_block, |page| {
            // Read the pair as one number, add, write the pair back: a carry
            // out of the low u16 is the whole point of the high one, and
            // adding to each half separately loses it.
            let bump = |page: &mut [u8], lo: usize, hi: usize, delta: i32| {
                let was = le16(page, lo) as u32
                    + if wide {
                        (le16(page, hi) as u32) << 16
                    } else {
                        0
                    };
                let now = (was as i64 + delta as i64) as u32;
                set16(page, lo, now as u16);
                if wide {
                    set16(page, hi, (now >> 16) as u16);
                }
            };
            bump(page, desc_off + 12, desc_off + 44, blocks);
            bump(page, desc_off + 14, desc_off + 46, inodes);
            true
        });
        self.update_block(self.sb.sb_block, |page| {
            let at = self.sb.sb_off;
            if self.sb.has_64bit {
                // Read the pair as one number: a carry out of the low word is
                // the whole point of the high one, and adding to each half
                // separately loses it.
                let was = le32(page, at + 12) as u64 | ((le32(page, at + 344) as u64) << 32);
                let now = (was as i64 + blocks as i64) as u64;
                set32(page, at + 12, now as u32);
                // 344, not 340: 340 is the high half of the *reserved* count.
                set32(page, at + 344, (now >> 32) as u32);
            } else {
                set32(
                    page,
                    at + 12,
                    (le32(page, at + 12) as i64 + blocks as i64) as u32,
                );
            }
            // Inodes are counted in 32 bits everywhere: there is no high word
            // for them to carry into.
            set32(
                page,
                at + 16,
                (le32(page, at + 16) as i64 + inodes as i64) as u32,
            );
            if self.sb.has_metadata_csum {
                // The counters above live inside what the superblock's own
                // checksum covers; leaving the old value behind would make
                // this write a write with a lie attached.
                let c = self::csum::sb_csum(&page[at..at + 1020]);
                set32(page, at + 1020, c);
            }
            true
        });
        // The counts just moved are inside the descriptor the group's own
        // checksum covers - and the bitmaps that changed hands with them
        // carry checksums of their own. One call puts the group back to
        // self-consistent.
        self.refresh_group_csums(g);
    }

    pub(crate) fn alloc_block(&self) -> VfsResult<u64> {
        let per_group = self.sb.blocks_per_group as u64;
        for g in 0..self.groups {
            let group = self.group(g);
            if group.free_blocks == 0 || !self.group_is_live(&group) {
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
                    if block < self.sb.blocks_count {
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

    /// Record that this group's inodes have now been used as far as table
    /// index `idx`: the `bg_itable_unused` tail shrinks to keep that inode
    /// inside the used region. Lives only in the 64-byte descriptor, where
    /// the field sits at 28 with its high half at 50.
    fn push_itable_high_water(&self, g: u32, idx: u32) {
        if !self.sb.has_64bit {
            return;
        }
        let per = self.sb.inodes_per_group;
        let desc_size = self.sb.desc_size as usize;
        let bs = self.block_size();
        let at = g as usize * desc_size;
        let block = self.sb.first_data_block as u64 + 1 + (at / bs) as u64;
        let off = at % bs;
        self.update_block(block, |page| {
            let was = le16(page, off + 28) as u32
                + if desc_size >= 52 {
                    (le16(page, off + 50) as u32) << 16
                } else {
                    0
                };
            if idx < per.saturating_sub(was) {
                return false;
            }
            let unused = per - (idx + 1);
            set16(page, off + 28, unused as u16);
            if desc_size >= 52 {
                set16(page, off + 50, (unused >> 16) as u16);
            }
            true
        });
    }

    pub(crate) fn alloc_inode(&self) -> VfsResult<u32> {
        let per_group = self.sb.inodes_per_group as u64;
        for g in 0..self.groups {
            let group = self.group(g);
            if group.free_inodes == 0 || !self.group_is_live(&group) {
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
                // The bitmap offered a number the volume says it does not have.
                // Both words are disk data and neither is authoritative over
                // the other, so the honest answer is that the volume is
                // inconsistent: refuse the allocation and leave the bit set -
                // an inode marked used that is not costs one slot, while the
                // reverse hands one inode to two owners.
                if ino > self.sb.inodes_count as u64 {
                    return Err(VfsError::Corrupt);
                }
                // An inode taken from the table's never-used tail moves the
                // group's unused count down with it: that count is not an
                // opinion about free space but a promise about which table
                // slots were never written, and a used inode inside the
                // promised-virgin tail is the error e2fsck names.
                self.push_itable_high_water(g, ((ino - 1) % per_group) as u32);
                self.bump_free(g, 0, -1);
                let Some((block, off)) = self.inode_pos(ino as u32) else {
                    return Err(VfsError::Corrupt);
                };
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
    pub(crate) fn free_block(&self, block: u64) {
        let per_group = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;
        // The number arrives out of an inode's own mapping, so it is disk data.
        // One below the first data block would underflow the subtraction here
        // and one in a group past the volume would index a descriptor that does
        // not exist. Neither is this filesystem's block to give back, and
        // leaving its bit set is the safe direction to be wrong in: the count
        // comes out one block short, where freeing a block twice hands the same
        // one to two owners.
        if block < first {
            return;
        }
        let g = ((block - first) / per_group) as u32;
        if g >= self.groups {
            return;
        }
        let bit = (block - first) % per_group;
        let group = self.group(g);
        if !self.group_is_live(&group) {
            return;
        }
        self.update_block(group.block_bitmap, |page| {
            page[(bit / 8) as usize] &= !(1u8 << (bit % 8));
            true
        });
        self.bump_free(g, 1, 0);
    }

    /// Free every block `data` maps. A block map's are the direct ones plus
    /// whatever the two levels of indirection reach - the indirect blocks
    /// themselves last, once nothing points through them - and a zero pointer
    /// is a hole, something nothing was ever allocated for. An extent tree's
    /// are the runs its leaves name, plus the tree's own blocks.
    ///
    /// A fast symlink has no blocks to free and must not be walked: its
    /// "pointers" are the *text* of the name it stands for, and a target that
    /// reads as a number would take blocks out of somebody else's file -
    /// or, when it does not read as a plausible one at all, walk off the end
    /// of the group table and take the kernel with it.
    pub(crate) fn free_blocks_of(&self, data: &InodeData) {
        if data.is_fast_symlink() {
            return;
        }
        if self.is_extent_mapped(data) {
            self.free_extents_of(data);
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
    pub(crate) fn free_indirect(&self, block: u64, per_block: usize, double: bool) {
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
    /// Release an inode whose last name is gone: its data blocks first, then
    /// its bitmap bit. Each block puts its own count back as it goes, so
    /// only the inode is counted here.
    pub(crate) fn free_inode(&self, ino: u32) {
        if !self.inode_is_live(ino) {
            return;
        }
        self.free_blocks_of(&self.read_inode(ino));
        let g = (ino - 1) / self.sb.inodes_per_group;
        let index = (ino - 1) % self.sb.inodes_per_group;
        let group = self.group(g);
        if !self.group_is_live(&group) {
            return;
        }
        self.update_block(group.inode_bitmap, |page| {
            page[(index / 8) as usize] &= !(1u8 << (index % 8));
            true
        });
        self.bump_free(g, 0, 1);
    }

    /// Whether `ino` is a number that could name an inode of this volume at
    /// all, decided from the superblock alone: inode 0 is reserved, every
    /// position below is counted from 1, and the table cannot hold more than
    /// `groups * inodes_per_group` of them. Whether the group it lands in is a
    /// group this disk really has is the next question, and
    /// [`inode_pos`](Self::inode_pos) answers that one.
    pub(crate) fn inode_is_live(&self, ino: u32) -> bool {
        ino != 0
            && ino <= self.sb.inodes_count
            && u64::from(ino - 1) < u64::from(self.groups) * u64::from(self.sb.inodes_per_group)
    }

    /// Block and offset of inode `ino`, or `None` when the volume has no such
    /// place.
    ///
    /// `Ext4Fs::inode_of` hands out a handle for any `u32`, and a directory
    /// entry can carry any `u32` too, so this is where an invented inode number
    /// stops being a read request for a block outside the filesystem. The
    /// answer for it is the same one the format has for an empty table slot:
    /// there is no record there, which every caller already reads as a file
    /// that does not exist.
    pub(crate) fn inode_pos(&self, ino: u32) -> Option<(u64, usize)> {
        if !self.inode_is_live(ino) {
            return None;
        }
        let index = (ino - 1) % self.sb.inodes_per_group;
        let group = self.group((ino - 1) / self.sb.inodes_per_group);
        if !self.group_is_live(&group) {
            return None;
        }
        let off = index as usize * self.sb.inode_size as usize;
        let block = group.inode_table + (off / self.block_size()) as u64;
        self.block_is_live(block)
            .then_some((block, off % self.block_size()))
    }
}
