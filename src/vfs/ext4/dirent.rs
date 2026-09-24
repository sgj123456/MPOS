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

/// Walk the entries of one directory block. Zero-inode slots are holes and
/// are skipped; `visit` returning false stops the walk.
pub(crate) fn dir_walk(page: &[u8], mut visit: impl FnMut(u32, u8, &[u8]) -> bool) {
    let mut at = 0;
    while at < page.len() {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= page.len(),
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino != 0 {
            let name = &page[at + 8..at + 8 + page[at + 6] as usize];
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
    if le32(page, 0) == 0 && le16(page, 4) == 0 {
        debug_assert!(
            page.iter().all(|b| *b == 0),
            "a fresh leaf block is all zero"
        );
        write_dirent(page, 0, child, limit, name, ftype);
        return true;
    }
    let mut at = 0;
    while at < limit {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= limit,
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino == 0 {
            if rec >= needed {
                let rest = rec - needed;
                let entry_rec = if rest >= DIRENT_HEADER { needed } else { rec };
                write_dirent(page, at, child, entry_rec, name, ftype);
                if entry_rec < rec {
                    // What remains of the hole stays a hole, reachable from
                    // the new entry's end.
                    set32(page, at + entry_rec, 0);
                    set16(page, at + entry_rec + 4, (rec - entry_rec) as u16);
                }
                return true;
            }
        } else {
            let own = dirent_len(page[at + 6] as usize);
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
    while at < page.len() {
        let rec = le16(page, at + 4) as usize;
        assert!(
            rec >= DIRENT_HEADER && at + rec <= page.len(),
            "corrupt ext2 directory: entry at {at} claims {rec} bytes"
        );
        let ino = le32(page, at);
        if ino != 0 {
            if &page[at + 8..at + 8 + page[at + 6] as usize] == name.as_bytes() {
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
