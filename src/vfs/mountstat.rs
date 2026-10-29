//! `statmount(2)` and `listmount(2)`: what a mount table can answer about
//! itself, in Linux's own two structs.
//!
//! Both calls read one fact this kernel already keeps - [`MountRecord`] - and
//! the interesting part is not the table but the ABI's shape, which is what was
//! measured on Linux 7.2.11 before any of it was written:
//!
//! * `statmount(2)`'s buffer is a fixed `struct statmount` (512 bytes on that
//!   kernel) followed by a string area; a string field is an **offset into the
//!   string area** (offset 0 is the empty string, which is how a mount whose
//!   root is the filesystem's root reports `mnt_root`). `size` in the returned
//!   struct is the large of what was written and what the caller gave room for;
//!   the caller discovers the needed size by asking with a big buffer.
//! * `mask` is the **fields the kernel wrote**, not the fields requested: a
//!   kernel that cannot produce a field leaves the bit clear, and
//!   `supported_mask` says what this kernel *could* produce at all. A caller
//!   that asks for an unknown bit is answered, not refused.
//! * `bufsize` smaller than the fixed part is not an error: the response is
//!   truncated and `size` says how much was written (`bufsize=16` answered
//!   `size=16, mask=SB_BASIC`). It becomes `EOVERFLOW` (75) only when a
//!   requested *string* field has no room in the area after the fixed part -
//!   `bufsize = sizeof` with `STATMOUNT_MNT_POINT` asked is exactly that.
//! * `mnt_id` is the **unique** id (`statx`'s `STATX_MNT_ID_UNIQUE` /
//!   `listmount`'s output), not the reused 32-bit one `/proc/self/fdinfo`
//!   prints: measured, `statmount` with the fdinfo number is `EINVAL`, and with
//!   the unique one it answers. So this module is fed [`MountRecord::mount_id`],
//!   which is the table's own unique number.
//! * `listmount(2)` walks the mounts of a namespace in tree order (root first,
//!   depth-first), and `param` is the last id returned: `param=ids[2]` resumed
//!   at `ids[3]`. `LISTMOUNT_REVERSE` walks it backwards. The interesting
//!   refusals are the table's (`mnt_id` must be `LSMT_ROOT`), `nr=0` answers 0,
//!   a small `nr` answers `nr` ids, and a bad `flags` is `EINVAL`.
//!
//! What this kernel can produce is a subset of Linux's field list, and the
//! subset is declared rather than faked: the mount options string, the option
//! arrays and the user/group id maps have no per-mount storage here, so those
//! mask bits are never set. `mnt_root` is refused for a bind, because a bind's
//! root is a path relative to the serving filesystem and this table records
//! mounts by attachment, not by root offset.
//!
//! The module is host-compiled and pure: it renders a record into bytes. The
//! syscall layer owns the pointers and the errno names.

use alloc::string::String;
use alloc::vec::Vec;

use super::mount::MountRecord;

/// `struct mnt_id_req`, version 0 - the first 24 bytes, which is what a caller
/// that only names a mount sends.
pub const MNT_ID_REQ_VER0: u32 = 24;
/// `struct mnt_id_req`, version 1 - 32 bytes, which adds `mnt_ns_id` and the
/// `mnt_fd`/`mnt_ns_fd` union member the `STATMOUNT_BY_FD` form reads.
pub const MNT_ID_REQ_VER1: u32 = 32;

/// The fixed part of `struct statmount`: measured `sizeof` was 512 on Linux
/// 7.2.11, and the string area begins here.
pub const STATMOUNT_BYTES: usize = 512;

/// `listmount(2)`'s way of saying "the root of this namespace": listing any
/// other mount is `EINVAL` (measured).
pub const LSMT_ROOT: u64 = u64::MAX;
/// `listmount(2)`'s one flag: list later mounts first.
pub const LISTMOUNT_REVERSE: u64 = 1;
/// `statmount(2)`'s one flag: the request names the mount by a file descriptor
/// on it rather than by id.
pub const STATMOUNT_BY_FD: u64 = 1;

