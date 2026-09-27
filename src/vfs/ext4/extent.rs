use super::*;

// ---- extent trees ----
/// Every extent node opens with this word, which is what says "the sixty
/// bytes of i_block are a tree" rather than fifteen block pointers.
pub(crate) const EXTENT_MAGIC: u16 = 0xf30a;
pub(crate) const EXTENT_HEADER: usize = 12;
/// One extent, or one pointer to a node below: `ee_block`, a length, and a
/// physical start, or the child's block for an index entry.
pub(crate) const EXTENT_ENTRY: usize = 12;
/// How much of an inode the tree's root gets: `i_block` is fifteen words, the
/// header takes three of them, and what is left holds four extents.
pub(crate) const EXTENT_ROOT_BYTES: usize = 60;
/// ext4 allows five levels; deeper than that is a corrupt tree, not a big
/// one, and walking it would recurse into whatever the child block says.
pub(crate) const EXTENT_MAX_DEPTH: u16 = 5;
/// The longest run one entry can describe: `EXT_INIT_MAX_LEN`, half the
/// sixteen-bit length field.
pub(crate) const EXTENT_INIT_MAX: u16 = 32768;

/// The length an extent entry covers, and whether it is *unwritten*: blocks
/// that are allocated but hold nothing yet, which read as zeros.
///
/// ext4 keeps no flag bit beside the length - the length **is** the flag. A
/// value of at most 32768 is a written extent of that many blocks; a larger
/// one is an unwritten extent of `ee_len - 32768` blocks, because an
/// unwritten run can never be the full 32768 long.
///
/// The boundary is the whole reason this is a function rather than a mask.
/// 32768 is a legal *written* length - it is the most one entry can cover - so
/// testing the top bit reads the largest extent a filesystem can write as a
/// zero-length hole, and every block of it comes back as zeros. That is not a
/// rare shape: any file large enough for the allocator to cap a run at exactly
/// 32768 blocks has one, which is how a 180 MB shared library read as 128 MB
/// of zeros and loading it failed with "Exec format error".
pub(crate) fn ext_len(raw: u16) -> (u64, bool) {
    if raw <= EXTENT_INIT_MAX {
        (raw as u64, false)
    } else {
        ((raw - EXTENT_INIT_MAX) as u64, true)
    }
}

/// The length field [`ext_len`] reads, written back: a written run is its
/// length and an unwritten one is its length plus 32768.
///
/// Anything that copies an extent entry - a truncate rewriting the head of a
/// run it kept - has to go through here. Writing the length alone turns an
/// unwritten run into a written one, and a hole the file never wrote becomes
/// whatever the block's last owner left on the disk.
pub(crate) fn ext_raw_len(len: u64, unwritten: bool) -> u16 {
    let len = len as u16;
    if unwritten {
        len + EXTENT_INIT_MAX
    } else {
        len
    }
}

// ---- extent node images, pure over bytes ----

/// A node that could not hold what it was given and was split in two. The
/// parent has to index the new sibling under the first logical block it
/// covers; that is the whole of what an index entry says.
pub(crate) struct ExtSplit {
    pub(crate) first: u32,
    pub(crate) child: u64,
}

/// What a split has to put into whichever half takes it.
pub(crate) enum ExtInsert {
    /// A new single-block extent covering logical block `l`.
    Leaf(u64),
    /// A sibling handed up by a node below, which becomes an index entry.
    Index(ExtSplit),
}

