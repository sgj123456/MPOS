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
/// The top bit of `ee_len` marks an extent that is allocated but not written:
/// a hole that has a block behind it. Reads see zeros, writes clear it.
pub(crate) const EXTENT_UNWRITTEN: u16 = 1 << 15;
/// The most a written extent can cover: the length field's other fifteen
/// bits, minus the one reserved value of zero.
pub(crate) const EXTENT_INIT_MAX: u16 = 32768;

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
        count + 1 <= ext_max(node),
        "extent node holds {count} of {} entries already",
        ext_max(node)
    );
    let from = EXTENT_HEADER + at * EXTENT_ENTRY;
    let to = EXTENT_HEADER + count * EXTENT_ENTRY;
    node.copy_within(from..to, from + EXTENT_ENTRY);
    ext_set_entries(node, count + 1);
}

/// Leaf entry `i`: the logical block it starts at, its length, and the
/// physical block that run begins on. The length's top bit is the unwritten
/// flag rather than part of the number, so callers mask before using it.
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