// `statmount(2)`'s mask bits, spelled as Linux spells them.
pub const SB_BASIC: u64 = 0x0000_0001;
pub const MNT_BASIC: u64 = 0x0000_0002;
pub const PROPAGATE_FROM: u64 = 0x0000_0004;
pub const MNT_ROOT: u64 = 0x0000_0008;
pub const MNT_POINT: u64 = 0x0000_0010;
pub const FS_TYPE: u64 = 0x0000_0020;
pub const MNT_NS_ID: u64 = 0x0000_0040;
pub const MNT_OPTS: u64 = 0x0000_0080;
pub const FS_SUBTYPE: u64 = 0x0000_0100;
pub const SB_SOURCE: u64 = 0x0000_0200;
pub const OPT_ARRAY: u64 = 0x0000_0400;
pub const OPT_SEC_ARRAY: u64 = 0x0000_0800;
pub const SUPPORTED_MASK: u64 = 0x0000_1000;
pub const MNT_UIDMAP: u64 = 0x0000_2000;
pub const MNT_GIDMAP: u64 = 0x0000_4000;

/// What this kernel can produce for a record at all. `MNT_ROOT` is added
/// per-record (a bind refuses it) and `SB_BASIC` only for a filesystem whose
/// magic this module knows.
pub const SUPPORTED: u64 = MNT_BASIC | MNT_POINT | FS_TYPE | MNT_NS_ID | SB_SOURCE | SUPPORTED_MASK;

/// `MS_PRIVATE`, which is how Linux reports a mount that propagates nowhere.
/// This table has no propagation machinery, so every mount is private - the
/// measured value on Linux for a `mount(2)` made in a private namespace was
/// `0x40000`.
pub const MNT_PROPAGATION_PRIVATE: u64 = 0x0004_0000;

/// `MOUNT_ATTR_RDONLY`: the one per-mount attribute this kernel actually keeps.
pub const MOUNT_ATTR_RDONLY: u64 = 1;
/// `SB_RDONLY`: the superblock flag a read-only mount reports.
pub const SB_RDONLY: u32 = 1;

/// One validated `struct mnt_id_req`.
///
/// The two calls disagree about what `param` means - a mask for `statmount`,
/// the last id for `listmount` - so this is the raw shape and the call's own
/// layer interprets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MntIdReq {
    /// Bytes the caller said the struct is.
    pub size: u32,
    /// `mnt_fd` (with `STATMOUNT_BY_FD`) or `mnt_ns_fd` (a foreign namespace);
    /// both live at the same offset and the flags word says which is meant.
    pub fd: u32,
    /// The mount to ask about, unique id - or `LSMT_ROOT` for `listmount`.
    pub mnt_id: u64,
    /// Mask (`statmount`) or continuation id (`listmount`).
    pub param: u64,
    /// A foreign namespace's id, the version-1 field.
    pub mnt_ns_id: u64,
}

