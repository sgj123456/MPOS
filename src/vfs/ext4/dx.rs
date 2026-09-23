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
/// structure, one entry per eight bytes.
pub(crate) fn dx_root_limit(block_size: usize, info_length: usize) -> usize {
    (block_size - dirent_len(1) - dirent_len(2) - info_length) / 8
}

/// How many entries an intermediate index block can hold: the block less the
/// fake dirent that stands in for the header.
pub(crate) fn dx_node_limit(block_size: usize) -> usize {
    (block_size - DIRENT_HEADER) / 8
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
    pub(crate) fn dx_split_leaf(
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
        dir.size += block_size as u64;
        dir.blocks += self.block_units(dir.flags);
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
    pub(crate) fn dx_split_node(
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
pub(crate) fn dx_move_dirents(from: &mut [u8], to: &mut [u8], map: &[(u32, usize, usize)]) {
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
pub(crate) fn dx_pack_dirents(page: &mut [u8]) {
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
