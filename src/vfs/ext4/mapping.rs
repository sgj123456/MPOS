use super::*;

impl Core {
    // ---- block mapping ----

    /// Pointer `i` of an indirect block; `None` for zero, i.e. unmapped.
    ///
    /// A pointer past the end of the volume is unmapped too. The word is a
    /// `u32`, so on a filesystem with 4 KiB blocks it can name a block the
    /// disk does not have, and the caller's answer for a hole is already the
    /// right one for that: read as zeros, never written to.
    pub(crate) fn ptr_at(&self, block: u64, i: usize) -> Option<u64> {
        let mut bytes = [0u8; 4];
        self.read_bytes(block, i * 4, &mut bytes);
        let value = le32(&bytes, 0);
        (value != 0 && self.block_is_live(value as u64)).then_some(value as u64)
    }

    pub(crate) fn put_ptr(&self, block: u64, i: usize, value: u32) {
        let mut bytes = [0u8; 4];
        set32(&mut bytes, 0, value);
        self.write_bytes(block, i * 4, &bytes);
    }

    /// Which disk block holds logical block `l`. `None` is a hole, or a block
    /// past what this inode maps.
    ///
    /// The inode says which of the two shapes it is, and the answer has to
    /// come from it rather than from the filesystem: an image converted from
    /// ext2 holds a block map and a tree side by side, and guessing wrong
    /// reads a file's first extent as a pointer to somebody else's data.
    pub(crate) fn bmap_lookup(&self, d: &InodeData, l: u64) -> Option<u64> {
        let block = if self.is_extent_mapped(d) {
            self.extent_lookup(d, l)
        } else {
            self.block_map_lookup(d, l)
        };
        // The same rule as a block map's entry, one layer up: an extent's
        // `ee_block` is a 32-bit word, so a tree can name a block past the
        // volume even when the volume itself is consistent. `None` is what the
        // caller already has for "this logical block maps nowhere".
        block.filter(|&block| self.block_is_live(block))
    }

    /// Map logical block `l` for writing, allocating anything missing. Returns
    /// the disk block and whether anything new was allocated, for i_blocks
    /// accounting.
    pub(crate) fn bmap_ensure(
        &self,
        d: &mut InodeData,
        ino: u32,
        l: u64,
    ) -> VfsResult<(u64, bool)> {
        let mapped = if self.is_extent_mapped(d) {
            self.extent_ensure(d, ino, l)
        } else {
            self.block_map_ensure(d, l)
        }?;
        // A block that exists has to be a block the volume has. Everything the
        // allocator hands out is, so a number that is not is one read out of
        // the inode - and the write that was asked for is refused rather than
        // landing somewhere outside the filesystem, where a later mount would
        // find two owners of one block.
        if !self.block_is_live(mapped.0) {
            return Err(VfsError::Corrupt);
        }
        Ok(mapped)
    }

    /// Drop every mapping an inode has. The two shapes empty differently: a
    /// block map zeroes fifteen words, an extent tree keeps its header and
    /// forgets its entries - the header is the only thing that says the tree
    /// is a tree, and zeroing it would turn the inode into a block map.
    pub(crate) fn reset_mapping(&self, d: &mut InodeData) {
        if self.is_extent_mapped(d) {
            let mut root = [0u8; EXTENT_ROOT_BYTES];
            ext_init_header(&mut root, 0);
            ext_put_root(d, &root);
        } else {
            d.block = [0; 15];
        }
    }