impl MntIdReq {
    /// Parse a struct the syscall layer has already read out of user memory.
    ///
    /// `None` for a size this kernel does not know: Linux accepted 24 and 32
    /// and refused 20/40 with `EINVAL` (measured), which is the "a caller must
    /// say which version it is speaking" shape every versioned kernel struct
    /// has.
    pub fn parse(bytes: &[u8]) -> Option<MntIdReq> {
        if bytes.len() < 4 {
            return None;
        }
        let size = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if size != MNT_ID_REQ_VER0 && size != MNT_ID_REQ_VER1 {
            return None;
        }
        let size = size as usize;
        if bytes.len() < size {
            return None;
        }
        let word = |at: usize| -> u32 {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let qword = |at: usize| -> u64 {
            let mut v = [0u8; 8];
            v.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(v)
        };
        Some(MntIdReq {
            size: size as u32,
            fd: word(4),
            mnt_id: qword(8),
            param: qword(16),
            mnt_ns_id: if size >= MNT_ID_REQ_VER1 as usize {
                qword(24)
            } else {
                0
            },
        })
    }

    /// Whether the request's mount is named by a descriptor. Linux gates the
    /// `mnt_fd` reading on this flag, not on the version (measured: the fd form
    /// with flags 0 is `EINVAL`, with `STATMOUNT_BY_FD` it answers).
    pub fn by_fd(&self, flags: u64) -> bool {
        flags & STATMOUNT_BY_FD != 0
    }

    /// Whether the request names a *foreign* mount namespace. Version 1 carries
    /// the field; this kernel can only answer for the caller's own namespace
    /// (the caller's mount table is the one it can name), so the syscall layer
    /// refuses it with `EOPNOTSUPP` rather than pretending.
    pub fn foreign_namespace(&self) -> bool {
        self.mnt_ns_id != 0 || (self.size >= MNT_ID_REQ_VER1 && self.fd != 0 && self.mnt_id != 0)
    }
}

/// The superblock magic a filesystem type is known by.
///
/// These are ABI facts (`linux/magic.h`), not this kernel's invention: a
/// `statmount` caller compares `sb_magic` to decide *what* is mounted (a
/// container runtime tells tmpfs from overlay that way), so the number has to
/// be the filesystem's, and a filesystem this table would have to guess at is
/// left out rather than answered wrongly - which is why the value is an
/// `Option`.
pub fn sb_magic(fstype: &str) -> Option<u64> {
    Some(match fstype {
        "tmpfs" => 0x0102_1994,
        "ramfs" => 0x8584_58f6,
        "ext2" | "ext4" => 0x0000_ef53,
        "proc" => 0x0000_9fa0,
        "sysfs" => 0x6265_6572,
        "devtmpfs" => 0x0102_1997,
        "devpts" => 0x0000_1cd1,
        "9p" | "9p2000" => 0x0102_1997,
        "overlay" => 0x794c_7630,
        "cgroup" | "cgroup2" => 0x6367_7270,
        _ => return None,
    })
}

/// The per-mount attributes this kernel keeps, in `MOUNT_ATTR_*` bits.
pub fn mnt_attr(options: &str) -> u64 {
    if read_only(options) {
        MOUNT_ATTR_RDONLY
    } else {
        0
    }
}

/// The superblock flags this kernel keeps, in `SB_*` bits.
pub fn sb_flags(options: &str) -> u32 {
    if read_only(options) { SB_RDONLY } else { 0 }
}

/// Whether a record's option string says the mount is read-only. The string is
/// [`super::mount::mount_options`]' wording - `ro` is its first field when the
/// mount is read-only - so this reads the word rather than inventing a second
/// field the record does not carry.
fn read_only(options: &str) -> bool {
    options
        .strip_prefix("ro")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(','))
}

/// Whether a record is a bind alias (its option string says `bind`). A bind's
/// `mnt_root` is a path relative to the serving filesystem that this table does
/// not record, so the field is refused for one instead of answered `/`.
fn is_bind(options: &str) -> bool {
    options.split(',').any(|word| word == "bind")
}

/// What one `statmount(2)` answered: the mask actually written, and the bytes
/// (fixed part then string area, `size` already set to their length).
pub struct Rendered {
    pub mask: u64,
    pub bytes: Vec<u8>,
}

