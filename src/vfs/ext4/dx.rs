use super::*;

/// An htree never grows deeper than one intermediate index level below the
/// root - the shape every ext2/ext4 without the `largedir` feature makes.
pub(crate) const HTREE_MAX_LEVELS: u8 = 1;

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
pub(crate) struct DxRoot {
    pub(crate) hash_version: u8,
    pub(crate) indirect_levels: u8,
    /// Where the entries begin: right after the info block, whose length is
    /// itself recorded (and always 8 in every tree anyone has made).
    pub(crate) entries_at: usize,
}

impl DxRoot {
    /// Parse the root's info block, or `None` for an index this driver does
    /// not walk. The caller falls back to a linear insert and lets the flag
    /// go, which is what Linux's own checksum-less fallback does.
    pub(crate) fn parse(page: &[u8]) -> Option<DxRoot> {
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
pub(crate) struct DxFrame {
    pub(crate) block: u64,
    pub(crate) entries_at: usize,
    pub(crate) at: usize,
}

/// The path down the index: the root, an optional intermediate level, and
/// the logical block of the leaf that owns the hash.
pub(crate) struct DxPath {
    pub(crate) root: DxFrame,
    pub(crate) node: Option<DxFrame>,
    pub(crate) leaf: u64,
}

pub(crate) fn dx_count(page: &[u8], entries_at: usize) -> usize {
    le16(page, entries_at + 2) as usize
}

pub(crate) fn dx_limit(page: &[u8], entries_at: usize) -> usize {
    le16(page, entries_at) as usize
}

pub(crate) fn dx_entry_hash(page: &[u8], entries_at: usize, i: usize) -> u32 {
    le32(page, entries_at + i * 8)
}

/// The logical block an entry points at. The top four bits are reserved and
/// never part of a block number.
pub(crate) fn dx_entry_block(page: &[u8], entries_at: usize, i: usize) -> u32 {
    le32(page, entries_at + i * 8 + 4) & 0x0fff_ffff
}

pub(crate) fn dx_set_count(page: &mut [u8], entries_at: usize, count: usize) {
    set16(page, entries_at + 2, count as u16);
}

/// How many entries a root can hold: the block less ".", ".." and the info
/// structure and the eight-byte tail they all stop short of, one entry per
/// eight bytes. On a 1 KiB block that is 123, which is what a real `mkfs`
/// volume records and what a probe compares against.
pub(crate) fn dx_root_limit(block_size: usize, info_length: usize) -> usize {
    (block_size - dirent_len(1) - dirent_len(2) - info_length - 8) / 8
}

/// How many entries an intermediate index block can hold: the block less the
/// fake dirent that stands in for the header, less the tail at the end.
pub(crate) fn dx_node_limit(block_size: usize) -> usize {
    (block_size - DIRENT_HEADER - 8) / 8
}

/// Shift the index entries after slot `at` up by one and put
/// `hash -> block` into the gap: the entry a new leaf registers under.
pub(crate) fn dx_insert_entry(
    page: &mut [u8],
    entries_at: usize,
    at: usize,
    hash: u32,
    block: u32,
) {
    let count = dx_count(page, entries_at);
    let from = entries_at + (at + 1) * 8;
    page.copy_within(from..entries_at + count * 8, from + 8);
    set32(page, from, hash);
    set32(page, from + 4, block);
    dx_set_count(page, entries_at, count + 1);
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A minimal index root: the two words an htree checks before it walks
    /// anything, then the info block at 24.
    fn root(hash_version: u8, info_length: u8, indirect_levels: u8, unused_flags: u8) -> Vec<u8> {
        let mut page = vec![0u8; 128];
        page[28] = hash_version;
        page[29] = info_length;
        page[30] = indirect_levels;
        page[31] = unused_flags;
        page
    }

    /// A root is walked only when its info block is the shape this driver
    /// knows; anything else falls back to a linear insert rather than walking
    /// a tree it cannot trust.
    #[test]
    fn a_root_parses_only_its_own_shape() {
        let parsed = DxRoot::parse(&root(1, 8, 0, 0)).expect("a well-formed root");
        assert_eq!(parsed.hash_version, 1);
        assert_eq!(parsed.indirect_levels, 0);
        assert_eq!(
            parsed.entries_at, 32,
            "the entries begin past the info block"
        );

        // Hash versions 0..=2 are the ones with a hash this driver computes.
        assert_eq!(
            DxRoot::parse(&root(2, 8, 0, 0)).map(|r| r.hash_version),
            Some(2)
        );
        assert!(
            DxRoot::parse(&root(3, 8, 0, 0)).is_none(),
            "an unknown hash version is not walked"
        );
        // One intermediate level is all an ext2/ext4 without `largedir` has.
        assert!(DxRoot::parse(&root(1, 8, 1, 0)).is_some());
        assert!(
            DxRoot::parse(&root(1, 8, 2, 0)).is_none(),
            "a tree deeper than the format this driver makes"
        );
        // The info length is 8 in every tree anyone has made, and the flag
        // word's low bit is the "this index is broken" marker.
        assert!(
            DxRoot::parse(&root(1, 12, 0, 0)).is_none(),
            "info length other than eight"
        );
        assert!(
            DxRoot::parse(&root(1, 8, 0, 1)).is_none(),
            "an index flagged broken"
        );
    }

    /// The first word of the info block must be zero - anything else is a
    /// block that is not this structure.
    #[test]
    fn a_root_with_a_nonzero_reserved_word_is_not_an_index() {
        let mut page = root(1, 8, 0, 0);
        set32(&mut page, 24, 1);
        assert!(DxRoot::parse(&page).is_none());
    }

    /// Entry 0 is not an entry: its two halves are the count and the limit,
    /// and the real entries begin behind it. Entry `i` beyond that is an
    /// eight-byte pair - the hash, then the block, whose top four bits are
    /// reserved and never part of the number.
    #[test]
    fn an_entry_is_a_hash_and_a_block_behind_a_mask() {
        let mut page = vec![0u8; 64];
        let at = 32;
        dx_set_count(&mut page, at, 2);
        set16(&mut page, at, 9); // the limit word, entry 0's low half
        set32(&mut page, at + 8, 0x1234_5678); // entry 1's hash
        set32(&mut page, at + 12, 0xf000_0001); // its block, reserved bits set
        set32(&mut page, at + 16, 0xffff_ffff); // entry 2's hash

        assert_eq!(dx_count(&page, at), 2);
        assert_eq!(dx_limit(&page, at), 9);
        assert_eq!(dx_entry_hash(&page, at, 1), 0x1234_5678);
        assert_eq!(
            dx_entry_block(&page, at, 1),
            1,
            "the reserved top four bits are not part of the block"
        );
        assert_eq!(dx_entry_hash(&page, at, 2), 0xffff_ffff);
    }

    /// A root holds the block less `.`, `..`, the info block and the
    /// eight-byte tail, one entry per eight bytes - the 123 a real 1 KiB
    /// `mkfs` records.
    #[test]
    fn a_root_holds_what_the_block_leaves_it() {
        assert_eq!(dx_root_limit(1024, 8), 123);
        assert_eq!(dx_root_limit(4096, 8), (4096 - 12 - 12 - 8 - 8) / 8);
        // An intermediate block has no `.`/`..` - just the fake dirent header
        // that stands in for them.
        assert_eq!(dx_node_limit(4096), (4096 - DIRENT_HEADER - 8) / 8);
        assert_eq!(dx_node_limit(1024), (1024 - DIRENT_HEADER - 8) / 8);
    }

    /// Inserting puts the new pair in the slot *behind* the one it names and
    /// shifts what follows up - the ordering every probe below relies on.
    /// With entry 0 the header, inserting behind it lands at index 1.
    #[test]
    fn an_insert_puts_the_pair_behind_the_slot_it_names() {
        let mut page = vec![0u8; 64];
        let at = 32;
        dx_set_count(&mut page, at, 2); // the header and one real entry
        set32(&mut page, at + 8, 10);
        set32(&mut page, at + 12, 100);

        dx_insert_entry(&mut page, at, 0, 5, 50);

        assert_eq!(dx_count(&page, at), 3);
        assert_eq!(
            (dx_entry_hash(&page, at, 1), dx_entry_block(&page, at, 1)),
            (5, 50),
            "the new pair went into the slot behind the one named"
        );
        assert_eq!(
            (dx_entry_hash(&page, at, 2), dx_entry_block(&page, at, 2)),
            (10, 100),
            "the entry that was there shifted up"
        );
    }
}

impl Core {
    /// Walk the index to the leaf that owns `hash`, validating every level
    /// against the shape it must have. `Err` is "not a tree this driver
    /// walks" - a corrupt or foreign index, which the insert path answers by
    /// dropping the flag and going linear, as Linux does for a tree it
    /// cannot trust and cannot checksum.
    pub(crate) fn dx_probe(&self, dir: &InodeData, hash: u32) -> Result<DxPath, ()> {
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
    pub(crate) fn dir_add_indexed(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let root_disk = match self.bmap_lookup(dir, 0) {
            Some(block) => block,
            None => return self.dir_add_fallback(dir, dir_ino, name, child, ftype),
        };
        let mut root_page = vec![0u8; block_size];
        self.read_bytes(root_disk, 0, &mut root_page);
        let root = match DxRoot::parse(&root_page) {
            Some(root) => root,
            None => return self.dir_add_fallback(dir, dir_ino, name, child, ftype),
        };
        let Some(hash) = dx_hash_name(
            name.as_bytes(),
            root.hash_version,
            self.sb.hash_seed,
            self.sb.hash_unsigned,
        ) else {
            return self.dir_add_fallback(dir, dir_ino, name, child, ftype);
        };

        loop {
            let path = match self.dx_probe(dir, hash) {
                Ok(path) => path,
                Err(()) => return self.dir_add_fallback(dir, dir_ino, name, child, ftype),
            };
            let leaf_disk = self.bmap_lookup(dir, path.leaf).ok_or(VfsError::NoSpace)?;
            let mut leaf_page = vec![0u8; block_size];
            self.read_bytes(leaf_disk, 0, &mut leaf_page);
            trim_dir_tail(&mut leaf_page, self.leaf_limit(block_size));
            if dir_try_insert(
                &mut leaf_page,
                self.leaf_limit(block_size),
                name,
                child,
                ftype,
            ) {
                self.stamp_leaf_tail(&mut leaf_page, dir_ino, dir.generation);
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
                    dir_ino,
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
                        dir_ino,
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
    ///
    /// Splitting is the one operation that needs every part of the node at
    /// once - the directory, its inode number, the entry being added, the
    /// hash the level is keyed by and the hash being inserted - because each
    /// half of the pair is written from the other's state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dx_split_leaf(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
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
        let new_logical = dir.size / block_size as u64;
        let new_disk = self.bmap_ensure(dir, dir_ino, new_logical)?.0;
        dir.size += block_size as u64;
        dir.blocks += self.block_units(dir.flags);
        let mut new_page = vec![0u8; block_size];

        // Map the live entries and sort them by hash. The usable block ends
        // where the checksum trailer begins, and the last live entry's
        // record ends there too.
        let limit = self.leaf_limit(block_size);
        trim_dir_tail(&mut leaf_page, limit);
        let mut map: Vec<(u32, usize, usize)> = Vec::new();
        let mut at = 0;
        let mut tiled = true;
        while at < limit {
            let Some(rec) = entry_rec(&leaf_page, at, limit) else {
                tiled = false;
                break;
            };
            let ino = le32(&leaf_page, at);
            let name = entry_name(&leaf_page, at, rec);
            if ino != 0 && !name.is_empty() {
                let hash =
                    dx_hash_name(name, hash_version, self.sb.hash_seed, self.sb.hash_unsigned)
                        .unwrap_or(0);
                map.push((hash, at, rec));
            }
            at += rec;
        }
        // A leaf whose entries do not tile the block cannot be split: the
        // operation moves every entry in it, and an entry the walk could not
        // reach would simply be gone from the directory afterwards. `Corrupt`
        // refuses the insert and leaves the block alone, which is the whole
        // reason the walk above stops rather than asserting.
        if !tiled {
            return Err(VfsError::Corrupt);
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

        dx_move_dirents(&mut leaf_page, &mut new_page, &map[split..], limit);
        dx_pack_dirents(&mut leaf_page, limit);
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
            dir_try_insert(&mut new_page, limit, name, child, ftype)
        } else {
            dir_try_insert(&mut leaf_page, limit, name, child, ftype)
        };
        if !placed {
            placed = if into_new {
                dir_try_insert(&mut leaf_page, limit, name, child, ftype)
            } else {
                dir_try_insert(&mut new_page, limit, name, child, ftype)
            };
        }
        if !placed {
            return Err(VfsError::NoSpace);
        }
        self.stamp_leaf_tail(&mut leaf_page, dir_ino, dir.generation);
        self.stamp_leaf_tail(&mut new_page, dir_ino, dir.generation);
        self.stamp_dx_tail(&mut parent_page, parent.entries_at, dir_ino, dir.generation);
        self.write_bytes(leaf_disk, 0, &leaf_page);
        self.write_bytes(new_disk, 0, &new_page);
        self.write_bytes(parent_disk, 0, &parent_page);
        Ok(())
    }

    /// Split a full intermediate index node. The caller has checked the root
    /// has a slot: the node's upper half of entries moves to a fresh block,
    /// and the root's entry naming the old node gains a sibling for the new
    /// one, keyed by the first hash that moved.
    ///
    /// The same reason as the leaf split below: the two blocks that trade
    /// entries are each described by a node, a disk address and a page, and
    /// all six have to be in hand at once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dx_split_node(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
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

        let new_logical = dir.size / block_size as u64;
        let new_disk = self.bmap_ensure(dir, dir_ino, new_logical)?.0;
        dir.size += block_size as u64;
        dir.blocks += self.block_units(dir.flags);

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

        self.stamp_dx_tail(node_page, node_frame.entries_at, dir_ino, dir.generation);
        self.stamp_dx_tail(&mut new_page, DIRENT_HEADER, dir_ino, dir.generation);
        self.stamp_dx_tail(root_page, root_frame.entries_at, dir_ino, dir.generation);
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
    pub(crate) fn dir_add_fallback(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        dir.flags &= !INODE_INDEX_FL;
        self.dir_add_linear(dir, dir_ino, name, child, ftype)
    }
}

/// Move the mapped entries into a fresh leaf, packed to their minimal
/// lengths, the last one's record running to the end of the usable block -
/// which is where the checksum trailer starts, on a volume that has one.
/// Each source slot is emptied - inode zeroed, name wiped - so the
/// compaction of what stays never sees it.
pub(crate) fn dx_move_dirents(
    from: &mut [u8],
    to: &mut [u8],
    map: &[(u32, usize, usize)],
    limit: usize,
) {
    let mut at = 0;
    let last = map.len().saturating_sub(1);
    for (i, &(_hash, offs, size)) in map.iter().enumerate() {
        let own = dirent_len(from[offs + 6] as usize);
        to[at..at + own].copy_from_slice(&from[offs..offs + own]);
        set16(to, at + 4, own as u16);
        if i == last {
            set16(to, at + 4, (limit - at) as u16);
        }
        set32(from, offs, 0);
        for b in &mut from[offs + 6..offs + size] {
            *b = 0;
        }
        at += own;
    }
}

/// Compact the live entries of a leaf to their minimal lengths at the front
/// of the usable block; the last one's record runs to the end of it, as a
/// walker - and the trailer, where there is one - expect.
pub(crate) fn dx_pack_dirents(page: &mut [u8], limit: usize) {
    let (mut to, mut last) = (0usize, 0usize);
    let mut at = 0;
    while at + DIRENT_HEADER <= limit {
        let rec = le16(page, at + 4) as usize;
        if rec < DIRENT_HEADER || at + rec > limit {
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
    set16(page, last + 4, (limit - last) as u16);
}
