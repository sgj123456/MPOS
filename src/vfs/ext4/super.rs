use super::*;

pub(crate) const INCOMPAT_FILETYPE: u32 = 1 << 1;
/// The journal holds transactions that were never applied. A filesystem in
/// this state is not merely one we choose not to write - it is mid-repair,
/// and writing over it is the one thing guaranteed to lose data.
pub(crate) const INCOMPAT_RECOVER: u32 = 1 << 2;
/// Inodes map their data with extent trees instead of block maps.
pub(crate) const INCOMPAT_EXTENTS: u32 = 1 << 6;
/// Block numbers are 64 bits wide: group descriptors grow to 64 bytes to hold
/// the high halves, and the superblock's block count grows a high word.
pub(crate) const INCOMPAT_64BIT: u32 = 1 << 7;
/// A group's bitmaps and inode table may live outside the group itself. That
/// is harmless here because every reader starts from the descriptor, which
/// still names where they went - unlike `META_BG`, which moves the
/// descriptors themselves out of the block following the superblock.
pub(crate) const INCOMPAT_FLEX_BG: u32 = 1 << 9;
/// A filesystem may keep a checksum seed in the superblock. It only matters
/// to whoever recomputes metadata checksums, which this driver does not.
pub(crate) const INCOMPAT_CSUM_SEED: u32 = 1 << 13;
/// Directory indexes may grow deeper than the one intermediate level this
/// driver maintains. Reads are a linear walk and do not care; only insertion
/// into a directory that actually needs the extra level would.
pub(crate) const INCOMPAT_LARGEDIR: u32 = 1 << 14;
/// The incompatible features this driver understands and can honour.
pub(crate) const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR;
/// The ones it understands well enough to refuse: each of them redefines what
/// a block or an inode *means* - compressed, encrypted, inline - so reading
/// past the refusal would hand back the wrong bytes, and refusing costs one
/// error at mount.
pub(crate) const INCOMPAT_REFUSED: u32 = (1 << 0)    // COMPRESSION
    | (1 << 3)                            // JOURNAL_DEV
    | (1 << 4)                            // META_BG
    | (1 << 8)                            // MMP
    | (1 << 10)                           // EA_INODE
    | (1 << 12)                           // DIRDATA
    | (1 << 15)                           // INLINE_DATA
    | (1 << 16)                           // ENCRYPT
    | (1 << 17); // CASEFOLD
/// Everything this driver can at least read unchanged: directory indexes,
/// files past 2 GiB, directories past 65000 links, inodes with extra space.
pub(crate) const ROCOMPAT_SUPPORTED: u32 = (1 << 0)  // SPARSE_SUPER
    | (1 << 1)                            // LARGE_FILE
    | (1 << 2)                            // BTREE_DIR (DIR_INDEX)
    | (1 << 3)                            // HUGE_FILE
    | (1 << 4)                            // GDT_CSUM
    | (1 << 5)                            // DIR_NLINK
    | (1 << 6)                            // EXTRA_ISIZE
    | (1 << 8)                            // QUOTA
    | (1 << 10)                           // METADATA_CSUM
    | (1 << 13); // PROJECT
/// Read-only-compatible features that change the unit of allocation or the
/// meaning of a file's contents, and so cannot be read correctly by a driver
/// that does not implement them.
pub(crate) const ROCOMPAT_REFUSED: u32 = (1 << 7)    // HAS_SNAPSHOT
    | (1 << 9)                            // BIGALLOC
    | (1 << 11)                           // REPLICA
    | (1 << 15)                           // VERITY
    | (1 << 16); // ORPHAN_PRESENT