/// Render one mount record into Linux's `struct statmount`.
///
/// `ns_id` is the mount namespace's own id ([`super::mount::MountTable::ns_id`]),
/// which the record does not carry. `want` is the caller's mask; the result's
/// mask is `want` masked by what this kernel produced.
pub fn statmount(record: &MountRecord, ns_id: u64, want: u64) -> Rendered {
    let mut fixed = [0u8; STATMOUNT_BYTES];
    // Linux's string area starts with the empty string: offset 0 is what
    // `mnt_root` reports for a mount whose root is the filesystem's root
    // (measured: `bufsize=sizeof+8` with `MNT_POINT` answered 6 string bytes,
    // one of them that leading NUL, and the point at offset 1).
    let mut strings = String::new();
    strings.push('\0');
    let mut mask: u64 = 0;

    let mut string = |s: &str| -> u32 {
        let at = strings.len() as u32;
        strings.push_str(s);
        strings.push('\0');
        at
    };

    let can_sb = sb_magic(&record.fstype).is_some();
    let can_root = !is_bind(&record.options);

    if want & SB_BASIC != 0 && can_sb {
        let (major, minor) = (
            crate::uevent::major_of(record.dev),
            crate::uevent::minor_of(record.dev),
        );
        put_u32(&mut fixed, 16, major);
        put_u32(&mut fixed, 20, minor);
        put_u64(&mut fixed, 24, sb_magic(&record.fstype).unwrap_or(0));
        put_u32(&mut fixed, 32, sb_flags(&record.options));
        mask |= SB_BASIC;
    }
    if want & MNT_BASIC != 0 {
        put_u64(&mut fixed, 40, record.mount_id);
        put_u64(&mut fixed, 48, record.parent_id);
        // The reused ids mountinfo prints: this table's unique id truncated is
        // the honest spelling of "the number mountinfo would show", and a
        // caller that compares them against /proc/self/mountinfo gets what it
        // asks for. A caller that needs identity uses `mnt_id`.
        put_u32(&mut fixed, 56, record.mount_id as u32);
        put_u32(&mut fixed, 60, record.parent_id as u32);
        put_u64(&mut fixed, 64, mnt_attr(&record.options));
        put_u64(&mut fixed, 72, MNT_PROPAGATION_PRIVATE);
        mask |= MNT_BASIC;
    }
    if want & MNT_ROOT != 0 && can_root {
        // Offset 0 in the string area is the empty string, which is Linux's
        // spelling for "the mount's root is the filesystem's root".
        put_u32(&mut fixed, 104, 0);
        mask |= MNT_ROOT;
    }
    if want & MNT_POINT != 0 {
        put_u32(&mut fixed, 108, string(&record.path));
        mask |= MNT_POINT;
    }
    if want & FS_TYPE != 0 {
        put_u32(&mut fixed, 36, string(&record.fstype));
        mask |= FS_TYPE;
    }
    if want & MNT_NS_ID != 0 {
        put_u64(&mut fixed, 112, ns_id);
        mask |= MNT_NS_ID;
    }
    if want & SB_SOURCE != 0 {
        put_u32(&mut fixed, 124, string(&record.source));
        mask |= SB_SOURCE;
    }
    if want & SUPPORTED_MASK != 0 {
        let supported =
            SUPPORTED | if can_sb { SB_BASIC } else { 0 } | if can_root { MNT_ROOT } else { 0 };
        put_u64(&mut fixed, 144, supported);
        mask |= SUPPORTED_MASK;
    }

    let mut bytes = Vec::with_capacity(STATMOUNT_BYTES + strings.len());
    bytes.extend_from_slice(&fixed);
    bytes.extend_from_slice(strings.as_bytes());
    let total = bytes.len() as u32;
    put_u32(&mut bytes, 0, total);
    put_u32(&mut bytes, 4, 0);
    put_u64(&mut bytes, 8, mask);
    Rendered { mask, bytes }
}

