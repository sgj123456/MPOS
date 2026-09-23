use super::*;

pub(crate) const DIRECT_BLOCKS: usize = 12;
/// How much of the 128-byte-plus inode we parse and rewrite. i_block ends at
/// byte 100; byte 108 is ext2's `i_dir_acl` and ext4's high half of a file's
/// size, and it is the last byte range this driver owns.
pub(crate) const INODE_PARSED: usize = 112;
/// Offset of `i_size_high`, which ext2 reads as `i_dir_acl`. Only a regular
/// file's size is split across the two; for anything else the word means
/// something else entirely and is preserved, not interpreted.
pub(crate) const INODE_SIZE_HIGH: usize = 108;
pub(crate) const FT_REG: u8 = 1;
pub(crate) const FT_DIR: u8 = 2;
pub(crate) const FT_CHRDEV: u8 = 3;
pub(crate) const FT_SYMLINK: u8 = 7;
pub(crate) const S_IFMT: u16 = 0xf000;
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFDIR: u16 = 0x4000;
pub(crate) const S_IFLNK: u16 = 0xa000;
pub(crate) const S_IFCHR: u16 = 0x2000;
/// The most a "fast" symbolic link can hold: the target lives in the fifteen
/// four-byte block pointers, so the format's own ceiling is 60 bytes. Alpine's
/// userland is built almost entirely out of links shorter than that -
/// `/bin/sh -> /bin/busybox` is twelve - so the fast form is the common one,
/// not an optimisation.
pub(crate) const FAST_SYMLINK_MAX: u32 = 60;

/// `i_blocks` is counted in filesystem blocks rather than 512-byte sectors.
/// It applies per inode, and the filesystem-wide feature is what lets Linux
/// set the flag - a file that needs it says so on itself.
pub(crate) const INODE_HUGE_FILE_FL: u32 = 1 << 18;
/// The inode's block pointers are an extent tree, not a block map.
pub(crate) const INODE_EXTENTS_FL: u32 = 1 << 19;
/// The directory is htree-indexed: block 0 is an index root rather than a
/// plain list of entries, so inserts have to maintain the tree. Reads are
/// immune - the index bytes hide in the slack of `..` and of whole-block
/// fake dirents, which every linear dirent walker skips - and so is removal,
/// which never moves an entry between leaves. Only insertion cares.
pub(crate) const INODE_INDEX_FL: u32 = 1 << 12;

/// The inode fields this driver reads and writes, decoded out of the table.
#[derive(Clone, Copy)]
pub(crate) struct InodeData {
    pub(crate) mode: u16,
    /// The owner, `i_uid` and `i_gid` - the low sixteen bits of each, which
    /// is all the 128-byte inode has room for and all a user id needs here.
    /// Every node used to answer as root's because nothing wrote these.
    pub(crate) uid: u16,
    pub(crate) gid: u16,
    pub(crate) flags: u32,
    /// Size in bytes. ext4 splits it across two words four bytes apart in
    /// meaning and a hundred apart on disk: the low one at 4, the high one at
    /// 108 - which is where ext2 keeps `i_dir_acl`. Only a regular file has a
    /// size up there, so any other inode carries the word it came with and
    /// hands it back unchanged.
    pub(crate) size: u64,
    /// That word, for everything that is not a regular file.
    pub(crate) dir_acl: u32,
    /// In 512-byte sectors, as ext2 counts them - or in filesystem blocks when
    /// the inode is marked `huge_file`.
    pub(crate) links: u16,
    pub(crate) blocks: u32,
    pub(crate) block: [u32; 15],
    /// Unix seconds, the ext2 on-disk words at 8, 12 and 16. `mtime` is the
    /// one callers decide by - apk judges a cached index fresh by it, and an
    /// epoch of zero read as 1970 sent every `apk add` back to the network.
    /// `atime` is carried but never bumped: no read path here updates it, and
    /// carrying it is what keeps a foreign atime from being zeroed by a
    /// metadata write this driver makes for other reasons.
    pub(crate) atime: u32,
    pub(crate) ctime: u32,
    pub(crate) mtime: u32,
}

/// The wall clock, in unix seconds - what inode timestamps are written in.
/// The RTC anchor rides the tick counter, the same sum `gettimeofday` reports;
/// on the host there is no clock, and the tests read the zero that stands for
/// it.
#[cfg(target_os = "none")]
pub(crate) fn now() -> u32 {
    (crate::arch::rtc::boot_epoch()
        + crate::arch::timer::ticks() / crate::arch::timer::TICK_HZ as u64) as u32
}

#[cfg(not(target_os = "none"))]
pub(crate) fn now() -> u32 {
    // The crate is `#![no_std]` even on the host, so there is no clock to
    // read here. A count that moves is what the tests need - a created file
    // has a mtime, and writing it moves it - and deterministic beats real.
    use core::sync::atomic::{AtomicU32, Ordering};
    static HOST_NOW: AtomicU32 = AtomicU32::new(1_700_000_000);
    HOST_NOW.fetch_add(1, Ordering::Relaxed)
}

impl InodeData {
    pub(crate) fn parse(bytes: &[u8]) -> InodeData {
        let mode = le16(bytes, 0);
        let uid = le16(bytes, 2);
        let gid = le16(bytes, 24);
        let high = le32(bytes, INODE_SIZE_HIGH);
        let regular = mode & S_IFMT == S_IFREG;
        InodeData {
            mode,
            uid,
            gid,
            flags: le32(bytes, 32),
            size: if regular {
                le32(bytes, 4) as u64 | ((high as u64) << 32)
            } else {
                le32(bytes, 4) as u64
            },
            dir_acl: high,
            links: le16(bytes, 26),
            blocks: le32(bytes, 28),
            block: core::array::from_fn(|i| le32(bytes, 40 + 4 * i)),
            atime: le32(bytes, 8),
            ctime: le32(bytes, 12),
            mtime: le32(bytes, 16),
        }
    }

    /// Write back exactly the fields this driver owns, leaving the rest of the
    /// on-disk inode (times, reserved space) untouched.
    pub(crate) fn store(&self, bytes: &mut [u8]) {
        set16(bytes, 0, self.mode);
        set16(bytes, 2, self.uid);
        set16(bytes, 24, self.gid);
        set32(bytes, 4, self.size as u32);
        // The high half of a size, or the word that was there before: writing
        // a directory's ACL field as if it were the top of a size would give
        // every directory a size in exabytes the next time it is read.
        set32(
            bytes,
            INODE_SIZE_HIGH,
            if self.mode & S_IFMT == S_IFREG {
                (self.size >> 32) as u32
            } else {
                self.dir_acl
            },
        );
        set32(bytes, 28, self.blocks);
        set16(bytes, 26, self.links);
        set32(bytes, 32, self.flags);
        // The times ride along parsed-and-stored, so a metadata write this
        // driver makes for its own reasons does not zero what the filesystem
        // had. Which of them move is decided by the operation, not here.
        set32(bytes, 8, self.atime);
        set32(bytes, 12, self.ctime);
        set32(bytes, 16, self.mtime);
        for (i, value) in self.block.iter().enumerate() {
            set32(bytes, 40 + 4 * i, *value);
        }
    }

    pub(crate) fn kind(&self) -> NodeKind {
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
    pub(crate) fn is_fast_symlink(&self) -> bool {
        self.size <= FAST_SYMLINK_MAX as u64 && self.blocks == 0
    }
}