/// The sixty bytes of `i_block` read as a node image. It is the same fifteen
/// words a block map uses, which is the point: the inode's flag, and only the
/// flag, says which one they are.
pub(crate) fn ext_root(d: &InodeData) -> [u8; EXTENT_ROOT_BYTES] {
    let mut node = [0u8; EXTENT_ROOT_BYTES];
    for (i, word) in d.block.iter().enumerate() {
        node[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    node
}

pub(crate) fn ext_put_root(d: &mut InodeData, node: &[u8]) {
    for (i, word) in d.block.iter_mut().enumerate() {
        *word = u32::from_le_bytes(node[i * 4..i * 4 + 4].try_into().expect("four bytes"));
    }
}

pub(crate) fn ext_ok(node: &[u8]) -> bool {
    le16(node, 0) == EXTENT_MAGIC
}

pub(crate) fn ext_entries(node: &[u8]) -> usize {
    le16(node, 2) as usize
}

pub(crate) fn ext_set_entries(node: &mut [u8], count: usize) {
    set16(node, 2, count as u16);
}

pub(crate) fn ext_max(node: &[u8]) -> usize {
    le16(node, 4) as usize
}

pub(crate) fn ext_depth(node: &[u8]) -> u16 {
    le16(node, 6)
}

/// Lay down an empty node: the magic that says this is a tree, no entries,
/// and room for however many fit the space it has - four in the inode's sixty
/// bytes, eighty-odd in a whole block.
pub(crate) fn ext_init_header(node: &mut [u8], depth: u16) {
    set16(node, 0, EXTENT_MAGIC);
    set16(node, 2, 0);
    set16(
        node,
        4,
        ((node.len() - EXTENT_HEADER) / EXTENT_ENTRY) as u16,
    );
    set16(node, 6, depth);
    set32(node, 8, 0);
}

/// Make room for one entry at `at`, shifting the rest up. The node has to
/// have room; a node that does not splits instead.
pub(crate) fn ext_insert(node: &mut [u8], at: usize) {
    let count = ext_entries(node);
    assert!(
        count < ext_max(node),
        "extent node holds {count} of {} entries already",
        ext_max(node)
    );
    let from = EXTENT_HEADER + at * EXTENT_ENTRY;
    let to = EXTENT_HEADER + count * EXTENT_ENTRY;
    node.copy_within(from..to, from + EXTENT_ENTRY);
    ext_set_entries(node, count + 1);
}

/// Leaf entry `i`: the logical block it starts at, its raw length field, and
/// the physical block that run begins on.
///
/// The length comes back raw because it is two things at once - see
/// [`ext_len`], which is the only thing that should read it.
pub(crate) fn ext_leaf(node: &[u8], i: usize) -> (u32, u16, u64) {
    let at = EXTENT_HEADER + i * EXTENT_ENTRY;
    let start = le32(node, at + 8) as u64 | ((le16(node, at + 6) as u64) << 32);
    (le32(node, at), le16(node, at + 4), start)
}

pub(crate) fn ext_write_leaf(node: &mut [u8], i: usize, first: u32, len: u16, start: u64) {
    let at = EXTENT_HEADER + i * EXTENT_ENTRY;
    set32(node, at, first);
    set16(node, at + 4, len);
    set16(node, at + 6, (start >> 32) as u16);
    set32(node, at + 8, start as u32);
}

/// Index entry `i`: the first logical block the child covers, and the child.
pub(crate) fn ext_index(node: &[u8], i: usize) -> (u32, u64) {
    let at = EXTENT_HEADER + i * EXTENT_ENTRY;
    (
        le32(node, at),
        le32(node, at + 4) as u64 | ((le16(node, at + 8) as u64) << 32),
    )
}

pub(crate) fn ext_write_index(node: &mut [u8], i: usize, first: u32, child: u64) {
    let at = EXTENT_HEADER + i * EXTENT_ENTRY;
    set32(node, at, first);
    set32(node, at + 4, child as u32);
    set16(node, at + 8, (child >> 32) as u16);
    set16(node, at + 10, 0);
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A node image of `depth`, in the sixty bytes an inode's `i_block` gives
    /// the tree's root.
    fn root(depth: u16) -> [u8; EXTENT_ROOT_BYTES] {
        let mut node = [0u8; EXTENT_ROOT_BYTES];
        ext_init_header(&mut node, depth);
        node
    }

    /// The length field is the unwritten flag, and the boundary is the whole
    /// reason it is a function and not a mask: 32768 is the longest *written*
    /// run, and a top-bit test reads it as a zero-length hole.
    #[test]
    fn the_length_is_the_flag_at_its_own_boundary() {
        assert_eq!(ext_len(0), (0, false));
        assert_eq!(ext_len(1), (1, false));
        assert_eq!(ext_len(32767), (32767, false));
        assert_eq!(ext_len(EXTENT_INIT_MAX), (EXTENT_INIT_MAX as u64, false));
        assert_eq!(ext_len(EXTENT_INIT_MAX + 1), (1, true));
        assert_eq!(
            ext_len(u16::MAX),
            ((u16::MAX - EXTENT_INIT_MAX) as u64, true)
        );
    }

    /// Every raw value round-trips through read-then-write, which is the
    /// property a truncate rewriting a run's head depends on: a length written
    /// back without its flag turns a hole into whatever the block last held.
    #[test]
    fn a_length_written_back_is_the_one_that_was_read() {
        for raw in [
            0u16,
            1,
            4096,
            32767,
            EXTENT_INIT_MAX,
            EXTENT_INIT_MAX + 1,
            u16::MAX,
        ] {
            let (len, unwritten) = ext_len(raw);
            assert_eq!(ext_raw_len(len, unwritten), raw, "raw {raw}");
        }
    }

    #[test]
    fn an_empty_node_carries_the_magic_and_says_how_many_fit() {
        let node = root(0);
        assert!(ext_ok(&node));
        assert_eq!(ext_entries(&node), 0);
        assert_eq!(ext_depth(&node), 0);
        assert_eq!(ext_max(&node), 4, "sixty bytes hold four extents");

        // A whole block holds the same header and eighty-odd entries.
        let mut block = [0u8; 4096];
        ext_init_header(&mut block, 1);
        assert!(ext_ok(&block));
        assert_eq!(ext_max(&block), (4096 - EXTENT_HEADER) / EXTENT_ENTRY);
        assert_eq!(ext_depth(&block), 1);
    }

    /// A block number is 48 bits, and the top half lives in the length field's
    /// neighbour - a read that drops it names the wrong block on any volume
    /// whose data start above 2^32 sectors.
    #[test]
    fn a_leaf_entry_carries_a_block_above_the_32_bit_line() {
        let mut node = root(0);
        ext_insert(&mut node, 0);
        ext_write_leaf(&mut node, 0, 1234, 7, 0x1_2345_6789);
        assert_eq!(ext_entries(&node), 1);
        assert_eq!(ext_leaf(&node, 0), (1234, 7, 0x1_2345_6789));
    }

    #[test]
    fn an_index_entry_carries_the_child_block_everywhere() {
        let mut node = root(1);
        ext_insert(&mut node, 0);
        ext_write_index(&mut node, 0, 99, 0xffff_0000_0001);
        assert_eq!(ext_index(&node, 0), (99, 0xffff_0000_0001));
    }

    /// Inserting in the middle opens a hole and shifts the rest up - the shift
    /// is what keeps the entries sorted by logical block, which every lookup
    /// below assumes.
    #[test]
    fn inserting_opens_a_hole_and_shifts_the_rest_up() {
        let mut node = root(0);
        for i in 0..3 {
            ext_insert(&mut node, i);
            ext_write_leaf(&mut node, i, i as u32, 1, 100 + i as u64);
        }
        ext_insert(&mut node, 1);
        ext_write_leaf(&mut node, 1, 42, 2, 4242);

        assert_eq!(ext_entries(&node), 4);
        assert_eq!(ext_leaf(&node, 0), (0, 1, 100));
        assert_eq!(ext_leaf(&node, 1), (42, 2, 4242));
        assert_eq!(ext_leaf(&node, 2), (1, 1, 101));
        assert_eq!(ext_leaf(&node, 3), (2, 1, 102));
    }

    /// A node that is full is a node that splits; inserting into one anyway is
    /// a bug the assert turns into a message rather than an overwrite of
    /// whatever follows the image in memory.
    #[test]
    #[should_panic(expected = "holds 4 of 4")]
    fn inserting_into_a_full_node_panics_rather_than_overflowing() {
        let mut node = root(0);
        for i in 0..4 {
            ext_insert(&mut node, i);
        }
        ext_insert(&mut node, 0);
    }

    /// The root image is the inode's own fifteen words, and reading it back
    /// must be the same words - an inode that loses its tree on a write-back
    /// is a file with no blocks.
    #[test]
    fn the_root_image_is_the_inodes_fifteen_words() {
        let mut data = InodeData::parse(&[0u8; 128]);
        for (i, word) in data.block.iter_mut().enumerate() {
            *word = 0x1000_0000 + i as u32;
        }
        let image = ext_root(&data);
        assert!(!ext_ok(&image), "the words are not a header");

        let mut back = InodeData::parse(&[0u8; 128]);
        ext_put_root(&mut back, &image);
        assert_eq!(back.block, data.block);
    }
}