/// The ids `listmount(LSMT_ROOT)` walks, in Linux's order: the root first, then
/// each mount's children in attachment order, depth-first.
///
/// The records are in attachment order already (`mount_id` rises with each
/// `mount(2)`), so a pre-order walk that sorts a parent's children by id is the
/// tree order Linux reported (measured: consecutive ids parent-first).
pub fn order(records: &[MountRecord]) -> Vec<u64> {
    let root = records
        .iter()
        .find(|record| record.parent_id == record.mount_id);
    let Some(root) = root else { return Vec::new() };
    let mut out = Vec::with_capacity(records.len());
    let mut stack = alloc::vec![root.mount_id];
    while let Some(id) = stack.pop() {
        out.push(id);
        let mut children: Vec<u64> = records
            .iter()
            .filter(|record| record.parent_id == id && record.mount_id != id)
            .map(|record| record.mount_id)
            .collect();
        children.sort_unstable();
        // Pushed in reverse so the lowest id is walked first.
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    out
}

/// The ids a `listmount(2)` call with `param`, `flags` and room for `nr` should
/// answer, in order.
///
/// `param == 0` starts at the beginning; otherwise the walk resumes after the
/// entry whose id it is (`param=ids[2]` answered ids[3], measured). An id that
/// is not in the list is `EINVAL` at the syscall layer, which asks
/// [`resume_index`] first.
pub fn resume(order: &[u64], param: u64, reverse: bool) -> Option<usize> {
    if param == 0 {
        return Some(0);
    }
    if reverse {
        order.iter().position(|id| *id == param).map(|at| at + 1)
    } else {
        order.iter().position(|id| *id == param).map(|at| at + 1)
    }
}

/// The next ids after a resume point, at most `room` of them, in the requested
/// direction.
pub fn slice(order: &[u64], start: usize, room: usize, reverse: bool) -> Vec<u64> {
    if reverse {
        let end = order.len().saturating_sub(start);
        order[..end].iter().rev().copied().take(room).collect()
    } else {
        order[start.min(order.len())..]
            .iter()
            .copied()
            .take(room)
            .collect()
    }
}

fn put_u32(at: &mut [u8], off: usize, value: u32) {
    at[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(at: &mut [u8], off: usize, value: u64) {
    at[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, parent: u64, path: &str, fstype: &str, options: &str) -> MountRecord {
        MountRecord {
            mount_id: id,
            parent_id: parent,
            dev: 0x0000_0048,
            source: String::from("src"),
            path: String::from(path),
            fstype: String::from(fstype),
            options: String::from(options),
        }
    }

    #[test]
    fn the_request_versions_are_the_measured_two() {
        let mut bare = [0u8; MNT_ID_REQ_VER0 as usize];
        put_u32(&mut bare, 0, MNT_ID_REQ_VER0);
        put_u64(&mut bare, 8, 42);
        let req = MntIdReq::parse(&bare).unwrap();
        assert_eq!(req.mnt_id, 42);
        assert_eq!(req.param, 0);
        assert_eq!(req.mnt_ns_id, 0);

        let mut full = [0u8; MNT_ID_REQ_VER1 as usize];
        put_u32(&mut full, 0, MNT_ID_REQ_VER1);
        put_u32(&mut full, 4, 7);
        put_u64(&mut full, 8, 42);
        put_u64(&mut full, 24, 99);
        let req = MntIdReq::parse(&full).unwrap();
        assert_eq!(req.fd, 7);
        assert_eq!(req.mnt_ns_id, 99);
        assert!(req.foreign_namespace());

        // 20 and 40 were EINVAL on Linux; so are they here.
        let mut wrong = [0u8; 40];
        put_u32(&mut wrong, 0, 20);
        assert!(MntIdReq::parse(&wrong).is_none());
        put_u32(&mut wrong, 0, 40);
        assert!(MntIdReq::parse(&wrong).is_none());
    }

    #[test]
    fn a_full_answer_carries_the_string_offsets_linux_uses() {
        let rec = record(3, 1, "/mnt/data", "tmpfs", "rw");
        let out = statmount(
            &rec,
            7,
            SB_BASIC
                | MNT_BASIC
                | MNT_ROOT
                | MNT_POINT
                | FS_TYPE
                | MNT_NS_ID
                | SB_SOURCE
                | SUPPORTED_MASK,
        );
        assert_eq!(out.mask & SB_BASIC, SB_BASIC);
        assert_eq!(
            out.bytes.len(),
            STATMOUNT_BYTES + 1 + "/mnt/data\0tmpfs\0src\0".len()
        );
        let size = u32::from_le_bytes([out.bytes[0], out.bytes[1], out.bytes[2], out.bytes[3]]);
        assert_eq!(size as usize, out.bytes.len());
        // The string area's first byte is the empty string, and the point
        // offset names "/mnt/data" inside it.
        assert_eq!(out.bytes[STATMOUNT_BYTES], 0);
        let point = u32::from_le_bytes([
            out.bytes[108],
            out.bytes[109],
            out.bytes[110],
            out.bytes[111],
        ]);
        let text = &out.bytes[STATMOUNT_BYTES + point as usize..];
        let end = text.iter().position(|b| *b == 0).unwrap();
        assert_eq!(&text[..end], b"/mnt/data");
        assert_eq!(out.bytes[104..108], [0, 0, 0, 0]);
        let magic = u64::from_le_bytes(out.bytes[24..32].try_into().unwrap());
        assert_eq!(magic, 0x0102_1994);
    }

    #[test]
    fn a_bind_refuses_the_root_and_an_unknown_filesystem_refuses_the_superblock() {
        let bind = record(4, 1, "/alias", "tmpfs", "ro,bind");
        let out = statmount(&bind, 1, MNT_ROOT | SB_BASIC | MNT_BASIC | SUPPORTED_MASK);
        assert_eq!(out.mask & MNT_ROOT, 0);
        assert_eq!(out.mask & SB_BASIC, SB_BASIC);
        assert_eq!(out.mask & MNT_BASIC, MNT_BASIC);
        let supported = u64::from_le_bytes(out.bytes[144..152].try_into().unwrap());
        assert_eq!(supported & MNT_ROOT, 0);
        // ...and the read-only attribute is the bind's, not the fs's.
        let attr = u64::from_le_bytes(out.bytes[64..72].try_into().unwrap());
        assert_eq!(attr, MOUNT_ATTR_RDONLY);

        let odd = record(5, 1, "/x", "mysteryfs", "rw");
        let out = statmount(&odd, 1, SB_BASIC | SUPPORTED_MASK);
        assert_eq!(out.mask & SB_BASIC, 0);
        assert_eq!(out.mask & SUPPORTED_MASK, SUPPORTED_MASK);
    }

    #[test]
    fn the_walk_is_the_tree_order_and_resumes_where_it_was_asked() {
        let records = alloc::vec![
            record(1, 1, "/", "ramfs", "rw"),
            record(2, 1, "/proc", "proc", "rw"),
            record(3, 1, "/tmp", "tmpfs", "rw"),
            record(4, 3, "/tmp/a", "tmpfs", "rw"),
            record(5, 1, "/sys", "sysfs", "rw"),
        ];
        let walk = order(&records);
        assert_eq!(walk, alloc::vec![1, 2, 3, 4, 5]);
        assert_eq!(resume(&walk, 0, false), Some(0));
        assert_eq!(resume(&walk, 2, false), Some(2));
        assert_eq!(resume(&walk, 4, false), Some(4));
        assert_eq!(resume(&walk, 99, false), None);
        assert_eq!(slice(&walk, 2, 2, false), alloc::vec![3, 4]);
        assert_eq!(slice(&walk, 0, 3, true), alloc::vec![5, 4, 3]);
        assert_eq!(slice(&walk, 2, 2, true), alloc::vec![3, 2]);
        // A child is walked after its parent, and before the parent's later
        // siblings: that is depth-first, not breadth-first.
        let deep = alloc::vec![
            record(1, 1, "/", "ramfs", "rw"),
            record(2, 1, "/mnt", "tmpfs", "rw"),
            record(3, 2, "/mnt/x", "tmpfs", "rw"),
            record(4, 1, "/proc", "proc", "rw"),
        ];
        assert_eq!(order(&deep), alloc::vec![1, 2, 3, 4]);
    }
}