/// Metadata carries checksums this driver neither verifies nor recomputes.
/// Reading is fine - a checksum is not data - but every write would leave a
/// stale one behind, and a Linux that later mounts the image would trust it.
pub(crate) const ROCOMPAT_METADATA_CSUM: u32 = 1 << 10;
/// Group descriptors carry a checksum, for the same reason and with the same
/// consequence: `bump_free` rewrites a descriptor on every allocation.
pub(crate) const ROCOMPAT_GDT_CSUM: u32 = 1 << 4;
/// The compatible features, which by their own definition a reader may
/// ignore - except the two that decide whether writing is safe at all:
/// a filesystem with a journal must not be written by a driver that does not
/// maintain the journal, and one whose mount state says it was not unmounted
/// cleanly may be mid-repair. Both force the read-only answer below.
pub(crate) const COMPAT_DIR_PREALLOC: u32 = 1 << 0;
pub(crate) const COMPAT_IMAGIC_INODES: u32 = 1 << 1;
pub(crate) const COMPAT_HAS_JOURNAL: u32 = 1 << 2;
pub(crate) const COMPAT_EXTENDED_ATTR: u32 = 1 << 3;
pub(crate) const COMPAT_RESIZE_INODE: u32 = 1 << 4;
pub(crate) const COMPAT_DIR_INDEX: u32 = 1 << 5;
/// The modern orphan tracking: deleted-open inodes chained through inode 11
/// instead of the classic list head. This driver frees at unlink and never
/// writes either form, so seeing the bit costs nothing - but it is a named
/// bit, not a mystery, and the mount table stays honest about it.
pub(crate) const COMPAT_ORPHAN_FILE: u32 = 1 << 12;
/// The compat bits this driver can see and still honour: prealloc and attr
/// and dir-index and resize are writes it simply does not make use of;
/// `has_journal` is read-only until the journal milestone lands. `imagic`
/// redefines what the reserved inodes *are*, so it joins the refused set
/// below rather than this one.
pub(crate) const COMPAT_SUPPORTED: u32 = COMPAT_DIR_PREALLOC
    | COMPAT_HAS_JOURNAL
    | COMPAT_EXTENDED_ATTR
    | COMPAT_RESIZE_INODE
    | COMPAT_DIR_INDEX
    | COMPAT_ORPHAN_FILE;
pub(crate) const COMPAT_REFUSED: u32 = COMPAT_IMAGIC_INODES;
/// `EXT2_VALID_FS`: `s_state` without it means the volume was not unmounted
/// cleanly - the crash marker every fsck honors.
pub(crate) const STATE_VALID_FS: u16 = 0x0001;

/// `EXT2_FLAGS_UNSIGNED_HASH` in the superblock: the tree's hashes were made
/// with unsigned character semantics, which renumbers the stored hash
/// version from the signed variants onto the unsigned ones.
pub(crate) const SB_FLAGS_UNSIGNED_HASH: u32 = 1 << 1;

/// The fields of the on-disk superblock this driver keeps.
pub(crate) struct Super {
    pub(crate) inodes_count: u32,
    /// Blocks in the whole filesystem, high word included. A 64-bit image
    /// writes its size in two places, and a driver that reads only the low
    /// one sees a 4 TiB filesystem as a zero-block one - or, worse, as a
    /// plausible small one whose every block number is past its own end.
    pub(crate) blocks_count: u64,
    pub(crate) first_data_block: u32,
    pub(crate) log_block_size: u32,
    pub(crate) blocks_per_group: u32,
    pub(crate) inodes_per_group: u32,
    pub(crate) first_ino: u32,
    pub(crate) inode_size: u16,
    /// Bytes per group descriptor: 32 for ext2, 64 once `64bit` is on, which
    /// is where the high halves of the bitmap and inode-table pointers live.
    /// Every walk of the descriptor table is strided by this, not by 32.
    pub(crate) desc_size: u16,
    /// Inodes map data with extent trees. It is a property of the filesystem,
    /// not of an inode: a file created here has to be born extent-mapped or a
    /// Linux that later mounts the image finds a block map where the feature
    /// flags promised a tree.
    pub(crate) has_extents: bool,
    /// Block numbers and block counts are 64 bits wide.
    pub(crate) has_64bit: bool,
    /// Whether this filesystem must stay read-only however it was mounted.
    /// Checksums this driver cannot recompute, or a journal with unapplied
    /// transactions, make every write a corruption - so the mount that asked
    /// for read-write gets read-only, and says so through `statfs` and every
    /// refused write rather than by failing outright: an image worth reading
    /// is worth reading.
    pub(crate) must_read_only: bool,
    /// Whether this filesystem carries a journal at all. Read here so the
    /// mount can say out loud what it will not be maintaining, and so the
    /// journal milestone has one place to grow from.
    pub(crate) has_journal: bool,
    /// The htree hash seed: four words at `s_hash_seed` (offset 236, past
    /// the journal fields the rev-1 layout grew), salt shared by every
    /// directory index on this filesystem.
    pub(crate) hash_seed: [u32; 4],
    /// Whether those indexes were built with unsigned character semantics -
    /// the `s_flags` bit (offset 352) Linux records the first time it mounts
    /// a filesystem whose hash flavor was never written down. A filesystem
    /// that says nothing, like this image's, reads as signed.
    pub(crate) hash_unsigned: bool,
    /// The superblock's own home: block and offset within it. It moves with
    /// the block size, so the free counters can be rewritten in place later.
    pub(crate) sb_block: u64,
    pub(crate) sb_off: usize,
}

