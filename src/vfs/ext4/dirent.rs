use super::*;

pub(crate) const DIRENT_HEADER: usize = 8;
/// What a checksummed leaf block reserves for its trailer: the twelve-byte
/// fake entry at the block's end, past the last real entry. Every insert and
/// every last-record-length in such a block runs to the block less this, not
/// to the block's end.
pub(crate) const DIR_LEAF_TAIL: usize = 12;

// ---- directory entry encoding, pure over a block image ----

/// Full length of an entry with a `name_len`-byte name: header plus name,
/// padded to a 4-byte boundary.
pub(crate) fn dirent_len(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len).next_multiple_of(4)
}

/// The record length of the entry at `at`, or `None` where the block stops
/// describing entries.
///
/// Every walk of a directory block is reading a structure some unknown hand
/// wrote, and an entry whose length does not fit the block is not a fact to be
/// argued about - it is where the directory ends. That is Linux's answer too
/// (`ext2_readdir` prints "corrupt directory" and stops), and it is the only
/// one that keeps a bad block from being a dead kernel: an `assert!` here costs
/// the whole machine what fsck could have fixed.
pub(crate) fn entry_rec(page: &[u8], at: usize, limit: usize) -> Option<usize> {
    if at + DIRENT_HEADER > limit || limit > page.len() {
        return None;
    }
    let rec = usize::from(le16(page, at + 4));
    (rec >= DIRENT_HEADER && at + rec <= limit).then_some(rec)
}

/// The name of the entry at `at`, shortened to what its own record can hold.
///
/// `name_len` is a byte, so it can claim more name than the entry has room
/// for. Reading it literally is the out-of-bounds slice a corrupt block hands
/// to the caller; clamping to the record is what the format means, since an
/// entry's name lives entirely inside its record.
pub(crate) fn entry_name(page: &[u8], at: usize, rec: usize) -> &[u8] {
    let len = usize::from(page[at + 6]).min(rec - DIRENT_HEADER);
    &page[at + DIRENT_HEADER..at + DIRENT_HEADER + len]
}

/// Walk the entries of one directory block. Zero-inode slots are holes and
/// are skipped; `visit` returning false stops the walk.
pub(crate) fn dir_walk(page: &[u8], mut visit: impl FnMut(u32, u8, &[u8]) -> bool) {
    let mut at = 0;
    while let Some(rec) = entry_rec(page, at, page.len()) {
        let ino = le32(page, at);
        if ino != 0 {
            let name = entry_name(page, at, rec);
            if !visit(ino, page[at + 7], name) {
                return;
            }
        }
        at += rec;
    }
}

pub(crate) fn dir_find(page: &[u8], want: &str) -> Option<u32> {
    let mut hit = None;
    dir_walk(page, |ino, _type, name| {
        if name == want.as_bytes() {
            hit = Some(ino);
            false
        } else {
            true
        }
    });
    hit
}

