use super::*;

impl Core {
    // ---- directories ----

    /// The physical blocks of a directory, as far as its size reaches.
    ///
    /// A directory's `i_size` and its block map are two separate facts stored
    /// in two separate places, and on a volume that has been damaged - or
    /// invented - they need not agree. A logical block the mapping does not
    /// hold is skipped rather than trusted: it names no entries, which is what
    /// reading an unmapped block would have answered anyway. The walk this
    /// replaces took the agreement of the two as a promise and `expect`ed on
    /// it, which makes a mismatch a dead kernel.
    fn dir_blocks(&self, dir: &InodeData) -> Vec<u64> {
        let block_size = self.block_size() as u64;
        (0..dir.size / block_size)
            .filter_map(|l| self.bmap_lookup(dir, l))
            .collect()
    }

    pub(crate) fn dir_lookup(&self, dir: &InodeData, name: &str) -> VfsResult<Option<u32>> {
        let block_size = self.block_size();
        for block in self.dir_blocks(dir) {
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
    pub(crate) fn dir_entries(&self, dir: &InodeData) -> VfsResult<Vec<(String, NodeKind)>> {
        let block_size = self.block_size();
        let mut found = Vec::new();
        for block in self.dir_blocks(dir) {
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
    pub(crate) fn dir_add(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        assert!(
            !name.is_empty() && name.len() <= 255,
            "ext2 names are 1..=255 bytes"
        );
        if dir.flags & INODE_INDEX_FL != 0 {
            self.dir_add_indexed(dir, dir_ino, name, child, ftype)
        } else {
            self.dir_add_linear(dir, dir_ino, name, child, ftype)
        }
    }

    /// The linear insert: carve slack out of existing blocks first and grow
    /// the directory by one block only when nothing fits.
    pub(crate) fn dir_add_linear(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
        name: &str,
        child: u32,
        ftype: u8,
    ) -> VfsResult<()> {
        let block_size = self.block_size();
        let limit = self.leaf_limit(block_size);
        for block in self.dir_blocks(dir) {
            if self.update_dir_block(block, dir_ino, dir.generation, |page| {
                dir_try_insert(page, limit, name, child, ftype)
            }) {
                return Ok(());
            }
        }
        // Nothing fit, so the directory grows by one block past the ones it has.
        let blocks = dir.size / block_size as u64;
        let (block, _allocated) = self.bmap_ensure(dir, dir_ino, blocks)?;
        self.update_dir_block(block, dir_ino, dir.generation, |page| {
            write_dirent(page, 0, child, limit, name, ftype);
            true
        });
        dir.size += block_size as u64;
        dir.blocks += self.block_units(dir.flags);
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
    pub(crate) fn dir_remove(
        &self,
        dir: &mut InodeData,
        dir_ino: u32,
        name: &str,
    ) -> VfsResult<u32> {
        for block in self.dir_blocks(dir) {
            let mut found = None;
            self.update_dir_block(block, dir_ino, dir.generation, |page| {
                dir_take(page, name, &mut found)
            });
            if let Some(ino) = found {
                return Ok(ino);
            }
        }
        Err(VfsError::NoEntry)
    }

    /// A brand-new inode: reserved fields zeroed, mode and links set, and for
    /// a directory a first block holding "." and "..".
    pub(crate) fn new_inode(&self, kind: NodeKind, parent: u32) -> VfsResult<u32> {
        let ino = self.alloc_inode()?;
        let block_size = self.block_size();
        // A regular file or a directory on a filesystem that promised extents
        // is born extent-mapped: Linux sets the flag as it creates one, and an
        // inode without it is a block map in a filesystem whose superblock says
        // there are none. A symlink is never extent-mapped anywhere - its
        // sixty bytes hold the target, not a tree.
        let extent = self.sb.has_extents && matches!(kind, NodeKind::File | NodeKind::Dir);
        // The node belongs to whoever is creating it, taken here because
        // this is the one place every create passes through - file, directory,
        // symlink, device node and `O_TMPFILE` alike - and because the
        // creator's identity is a fact about *now*: a name does not remember
        // who first wrote it, and nothing later can reconstruct it.
        let (uid, gid) = crate::sched::current_fs_owner();
        let mut data = InodeData {
            uid: uid as u16,
            gid: gid as u16,
            mode: match kind {
                NodeKind::Dir => S_IFDIR | 0o755,
                // A link's permissions are not consulted by anything - Linux
                // ignores them - but every real ext2 link carries 0777, and a
                // filesystem tool that reads this back should see what it
                // expects.
                NodeKind::Symlink => S_IFLNK | 0o777,
                _ => S_IFREG | 0o644,
            },
            flags: if extent { INODE_EXTENTS_FL } else { 0 },
            generation: 0,
            size: 0,
            dir_acl: 0,
            links: 1,
            blocks: 0,
            block: [0; 15],
            // Born now, in all three senses: nothing has read it, nothing has
            // changed it, and nothing has written it yet.
            atime: now(),
            ctime: now(),
            mtime: now(),
        };
        if extent {
            let mut root = [0u8; EXTENT_ROOT_BYTES];
            ext_init_header(&mut root, 0);
            ext_put_root(&mut data, &root);
        }
        if kind == NodeKind::Dir {
            let (block, allocated) = self.bmap_ensure(&mut data, ino, 0)?;
            data.size = block_size as u64;
            if allocated {
                data.blocks += self.block_units(data.flags);
            }
            data.links = 2;
            self.update_dir_block(block, ino, 0, |page| {
                dir_init(page, self.leaf_limit(page.len()), ino, parent)
            });
        }
        self.write_inode(ino, &data);
        Ok(ino)
    }
}