    pub(crate) fn block_map_lookup(&self, d: &InodeData, l: u64) -> Option<u64> {
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

    /// A block number a block-map pointer can hold. The map stores 32-bit
    /// numbers, so a filesystem whose blocks do not fit in one is a filesystem
    /// made with `64bit` - and every inode on it is extent-mapped, which is
    /// why reaching here with a number that big means the image disagrees
    /// with itself. Truncating it would point the inode at another block.
    pub(crate) fn block32(&self, block: u64) -> VfsResult<u32> {
        u32::try_from(block).map_err(|_| VfsError::FileTooBig)
    }

    pub(crate) fn block_map_ensure(&self, d: &mut InodeData, l: u64) -> VfsResult<(u64, bool)> {
        let per_block = (self.block_size() / 4) as u64;
        let direct = DIRECT_BLOCKS as u64;
        let mut allocated = false;
        let target = if l < direct {
            if d.block[l as usize] == 0 {
                d.block[l as usize] = self.block32(self.alloc_block()?)?;
                allocated = true;
            }
            d.block[l as usize] as u64
        } else if l < direct + per_block {
            if d.block[12] == 0 {
                d.block[12] = self.block32(self.alloc_block()?)?;
                allocated = true;
            }
            self.ensure_slot(d.block[12] as u64, (l - direct) as usize, &mut allocated)?
        } else if l < direct + per_block + per_block * per_block {
            if d.block[13] == 0 {
                d.block[13] = self.block32(self.alloc_block()?)?;
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

    pub(crate) fn ensure_slot(
        &self,
        indirect: u64,
        i: usize,
        allocated: &mut bool,
    ) -> VfsResult<u64> {
        match self.ptr_at(indirect, i) {
            Some(block) => Ok(block),
            None => {
                let block = self.alloc_block()?;
                self.put_ptr(indirect, i, self.block32(block)?);
                *allocated = true;
                Ok(block)
            }
        }
    }

    // ---- extent trees ----
    //
    // An extent is a run: "logical block L for the next N blocks lives at
    // physical block P". That is the whole difference from a block map, and
    // it is why writing is not a matter of poking one word. A block map maps
    // one block at a time and never runs out of room in the inode until all
    // fifteen words are gone; an extent tree maps runs out of *four* of them
    // in the inode and then has to move to blocks, splitting nodes as it
    // goes, which is the part that makes this a tree rather than a list.

    /// Which disk block holds logical block `l` of an extent-mapped inode.
    ///
    /// `None` means a hole. It also means "unwritten", which is a different
    /// thing that reads the same way: the block is there and has never been
    /// written, so Linux hands back zeros rather than whatever the last owner
    /// of the block left behind.
    pub(crate) fn extent_lookup(&self, d: &InodeData, l: u64) -> Option<u64> {
        let mut node = ext_root(d).to_vec();
        loop {
            if !ext_ok(&node) {
                return None;
            }
            let depth = ext_depth(&node);
            if depth > EXTENT_MAX_DEPTH {
                return None;
            }
            let count = ext_entries(&node);
            if depth == 0 {
                for i in 0..count {
                    let (first, raw_len, start) = ext_leaf(&node, i);
                    let (len, unwritten) = ext_len(raw_len);
                    // Entries are sorted by logical block, so one that starts
                    // past `l` says every later one does too.
                    if l < first as u64 {
                        return None;
                    }
                    if l < first as u64 + len {
                        return if unwritten {
                            None
                        } else {
                            Some(start + (l - first as u64))
                        };
                    }
                }
                return None;
            }
            let (_at, child) = self.ext_child(&node, l)?;
            node = self.ext_load(child).ok()?;
        }
    }

    /// Map logical block `l` of an extent-mapped inode, allocating what is
    /// missing - the data block, and any node it takes to hold the extent
    /// that names it.
    pub(crate) fn extent_ensure(
        &self,
        d: &mut InodeData,
        ino: u32,
        l: u64,
    ) -> VfsResult<(u64, bool)> {
        let mut root = ext_root(d);
        if !ext_ok(&root) {
            // The flag says tree and the bytes say otherwise: an inode whose
            // block pointers were written by something that never heard of
            // extents. Reading it as a tree would follow a name as a block
            // number, which is how a filesystem takes itself apart.
            return Err(VfsError::Corrupt);
        }
        let mut nodes = 0u32;
        let (block, allocated, split) =
            self.ext_ensure_node(&mut root, ino, d.generation, l, &mut nodes)?;
        if let Some(split) = split {
            self.ext_grow_root(&mut root, ino, d.generation, split, &mut nodes)?;
        }
        // The tree's own blocks belong to the inode as much as its data does:
        // `i_blocks` counts them, and an inode that hides the two blocks its
        // index lives in reports six sectors where the disk holds eight.
        for _ in 0..nodes {
            d.blocks += self.block_units(d.flags);
        }
        ext_put_root(d, &root);
        Ok((block, allocated))
    }

    /// Ensure `l` is mapped inside `node`, recursing into the child that
    /// covers it. The node is modified in place and written back by the
    /// caller - which for the root is the inode, and for anything below it is
    /// the block it was loaded from plus the sibling a split may have made.
    /// `nodes` counts the tree's own blocks allocated along the way, for
    /// `i_blocks`.
    pub(crate) fn ext_ensure_node(
        &self,
        node: &mut [u8],
        ino: u32,
        genc: u32,
        l: u64,
        nodes: &mut u32,
    ) -> VfsResult<(u64, bool, Option<ExtSplit>)> {
        if !ext_ok(node) {
            return Err(VfsError::Corrupt);
        }
        let depth = ext_depth(node);
        if depth > EXTENT_MAX_DEPTH {
            return Err(VfsError::Corrupt);
        }
        let count = ext_entries(node);
        if depth > 0 {
            let (at, child_block) = self.ext_child(node, l).ok_or(VfsError::Corrupt)?;
            let mut child = self.ext_load(child_block)?;
            let (block, allocated, split) =
                self.ext_ensure_node(&mut child, ino, genc, l, nodes)?;
            // Written back whatever happened: the recursion may have added an
            // extent to it or taken half its entries away.
            self.stamp_extent(&mut child, ino, genc);
            self.write_bytes(child_block, 0, &child);
            let Some(split) = split else {
                return Ok((block, allocated, None));
            };
            // The child split, so there is a sibling to index. It goes beside
            // the child we came down through, which is the entry `at`.
            if count < ext_max(node) {
                ext_insert(node, at + 1);
                ext_write_index(node, at + 1, split.first, split.child);
                return Ok((block, allocated, None));
            }
            *nodes += 1;
            let split = self.ext_split(node, ino, genc, at + 1, ExtInsert::Index(split), block)?;
            return Ok((block, allocated, Some(split)));
        }
        // A leaf: either `l` is inside one of its extents, or a new one goes
        // in at the first entry that starts past it.
        let mut at = count;
        for i in 0..count {
            let (first, raw_len, start) = ext_leaf(node, i);
            let (len, unwritten) = ext_len(raw_len);
            if l < first as u64 {
                at = i;
                break;
            }
            if l < first as u64 + len {
                // An unwritten extent already has the block; "initialising"
                // it is exactly this, writing its length back as the written
                // one it now is - the same number, read the other way.
                if unwritten {
                    ext_write_leaf(node, i, first, len as u16, start);
                }
                return Ok((start + (l - first as u64), false, None));
            }
        }
        let block = self.alloc_block()?;
        if count < ext_max(node) {
            // A block that lands beside an extent it could have been part of
            // joins it instead of starting a new one. Logical and physical
            // adjacency together, and only between written extents - a run
            // the allocator gave out in pieces is one extent, not a list of
            // one-block ones a reference fsck would spend its first pass
            // folding back together.
            if at > 0 {
                let (first, raw_len, start) = ext_leaf(node, at - 1);
                let (len, unwritten) = ext_len(raw_len);
                // The length may grow by one only while the run stays a
                // written one: 32768 is the longest a written extent can be,
                // and one more than that would be read as an *unwritten*
                // extent of one block.
                if !unwritten
                    && len < EXTENT_INIT_MAX as u64
                    && first as u64 + len == l
                    && start + len == block
                {
                    ext_write_leaf(node, at - 1, first, raw_len + 1, start);
                    return Ok((block, true, None));
                }
            }
            if at < count {
                let (first, raw_len, start) = ext_leaf(node, at);
                let (len, unwritten) = ext_len(raw_len);
                if !unwritten
                    && len < EXTENT_INIT_MAX as u64
                    && first as u64 == l + 1
                    && start == block + 1
                {
                    ext_write_leaf(node, at, l as u32, raw_len + 1, block);
                    return Ok((block, true, None));
                }
            }
            ext_insert(node, at);
            ext_write_leaf(node, at, l as u32, 1, block);
            return Ok((block, true, None));
        }
        *nodes += 1;
        let split = self.ext_split(node, ino, genc, at, ExtInsert::Leaf(l), block)?;
        Ok((block, true, Some(split)))
    }

    /// The child of an index node that covers logical block `l`: the last
    /// entry whose first block is at or below it, or the first entry when
    /// every one of them starts above - a tree whose index never says 0 still
    /// answers for block 0.
    pub(crate) fn ext_child(&self, node: &[u8], l: u64) -> Option<(usize, u64)> {
        let count = ext_entries(node);
        let mut chosen = None;
        for i in 0..count {
            let (first, child) = ext_index(node, i);
            if first as u64 <= l {
                chosen = Some((i, child));
            } else {
                break;
            }
        }
        chosen.or_else(|| (count > 0).then(|| (0, ext_index(node, 0).1)))
    }

    pub(crate) fn ext_load(&self, block: u64) -> VfsResult<Vec<u8>> {
        if !self.block_is_live(block) {
            return Err(VfsError::Corrupt);
        }
        let mut node = vec![0u8; self.block_size()];
        self.read_bytes(block, 0, &mut node);
        Ok(node)
    }

    /// Split a full node in two: the upper half of its entries moves to a
    /// fresh block and the new entry goes into whichever half it belongs to.
    /// The sibling is written where it is born; this node is written by the
    /// caller, which is the only place that knows where it lives.
    pub(crate) fn ext_split(
        &self,
        node: &mut [u8],
        ino: u32,
        genc: u32,
        at: usize,
        what: ExtInsert,
        answer: u64,
    ) -> VfsResult<ExtSplit> {
        let sibling = self.alloc_block()?;
        let mut other = vec![0u8; self.block_size()];
        ext_init_header(&mut other, ext_depth(node));
        let count = ext_entries(node);
        let keep = count / 2;
        let moved = count - keep;
        other[EXTENT_HEADER..EXTENT_HEADER + moved * EXTENT_ENTRY].copy_from_slice(
            &node[EXTENT_HEADER + keep * EXTENT_ENTRY..EXTENT_HEADER + count * EXTENT_ENTRY],
        );
        ext_set_entries(&mut other, moved);
        ext_set_entries(node, keep);
        // Which half took the new entry decides where its block ends up.
        let (target, at) = if at >= keep {
            (&mut other[..], at - keep)
        } else {
            (node, at)
        };
        match what {
            // A run of one block. Joining the extent next door would be
            // cheaper on disk and wrong in general: two adjacent logical
            // blocks are only adjacent on disk until somebody frees one.
            ExtInsert::Leaf(l) => {
                ext_insert(target, at);
                ext_write_leaf(target, at, l as u32, 1, answer);
            }
            ExtInsert::Index(split) => {
                ext_insert(target, at);
                ext_write_index(target, at, split.first, split.child);
            }
        }
        self.stamp_extent(&mut other, ino, genc);
        self.write_bytes(sibling, 0, &other);
        // What the parent indexes is the sibling - the half that was moved
        // out, not the half that was kept, and not whichever one happened to
        // take the new entry. Two children under the same first block is a
        // tree that cannot be searched: a lookup picks the first one whose
        // range covers the block, and the other half becomes unreachable.
        let first = if ext_depth(&other) == 0 {
            ext_leaf(&other, 0).0
        } else {
            ext_index(&other, 0).0
        };
        Ok(ExtSplit {
            first,
            child: sibling,
        })
    }

    /// The root ran out of room and split, which is the only way a tree gets
    /// deeper: everything the root held moves into one fresh block and the
    /// root becomes an index over two children.
    pub(crate) fn ext_grow_root(
        &self,
        root: &mut [u8],
        ino: u32,
        genc: u32,
        split: ExtSplit,
        nodes: &mut u32,
    ) -> VfsResult<()> {
        let depth = ext_depth(root);
        if depth + 1 > EXTENT_MAX_DEPTH {
            // Five levels hold a file in the exabytes. Past that it is not a
            // big tree, it is a loop someone wrote into the image.
            return Err(VfsError::FileTooBig);
        }
        *nodes += 1;
        let child = self.alloc_block()?;
        let mut node = vec![0u8; self.block_size()];
        ext_init_header(&mut node, depth);
        let count = ext_entries(root);
        // The entries keep their shape: what was a leaf stays a leaf one
        // level down, what was an index stays an index.
        node[EXTENT_HEADER..EXTENT_HEADER + count * EXTENT_ENTRY]
            .copy_from_slice(&root[EXTENT_HEADER..EXTENT_HEADER + count * EXTENT_ENTRY]);
        ext_set_entries(&mut node, count);
        self.stamp_extent(&mut node, ino, genc);
        self.write_bytes(child, 0, &node);
        let first = if depth == 0 {
            ext_leaf(&node, 0).0
        } else {
            ext_index(&node, 0).0
        };
        ext_init_header(root, depth + 1);
        ext_set_entries(root, 2);
        ext_write_index(root, 0, first, child);
        ext_write_index(root, 1, split.first, split.child);
        Ok(())
    }

    /// Give back every block an extent tree maps, and the tree's own blocks
    /// with them: a node is freed after everything it names, because a block
    /// freed while something still points at it is a block somebody else is
    /// about to be given twice.
    pub(crate) fn free_extents_of(&self, data: &InodeData) {
        let root = ext_root(data);
        if !ext_ok(&root) {
            return;
        }
        self.free_extent_node(&root, ext_depth(&root));
    }

    pub(crate) fn free_extent_node(&self, node: &[u8], depth: u16) {
        if depth > EXTENT_MAX_DEPTH {
            return;
        }
        for i in 0..ext_entries(node) {
            if depth == 0 {
                let (_first, raw_len, start) = ext_leaf(node, i);
                let (len, _unwritten) = ext_len(raw_len);
                for n in 0..len {
                    self.free_block(start + n);
                }
            } else {
                let (_first, child) = ext_index(node, i);
                let Ok(child_node) = self.ext_load(child) else {
                    continue;
                };
                if ext_ok(&child_node) {
                    self.free_extent_node(&child_node, depth - 1);
                }
                self.free_block(child);
            }
        }
    }

    /// Free the tail of an extent-mapped file: every block from logical block
    /// `keep_blocks` onward, splitting the extent that straddles the cut. The
    /// kept blocks are left exactly where they are - a shrink never moves data,
    /// it only gives the tail back - so this is bounded by the tail, not the
    /// file. Returns `Some(freed)` (written blocks given back) when it handled
    /// the mapping, or `None` for a shape it does not cover (not extent-mapped,
    /// or a tree deeper than the root - the caller then falls back).
    pub(crate) fn free_blocks_after(&self, d: &mut InodeData, keep_blocks: u64) -> Option<u64> {
        if !self.is_extent_mapped(d) {
            return None;
        }
        let mut root = ext_root(d);
        if !ext_ok(&root) {
            return Some(0); // no mapping: nothing to free
        }
        if ext_depth(&root) != 0 {
            return None; // deeper tree: caller falls back to the buffering path
        }
        let entries = ext_entries(&root);
        let mut freed = 0u64;
        let mut kept = 0usize;
        for i in 0..entries {
            let (first, raw_len, start) = ext_leaf(&root, i);
            let (len, unwritten) = ext_len(raw_len);
            let first = first as u64;
            if first >= keep_blocks {
                // Entirely past the cut: give the run back, drop the entry.
                if !unwritten {
                    for n in 0..len {
                        self.free_block(start + n);
                    }
                    freed += len;
                }
                continue;
            }
            if first + len > keep_blocks {
                // Straddles the cut: free from the cut to the run's end, keep
                // the head, and rewrite the entry to the shorter run.
                let keep_len = keep_blocks - first;
                if !unwritten {
                    for n in keep_len..len {
                        self.free_block(start + n);
                    }
                    freed += len - keep_len;
                }
                let raw = ext_raw_len(keep_len, unwritten);
                ext_write_leaf(&mut root, kept, first as u32, raw, start);
            } else {
                // Entirely kept: move it down to the compacted slot.
                let raw = ext_raw_len(len, unwritten);
                ext_write_leaf(&mut root, kept, first as u32, raw, start);
            }
            kept += 1;
        }
        for i in kept..entries {
            ext_write_leaf(&mut root, i, 0, 0, 0);
        }
        ext_set_entries(&mut root, kept);
        ext_put_root(d, &root);
        Some(freed)
    }
}