pub(crate) fn write_dirent(
    page: &mut [u8],
    at: usize,
    ino: u32,
    rec: usize,
    name: &str,
    ftype: u8,
) {
    set32(page, at, ino);
    set16(page, at + 4, rec as u16);
    page[at + 6] = name.len() as u8;
    page[at + 7] = ftype;
    page[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
}

/// Bring a last record that overhangs the trailer's home back to the usable
/// limit. A volume that grew its checksum trailer the way the format says
/// already stops there; one that does not - an entry whose record still
/// claims the trailer's twelve bytes - is repaired by taking the overhang
/// back, which is exactly what the kernel does when it adds the trailer to
/// an old block. Returns whether it changed anything.
pub(crate) fn trim_dir_tail(page: &mut [u8], limit: usize) -> bool {
    let mut at = 0;
    let mut changed = false;
    while at + DIRENT_HEADER <= limit {
        let rec = le16(page, at + 4) as usize;
        if rec < DIRENT_HEADER {
            break;
        }
        if at + rec > limit {
            set16(page, at + 4, (limit - at) as u16);
            changed = true;
        }
        at += rec;
    }
    changed
}

/// Try to fit one more entry into a directory block, returning whether it
/// went in. Two sources of room: a hole wide enough for the whole entry, or
/// slack after a live entry wide enough for the entry plus a header's worth
/// of its own hole. `limit` is the end of the usable block - the block's
/// full length, less the checksum trailer where a volume has one.
pub(crate) fn dir_try_insert(
    page: &mut [u8],
    limit: usize,
    name: &str,
    child: u32,
    ftype: u8,
) -> bool {
    let needed = dirent_len(name.len());
    // A block nothing has been written into yet - zero inode, zero record.
    // Fresh in a way no damaged entry is: every byte empty. It takes the
    // entry whole, as its first and only record.
    //
    // "Every byte empty" is part of the test rather than an assertion about it,
    // because the two words that look like freshness - a zero inode and a zero
    // record length - are what a truncated or damaged block starts with too.
    // Claiming a block is fresh when the rest of it holds entries overwrites
    // them; refusing the shortcut and walking the block is the answer that is
    // right either way.
    if le32(page, 0) == 0 && le16(page, 4) == 0 && page.iter().all(|byte| *byte == 0) {
        write_dirent(page, 0, child, limit, name, ftype);
        return true;
    }
    let mut at = 0;
    while let Some(rec) = entry_rec(page, at, limit) {
        let ino = le32(page, at);
        if ino == 0 {
            if rec >= needed {
                let rest = rec - needed;
                let entry_rec_len = if rest >= DIRENT_HEADER { needed } else { rec };
                write_dirent(page, at, child, entry_rec_len, name, ftype);
                if entry_rec_len < rec {
                    // What remains of the hole stays a hole, reachable from
                    // the new entry's end.
                    set32(page, at + entry_rec_len, 0);
                    set16(page, at + entry_rec_len + 4, (rec - entry_rec_len) as u16);
                }
                return true;
            }
        } else {
            let own = dirent_len(entry_name(page, at, rec).len());
            if rec - own >= needed {
                set16(page, at + 4, own as u16);
                write_dirent(page, at + own, child, rec - own, name, ftype);
                return true;
            }
        }
        at += rec;
    }
    false
}

/// Take the entry called `name` out of one directory block, reporting the
/// inode it named through `found`. Its bytes go to the entry in front of it,
/// which is how ext2 keeps a block free of holes; one at the very front has
/// nothing in front of it, so it is emptied in place instead.
pub(crate) fn dir_take(page: &mut [u8], name: &str, found: &mut Option<u32>) -> bool {
    let mut at = 0;
    let mut prev: Option<usize> = None;
    while let Some(rec) = entry_rec(page, at, page.len()) {
        let ino = le32(page, at);
        if ino != 0 {
            if entry_name(page, at, rec) == name.as_bytes() {
                *found = Some(ino);
                match prev {
                    // The predecessor's record grows over the whole gap.
                    Some(p) => set16(page, p + 4, (at + rec - p) as u16),
                    None => set32(page, at, 0),
                }
                return true;
            }
            prev = Some(at);
        }
        at += rec;
    }
    false
}

/// Initialise a fresh directory block with "." and "..", the latter taking
/// the rest of the usable block - which on a checksummed volume stops short
/// of the trailer that follows.
pub(crate) fn dir_init(page: &mut [u8], limit: usize, dir_ino: u32, parent_ino: u32) -> bool {
    write_dirent(page, 0, dir_ino, dirent_len(1), ".", FT_DIR);
    write_dirent(
        page,
        dirent_len(1),
        parent_ino,
        limit - dirent_len(1),
        "..",
        FT_DIR,
    );
    true
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// A block with `.` and `..` laid down, the way a fresh one starts.
    fn fresh(limit: usize) -> Vec<u8> {
        let mut page = vec![0u8; limit];
        dir_init(&mut page, limit, 2, 2);
        page
    }

    /// The record length is the header plus the name padded to four bytes;
    /// every walker and every insert steps by it.
    #[test]
    fn a_record_is_the_header_plus_a_padded_name() {
        assert_eq!(dirent_len(0), 8);
        assert_eq!(dirent_len(1), 12);
        assert_eq!(dirent_len(4), 12);
        assert_eq!(dirent_len(5), 16);
        assert_eq!(dirent_len(16), 24);
        for name_len in 0..64 {
            let rec = dirent_len(name_len);
            assert!(rec >= DIRENT_HEADER + name_len, "name_len {name_len}");
            assert_eq!(rec % 4, 0, "name_len {name_len}");
        }
    }

    /// A record that does not fit the usable block ends the walk rather than
    /// faulting: this is the one place a corrupt block is allowed to stop and
    /// a `assert!` would cost the whole machine.
    #[test]
    fn a_record_that_does_not_fit_ends_the_walk() {
        let mut page = vec![0u8; 32];
        write_dirent(&mut page, 0, 5, 12, "abc", FT_REG);
        assert_eq!(entry_rec(&page, 0, 32), Some(12));
        // An all-zero rec (a hole the block never wrote) is not a record.
        assert_eq!(entry_rec(&page, 12, 32), None);
        // A rec claiming past the limit stops, and so does a limit past the
        // page.
        write_dirent(&mut page, 12, 6, 40, "z", FT_REG);
        assert_eq!(entry_rec(&page, 12, 32), None);
        assert_eq!(entry_rec(&page, 0, 40), None);
    }

    /// `name_len` is a byte that can claim more name than the record holds;
    /// the name is what fits, which is what keeps a bad block from slicing
    /// out of bounds.
    #[test]
    fn a_name_is_clamped_to_its_own_record() {
        let mut page = vec![0u8; 32];
        write_dirent(&mut page, 0, 5, 12, "", FT_REG);
        page[6] = 20; // claims twenty bytes in a record with room for four
        assert_eq!(entry_name(&page, 0, 12).len(), 4);
    }

    /// The walk skips holes (a zero inode) and reports every live name.
    #[test]
    fn a_walk_skips_holes_and_finds_names() {
        let mut page = fresh(64);
        assert!(dir_try_insert(&mut page, 64, "x", 10, FT_REG));
        assert_eq!(dir_find(&page, "."), Some(2));
        assert_eq!(dir_find(&page, ".."), Some(2));
        assert_eq!(dir_find(&page, "x"), Some(10));
        assert_eq!(dir_find(&page, "gone"), None);

        // Empty the front entry and the walk still reads the rest.
        set32(&mut page, 0, 0);
        assert_eq!(dir_find(&page, "."), None);
        assert_eq!(dir_find(&page, "x"), Some(10));
    }

    /// A block nothing has written takes the entry whole, as its only record.
    #[test]
    fn a_fresh_block_takes_the_entry_whole() {
        let mut page = vec![0u8; 128];
        assert!(dir_try_insert(&mut page, 128, "name", 7, FT_REG));
        assert_eq!(entry_rec(&page, 0, 128), Some(128));
        assert_eq!(dir_find(&page, "name"), Some(7));
    }

    /// Otherwise the room comes from the slack after a live entry - the
    /// predecessor's record shrinks to its own length and the new entry takes
    /// what is left.
    #[test]
    fn an_insert_takes_the_slack_after_a_live_entry() {
        let mut page = fresh(128);
        assert_eq!(entry_rec(&page, 12, 128), Some(116));
        assert!(dir_try_insert(&mut page, 128, "y", 11, FT_REG));
        assert_eq!(
            entry_rec(&page, 12, 128),
            Some(12),
            "`..` shrank to its own"
        );
        assert_eq!(entry_rec(&page, 24, 128), Some(104), "`y` took the rest");
        assert_eq!(dir_find(&page, "y"), Some(11));
    }

    /// A block with no room refuses the entry rather than overwriting what is
    /// there.
    #[test]
    fn a_full_block_refuses_the_entry() {
        let mut page = fresh(24);
        assert!(!dir_try_insert(&mut page, 24, "x", 10, FT_REG));
        assert_eq!(dir_find(&page, "."), Some(2));
        assert_eq!(dir_find(&page, ".."), Some(2));
    }

    /// Taking a name grows its predecessor's record over the gap, which is
    /// how the block stays free of holes.
    #[test]
    fn a_take_is_absorbed_by_the_entry_in_front() {
        let mut page = fresh(128);
        assert!(dir_try_insert(&mut page, 128, "y", 11, FT_REG));
        let mut found = None;
        assert!(dir_take(&mut page, "y", &mut found));
        assert_eq!(found, Some(11));
        assert_eq!(dir_find(&page, "y"), None);
        assert_eq!(
            entry_rec(&page, 12, 128),
            Some(116),
            "`..` grew back over it"
        );
        assert_eq!(dir_find(&page, ".."), Some(2));
    }

    /// A name at the very front has nothing to be absorbed by, so it is
    /// emptied in place.
    #[test]
    fn a_take_at_the_front_empties_the_slot() {
        let mut page = fresh(64);
        let mut found = None;
        assert!(dir_take(&mut page, ".", &mut found));
        assert_eq!(found, Some(2));
        assert_eq!(le32(&page, 0), 0, "the front slot became a hole");
        assert_eq!(dir_find(&page, "."), None);
        assert_eq!(dir_find(&page, ".."), Some(2));
        assert!(!dir_take(&mut page, "absent", &mut found));
    }

    /// A last record overhanging the trailer's home is pulled back to the
    /// usable limit, once.
    #[test]
    fn an_overhanging_tail_is_trimmed_once() {
        let mut page = vec![0u8; 32];
        write_dirent(&mut page, 0, 5, 40, "abc", FT_REG);
        assert!(trim_dir_tail(&mut page, 32));
        assert_eq!(entry_rec(&page, 0, 32), Some(32));
        assert!(!trim_dir_tail(&mut page, 32));
    }

    /// A fresh block opens with `.` and `..` - the two names every directory
    /// must answer - and `..` takes the usable remainder.
    #[test]
    fn a_fresh_block_opens_with_dot_and_dotdot() {
        let page = fresh(64);
        assert_eq!(dir_find(&page, "."), Some(2));
        assert_eq!(dir_find(&page, ".."), Some(2));
        assert_eq!(entry_rec(&page, 0, 64), Some(12));
        assert_eq!(entry_rec(&page, 12, 64), Some(52));
        assert_eq!(page[7], FT_DIR, "`.` is a directory");
    }
}