impl Super {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Super, &'static str> {
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
        // Feature negotiation. Three answers instead of two: a feature this
        // driver cannot read is a refused mount, but one it can read and not
        // maintain - checksums, an unrecovered journal - is a read-only
        // mount. A filesystem worth reading is worth reading, and the answer
        // has to be decided once, here, where the bits are still on screen.
        let incompat = le32(bytes, 96);
        let ro_compat = le32(bytes, 100);
        let compat = le32(bytes, 92);
        if incompat & INCOMPAT_REFUSED != 0
            || ro_compat & ROCOMPAT_REFUSED != 0
            || compat & COMPAT_REFUSED != 0
        {
            return Err("ext4 carries features this driver refuses rather than misreads");
        }
        if incompat & !INCOMPAT_SUPPORTED != 0 || ro_compat & !ROCOMPAT_SUPPORTED != 0 {
            return Err("ext4 carries features this driver does not know about");
        }
        if compat & !COMPAT_SUPPORTED != 0 {
            return Err("ext4 carries compatible features this driver does not know about");
        }
        let has_64bit = incompat & INCOMPAT_64BIT != 0;
        // A dirty volume is one a write would finish corrupting, not one we
        // choose to spare: a journal nobody maintains leaves tomorrow's
        // Linux replay to scribble over today's bypassing writes, and a
        // volume that never recorded a clean unmount may be mid-repair.
        // Until the journal milestone replays and maintains, both answer
        // read-only. (`has_journal` is a *compat* bit; the incompat RECOVER
        // below is the ext3-era dirty spelling, honored for old images.)
        let has_journal = compat & COMPAT_HAS_JOURNAL != 0;
        let state = le16(bytes, 58);
        let must_read_only = incompat & INCOMPAT_RECOVER != 0
            || state & STATE_VALID_FS == 0
            || has_journal
            || ro_compat & (ROCOMPAT_METADATA_CSUM | ROCOMPAT_GDT_CSUM) != 0;
        let (inode_size, first_ino) = match le32(bytes, 76) {
            0 => (128, 11),
            _ => (le16(bytes, 88) as usize, le32(bytes, 84)),
        };
        if inode_size < 128 {
            return Err("ext2 inode size below 128 bytes is corrupt");
        }
        let first_data_block = le32(bytes, 20);
        let block_size = 1024usize << log_block_size;
        // The descriptor stride is written down from rev 1 onwards; before
        // that it is 32 by definition, and a zero means "not written down"
        // rather than "no descriptors".
        let desc_size = if le32(bytes, 76) >= 1 && le16(bytes, 254) != 0 {
            le16(bytes, 254)
        } else {
            GROUP_DESC_BYTES as u16
        };
        if !(GROUP_DESC_BYTES as u16..=GROUP_DESC_BYTES_MAX as u16).contains(&desc_size)
            || desc_size % 4 != 0
            || desc_size as usize > block_size
        {
            return Err("ext4 group descriptor size is unusable");
        }
        let sb = Super {
            inodes_count: le32(bytes, 0),
            blocks_count: le32(bytes, 4) as u64
                | if has_64bit {
                    (le32(bytes, 336) as u64) << 32
                } else {
                    0
                },
            first_data_block,
            log_block_size,
            blocks_per_group: le32(bytes, 32),
            inodes_per_group: le32(bytes, 40),
            first_ino,
            inode_size: inode_size as u16,
            desc_size,
            has_extents: incompat & INCOMPAT_EXTENTS != 0,
            has_64bit,
            must_read_only,
            has_journal,
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
pub(crate) struct Group {
    pub(crate) block_bitmap: u64,
    pub(crate) inode_bitmap: u64,
    pub(crate) inode_table: u64,
    /// Free counts as u32 because `64bit` splits each of them across two
    /// u16s, and a driver that reads only the low one reports a nearly-full
    /// group for one that is nearly empty.
    pub(crate) free_blocks: u32,
    pub(crate) free_inodes: u32,
}
