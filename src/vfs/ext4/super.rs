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
    /// `metadata_csum`: every structure's checksum is the CRC-32C chain
    /// below. `GDT_CSUM` (`uninit_bg`): the same idea in the older, weaker
    /// CRC-16 spelling. The write paths branch on these constantly.
    pub(crate) has_metadata_csum: bool,
    pub(crate) has_gdt_csum: bool,
    /// The filesystem's UUID (`s_uuid`, offset 104). Every metadata checksum
    /// seeds from it - the group descriptors' CRC-16 literally folds its
    /// bytes in - so a driver that recomputes checksums has to know it, and
    /// a driver that reads them cannot place the descriptors' tails without
    /// it either.
    pub(crate) uuid: [u8; 16],
    /// The seed the metadata checksums start from: `s_checksum_seed` (offset
    /// 264) when the `metadata_csum_seed` bit says it is written down, and
    /// otherwise the CRC-32C of the UUID itself - the same answer from two
    /// places, which is why an image copied with a new UUID and the old seed
    /// left in place fails every checksum inside it.
    pub(crate) csum_seed: u32,
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
        // The UUID on disk is the textual bytes, in order (e2fsprogs writes
        // it as a string, not the mixed-endian of a `uuid_t`), and it is the
        // seed's source as well as the descriptors' salt.
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&bytes[104..120]);
        // `metadata_csum_seed` (incompat 13) means the seed is written down
        // far out in the superblock's grown region - byte 624, where
        // e2fsprogs put it and where `dumpe2fs` reads it from. Without the
        // bit, the seed is the CRC-32C of the UUID from an all-ones start,
        // which is how old images and new agree.
        let csum_seed = super::csum::csum_seed(
            if incompat & INCOMPAT_CSUM_SEED != 0 {
                Some(le32(bytes, 624))
            } else {
                None
            },
            &uuid,
        );
        // A dirty volume is one a write would finish corrupting, not one we
        // choose to spare: a journal nobody maintains leaves tomorrow's
        // Linux replay to scribble over today's bypassing writes, and a
        // volume that never recorded a clean unmount may be mid-repair.
        // Both answer read-only. (The checksummed forms of the same
        // warning - `uninit_bg` and `metadata_csum` - retired from this
        // list the day the driver started recomputing what it writes.)
        let has_journal = compat & COMPAT_HAS_JOURNAL != 0;
        let state = le16(bytes, 58);
        let must_read_only = incompat & INCOMPAT_RECOVER != 0
            || state & STATE_VALID_FS == 0
            || has_journal
            // The superblock carries a crc32c of its own first 1020 bytes,
            // from a raw all-ones start rather than the volume's seed. A
            // stale one means those bytes already disagree with whoever
            // wrote them; the recompute-on-write that makes fresh writes
            // honest is no reason to trust a volume that arrived lying.
            || (ro_compat & ROCOMPAT_METADATA_CSUM != 0
                && le32(bytes, 1020) != super::csum::sb_csum(&bytes[..1020]));
        let (inode_size, first_ino) = match le32(bytes, 76) {
            0 => (128, 11),
            _ => (le16(bytes, 88) as usize, le32(bytes, 84)),
        };
        if inode_size < 128 {
            return Err("ext2 inode size below 128 bytes is corrupt");
        }
        let first_data_block = le32(bytes, 20);
        let block_size = 1024usize << log_block_size;
        // `s_first_data_block` is 0 or 1 by the format - the block the
        // superblock sits in, which is the first one on a 1024-byte filesystem
        // and the second on the layouts that reserve block 0 for a boot sector.
        // Both halves matter here rather than being tidiness: the recorded
        // position of the superblock is the distance from that block's start to
        // byte 1024, so a claim larger than that distance is not an unusual
        // layout but an underflow, and the pair (1, 4096) is exactly that.
        if first_data_block > 1 || first_data_block as usize * block_size > SUPERBLOCK_AT as usize {
            return Err("ext2 superblock does not sit inside its own first block");
        }
        // The same arithmetic runs the other way when the allocator starts
        // reserving inodes: group 0's first candidate is `s_first_ino` minus
        // one, and a superblock that says nothing is reserved answers zero,
        // which has no minus one.
        if first_ino == 0 {
            return Err("ext2 reserves no inodes, which its own layout cannot mean");
        }
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
            has_metadata_csum: ro_compat & ROCOMPAT_METADATA_CSUM != 0,
            has_gdt_csum: ro_compat & ROCOMPAT_GDT_CSUM != 0,
            uuid,
            csum_seed,
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

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A superblock that parses: revision 0, 1 KiB blocks, no feature bits at
    /// all - the shape an old ext2 has, and the least a driver can be asked
    /// to read. Every test below is this image with one word changed, so what
    /// it refuses is attributable to that word.
    fn minimal() -> Vec<u8> {
        let mut bytes = vec![0u8; SUPERBLOCK_AT as usize];
        set16(&mut bytes, 56, MAGIC);
        set16(&mut bytes, 58, STATE_VALID_FS);
        set32(&mut bytes, 32, 8 * 1024); // blocks_per_group
        set32(&mut bytes, 40, 1024); // inodes_per_group
        bytes
    }

    /// The refusal message of a superblock that does not parse. `Super` has no
    /// `Debug` to hand `unwrap_err`, and the message is the whole answer
    /// anyway.
    fn refusal(bytes: &[u8]) -> &'static str {
        match Super::parse(bytes) {
            Ok(_) => panic!("the superblock parsed, and this test expected a refusal"),
            Err(e) => e,
        }
    }

    /// The buffer has to reach the `s_hash_flags` word the parse always reads
    /// (offset 352), which is why it is a whole superblock and not 60 bytes.
    #[test]
    fn a_minimal_superblock_parses_and_says_what_it_is() {
        let sb = Super::parse(&minimal()).expect("a minimal ext2 superblock");
        assert_eq!(sb.log_block_size, 0);
        assert_eq!(sb.inode_size, 128);
        assert_eq!(sb.first_ino, 11);
        assert_eq!(sb.blocks_per_group, 8192);
        assert_eq!(sb.inodes_per_group, 1024);
        assert_eq!(sb.desc_size, GROUP_DESC_BYTES as u16);
        assert!(!sb.has_extents, "no extents bit, no extent trees");
        assert!(!sb.has_64bit);
        assert!(!sb.has_journal);
        assert!(!sb.must_read_only, "a clean ext2 is writable");
        assert_eq!(sb.sb_block, 0);
        assert_eq!(sb.sb_off, SUPERBLOCK_AT as usize);
    }

    /// The magic is the first word asked about: without it the bytes are not
    /// a superblock at all, and no later field is worth reading.
    #[test]
    fn a_wrong_magic_is_refused_before_anything_else() {
        let mut bytes = minimal();
        set16(&mut bytes, 56, 0x1234);
        assert_eq!(refusal(&bytes), "no ext2 magic at byte 1024");
    }

    /// One word per refusal, so a mount that fails says which word failed.
    #[test]
    fn each_field_the_driver_cannot_read_is_refused_by_its_own_name() {
        let mut bytes = minimal();
        set32(&mut bytes, 24, 3); // 8 KiB blocks
        assert_eq!(refusal(&bytes), "ext2 block size above 4096 is unsupported");

        let mut bytes = minimal();
        set32(&mut bytes, 28, 1); // fragments differ from blocks
        assert_eq!(refusal(&bytes), "ext2 fragments are unsupported");

        // A feature this driver understands well enough to refuse.
        let mut bytes = minimal();
        set32(&mut bytes, 96, 1 << 16); // INCOMPAT_ENCRYPT
        assert_eq!(
            refusal(&bytes),
            "ext4 carries features this driver refuses rather than misreads"
        );

        // A feature nobody here has heard of.
        let mut bytes = minimal();
        set32(&mut bytes, 96, 1 << 20);
        assert_eq!(
            refusal(&bytes),
            "ext4 carries features this driver does not know about"
        );

        let mut bytes = minimal();
        set32(&mut bytes, 100, 1 << 20);
        assert_eq!(
            refusal(&bytes),
            "ext4 carries features this driver does not know about"
        );

        let mut bytes = minimal();
        set32(&mut bytes, 92, 1 << 20);
        assert_eq!(
            refusal(&bytes),
            "ext4 carries compatible features this driver does not know about"
        );
    }

    /// The two group-shape words are not decoration: every walk of the
    /// bitmap and inode table is strided by them.
    #[test]
    fn the_group_shape_is_checked() {
        let mut bytes = minimal();
        set32(&mut bytes, 32, 4096); // not eight blocks per KiB
        assert_eq!(
            refusal(&bytes),
            "ext2 blocks-per-group is not the standard eight per KiB"
        );

        let mut bytes = minimal();
        set32(&mut bytes, 40, 0);
        assert_eq!(
            refusal(&bytes),
            "ext2 superblock claims zero inodes per group"
        );
    }

    /// `64bit` puts the high half of the block count - and of every block
    /// number - in a second word, and a driver that ignores it sees a large
    /// filesystem as a small one.
    #[test]
    fn the_64bit_bit_reads_the_high_half_of_the_block_count() {
        let mut bytes = minimal();
        set32(&mut bytes, 96, INCOMPAT_64BIT);
        set32(&mut bytes, 4, 0x1234);
        set32(&mut bytes, 336, 2);
        let sb = Super::parse(&bytes).expect("64bit is a feature it knows");
        assert!(sb.has_64bit);
        assert_eq!(sb.blocks_count, (2u64 << 32) | 0x1234);
    }

    /// The feature bits the driver keeps are read out of the same word.
    #[test]
    fn the_feature_bits_are_read_out_of_their_own_words() {
        let mut bytes = minimal();
        set32(&mut bytes, 96, INCOMPAT_EXTENTS);
        let sb = Super::parse(&bytes).expect("extents are supported");
        assert!(sb.has_extents);

        let mut bytes = minimal();
        set32(&mut bytes, 100, ROCOMPAT_METADATA_CSUM);
        // The superblock's own checksum is the first 1020 bytes from an
        // all-ones start; write the honest one so only the bit is under test.
        let csum = csum::sb_csum(&bytes[..1020]);
        set32(&mut bytes, 1020, csum);
        let sb = Super::parse(&bytes).expect("metadata_csum is readable");
        assert!(sb.has_metadata_csum);
        assert!(!sb.must_read_only, "an honest checksum keeps it writable");
    }

    /// A volume that was not unmounted cleanly, or that carries a journal
    /// this driver will not maintain, is read-only rather than refused: an
    /// image worth reading is worth reading.
    #[test]
    fn a_dirty_or_journalled_volume_mounts_read_only() {
        let mut bytes = minimal();
        set16(&mut bytes, 58, 0); // no EXT2_VALID_FS
        let sb = Super::parse(&bytes).expect("a dirty ext2 still parses");
        assert!(sb.must_read_only, "not cleanly unmounted");

        let mut bytes = minimal();
        set32(&mut bytes, 92, COMPAT_HAS_JOURNAL);
        let sb = Super::parse(&bytes).expect("a journalled ext2 still parses");
        assert!(sb.has_journal);
        assert!(sb.must_read_only, "no journal is maintained here");
    }

    /// A stale `s_checksum` means the first 1020 bytes already disagree with
    /// whoever wrote them, so the volume mounts read-only rather than being
    /// trusted.
    #[test]
    fn a_stale_superblock_checksum_mounts_read_only() {
        let mut bytes = minimal();
        set32(&mut bytes, 100, ROCOMPAT_METADATA_CSUM);
        let csum = csum::sb_csum(&bytes[..1020]);
        set32(&mut bytes, 1020, csum);
        // Change a byte the checksum covers without updating it.
        set32(&mut bytes, 40, 2048);
        let sb = Super::parse(&bytes).expect("the fields still parse");
        assert!(sb.must_read_only);
    }

    /// The recorded position of the superblock is the distance from
    /// `s_first_data_block` to byte 1024; a claim larger than that distance
    /// is not a layout, it is an underflow.
    #[test]
    fn the_superblocks_own_position_is_the_distance_from_its_first_block() {
        let mut bytes = minimal();
        set32(&mut bytes, 20, 1); // 1 KiB blocks: block 1 is the superblock's
        let sb = Super::parse(&bytes).expect("a 1 KiB filesystem may start at 1");
        assert_eq!(sb.sb_block, 1);
        assert_eq!(sb.sb_off, 0, "the superblock sits at the block's start");

        // The same claim on a 4 KiB filesystem puts the superblock 3 KiB
        // before the block it names.
        let mut bytes = minimal();
        set32(&mut bytes, 24, 2); // 4096-byte blocks
        set32(&mut bytes, 28, 2); // fragments agree
        set32(&mut bytes, 32, 8 * 4096);
        set32(&mut bytes, 20, 1);
        assert_eq!(
            refusal(&bytes),
            "ext2 superblock does not sit inside its own first block"
        );
    }

    /// The descriptor stride is written down from revision 1 on; a value
    /// outside the range every format uses is a table no walk can stride.
    #[test]
    fn an_unusable_descriptor_stride_is_refused() {
        let mut bytes = minimal();
        set32(&mut bytes, 76, 1); // revision 1
        set16(&mut bytes, 88, 256); // inode_size
        set32(&mut bytes, 84, 11); // first_ino
        set16(&mut bytes, 254, 100); // not a descriptor size
        assert_eq!(refusal(&bytes), "ext4 group descriptor size is unusable");

        // A revision-1 image with the honest stride reads it from the field.
        let mut bytes = minimal();
        set32(&mut bytes, 76, 1);
        set16(&mut bytes, 254, 64);
        set16(&mut bytes, 88, 256);
        set32(&mut bytes, 84, 11);
        let sb = Super::parse(&bytes).expect("a 64-byte stride is the format's own");
        assert_eq!(sb.desc_size, 64);
        assert_eq!(sb.inode_size, 256);
    }
}
