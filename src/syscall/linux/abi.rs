//! The Linux ABI's pure half: the argument shapes, flag values, struct
//! layouts and encodings the syscall handlers spell, gathered where the
//! host build can compile them.
//!
//! The handlers in [`fs`] and [`proc`] need a scheduler, a page table and a
//! console under them, so they exist only on the machine - which used to
//! leave the *contract* they implement untested: the syscall numbers, the
//! `st_mode` format bits, the `WEXITSTATUS` field position, the `EINVAL`
//! ordering of a `clone` flag word. Everything here is that contract, as
//! the man pages spell it, and nothing here touches machine state - the
//! same rule `lib.rs` draws for the host-tested half of the kernel. The
//! handlers call these items rather than restating them, so a test that
//! passes here is about the code that runs, not about a copy of it.
//!
//! One writing rule for everything here that touches a caller's buffer: the
//! pointer arrives from ring 3 and no bit of the ABI promises its alignment,
//! so every word-sized access is the unaligned form. `write_volatile`'s
//! precondition is alignment, not permission - and in a debug build the
//! precondition is *checked*, which is how a stack buffer in a unit test
//! caught the first draft spelling these as plain volatile writes. The
//! machine's x86_64 does unaligned word accesses natively; the unaligned
//! form costs nothing there and is honest everywhere.

use alloc::string::String;

use crate::syscall::{buffer_ok, err};
use crate::vfs::{Metadata, NodeKind};

// ---------------------------------------------------------------------------
// dirfd-led path shapes: openat(2) and its *at relatives
// ---------------------------------------------------------------------------

/// `openat`'s magic dirfd: the path is relative to the caller's cwd.
pub(super) const AT_FDCWD: i32 = -100;
/// `AT_SYMLINK_NOFOLLOW`: name the link itself, not what it points at.
pub(super) const AT_SYMLINK_NOFOLLOW: usize = 0x100;
/// `AT_EACCESS`: ask the `access` question about the **effective** ids rather
/// than the real ones - "may I" rather than "could the user who invoked me".
/// It is the whole reason a setuid program can ask about somebody else
/// without dropping its own privileges first.
pub(super) const AT_EACCESS: usize = 0x200;
/// `AT_SYMLINK_FOLLOW`: `linkat`'s opposite number - the source is the file
/// the name leads to, not the name. It is the flag that gives a file with no
/// name of its own one: `apk` writes its repository index through
/// `O_TMPFILE` and links the result in by way of `/proc/self/fd/N`, and
/// without the flag that path is a link to a name on procfs rather than to
/// the file, which is EXDEV and a re-fetch.
pub(super) const AT_SYMLINK_FOLLOW: usize = 0x400;
/// `fstatat` by descriptor: an empty path names the dirfd itself.
pub(super) const AT_EMPTY_PATH: usize = 0x1000;
/// `unlinkat`'s spelling of `rmdir`: the flag says which of the two
/// operations the caller means, it is not an optimisation.
pub(super) const AT_REMOVEDIR: usize = 0x200;
/// `statx`'s `AT_NO_AUTOMOUNT`: don't trigger an automount on the final
/// component. This kernel has no automounting, so it is a legal no-op.
pub(crate) const AT_NO_AUTOMOUNT: usize = 0x800;
/// The `statx` synchronisation-type bits (`AT_STATX_FORCE_SYNC` 0x2000 and
/// `AT_STATX_DONT_SYNC` 0x4000; `AS_STAT` is the zero, the default). A single
/// non-networked filesystem cannot honour or violate a cache sync, so these
/// bits are accepted as no-ops - but they are still *legal* and must not be
/// rejected.
pub(crate) const AT_STATX_SYNC_TYPE: usize = 0x6000;

/// Whether a `statx` flag word is inside Linux's vocabulary. `AT_SYMLINK_NOFOLLOW`
/// and `AT_EMPTY_PATH` are honoured by the handler; `AT_NO_AUTOMOUNT` and the
/// sync-type bits are accepted as no-ops (one non-networked filesystem has
/// nothing to automount or sync). A bit outside the set is `EINVAL`. Measured on
/// a current host: the accepted set is exactly `{0x100, 0x800, 0x1000, 0x2000,
/// 0x4000}`; `0x10000` and `0x100000` answer `EINVAL`.
pub(crate) fn statx_flags_are_known(flags: usize) -> bool {
    flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE) == 0
}

// ---------------------------------------------------------------------------
// open(2) flag bits
// ---------------------------------------------------------------------------

/// O_CREAT: make the named file when it is not there already. With O_EXCL
/// it refuses instead of replacing; on its own it opens whatever is there.
pub(super) const O_CREAT: usize = 0o100;
/// `O_EXCL`: with `O_CREAT`, refuse a name that is already there rather than
/// opening it.
///
/// The value is `0o200`, and it is worth spelling out because it sat at
/// `0o400` here for a while - which is `O_NOCTTY`'s number. Nothing noticed
/// until `busybox syslogd` opened `/var/log/messages` with
/// `O_WRONLY|O_CREAT|O_NOCTTY|O_APPEND|O_NONBLOCK`: the kernel read the
/// `O_NOCTTY` bit as `O_EXCL`, the file existed, and the answer was `EEXIST`
/// - so the log file was never written and every message went to the console
/// instead.
pub(super) const O_EXCL: usize = 0o200;
/// `O_TRUNC`: an existing file is emptied as part of the open. Without it a
/// writer that reopens a name it wrote before keeps whatever tail was longer
/// than what it writes now - the new bytes followed by the old ones, with
/// nothing reporting it.
pub(super) const O_TRUNC: usize = 0o1000;
pub(super) const O_CLOEXEC: usize = 0o200_0000;
/// The access request is the low two bits, and it is a request the open is
/// judged against, not a hint it files away: a directory named by an open
/// asking to write is EISDIR before anything else happens. The values are
/// O_RDONLY (0), O_WRONLY (1) and O_RDWR (2) - which is why O_RDONLY is
/// invisible to a test for the other two, and why `F_GETFL` can answer it
/// only as the absence of them both.
pub(super) const O_ACCMODE: usize = 0o3;
/// The three values that field takes, named so a `match` can tell them
/// apart. `O_RDONLY` is zero, so it can only ever be the absence of the
/// other two - which is why the handlers below spell it as the fallback arm
/// rather than testing for it.
pub(super) const O_WRONLY: usize = 1;
pub(super) const O_RDWR: usize = 2;
/// O_DIRECTORY is a demand about the name, not a hint: the name must
/// already be a directory, and a file under it is ENOTDIR rather than a
/// descriptor the caller will read what it believes are entries out of.
pub(super) const O_DIRECTORY: usize = 0o20_0000;
/// The `__O_TMPFILE` half of the pair userspace spells O_TMPFILE. With
/// O_DIRECTORY over it, the pair asks for an inode no name holds - written
/// whole, then `linkat`ed into the tree when it is finished. The bit is
/// spelled out on its own because the open handler must tell the pair from
/// O_DIRECTORY alone, which is a legal flag meaning something else, and
/// because Linux reads the pairing - the bit without the flag - as EINVAL
/// before it reads anything else.
pub(super) const O_TMPFILE: usize = 0o2000_0000;
/// `O_NOFOLLOW`: if the final path component is a symbolic link, the open
/// fails with `ELOOP` rather than following it. Intermediate components are
/// still followed, so this is a guarantee about the *last* name only - the one
/// a caller uses to refuse opening something a symlink points at (the classic
/// `/tmp` world-writable-dir defence). Measured on the host:
/// `open(symlink, O_NOFOLLOW)` is `ELOOP`, `open(regular, O_NOFOLLOW)`
/// succeeds. It is an open-only bit, not a status flag: it never shows back in
/// `F_GETFL`'s value, so it is consumed at the open and not stored.
pub(super) const O_NOFOLLOW: usize = 0o40_0000;

// ---------------------------------------------------------------------------
// fcntl(2) commands and descriptor flags
// ---------------------------------------------------------------------------

pub(super) const F_DUPFD: usize = 0;
pub(super) const F_GETFD: usize = 1;
pub(super) const F_SETFD: usize = 2;
pub(super) const F_GETFL: usize = 3;
pub(super) const F_SETFL: usize = 4;
pub(super) const F_GETLK: usize = 5;
pub(super) const F_SETLK: usize = 6;
pub(super) const F_SETLKW: usize = 7;
/// `F_DUPFD` that flags the *new* descriptor `FD_CLOEXEC` in one call - what a
/// modern libc asks for instead of the `F_DUPFD` + `F_SETFD` pair (`musl`
/// reaches for it on every pipe-protected spawn). It does not sit with the
/// commands above: the kernel numbers the Linux-specific ones from
/// `F_LINUX_SPECIFIC_BASE` (1024), and this is the seventh of those.
///
/// ⚠️ Its value is load-bearing in a way the others' are not. `fs::fcntl`
/// matches on this *name*, and a name that is not in scope in a `match`
/// pattern is not a missing constant - it is a fresh **binding**, which
/// matches every value. That is a catch-all in disguise: every `fcntl` command
/// silently answered by the `dup` arm, and the only complaint is an
/// `unreachable_patterns` warning on the arms *below* it, which a build that
/// greps for `error` never shows. Hence the test that pins the value.
pub(super) const F_DUPFD_CLOEXEC: usize = 1030;
/// The one descriptor flag: the descriptor must not survive the next execve.
pub(super) const FD_CLOEXEC: u32 = 1;

// ---------------------------------------------------------------------------
// poll(2) / select(2)
// ---------------------------------------------------------------------------

/// One `struct pollfd` on x86_64: `fd`, `events`, then `revents`.
pub(super) const POLLFD_SIZE: usize = 8;
/// Where `revents` sits inside a `struct pollfd`.
pub(super) const POLLFD_REVENTS: usize = 6;
/// Where `events` sits inside a `struct pollfd`.
pub(super) const POLLFD_EVENTS: usize = 4;
/// What a caller may ask `poll` to watch for, and what it is told happened.
pub(super) const POLLIN: i16 = 0x001;
pub(super) const POLLOUT: i16 = 0x004;
/// `POLLERR` and `POLLHUP`: the node's peer is gone. They are not askable -
/// `poll` sets them on a descriptor whatever `events` said, which is why they
/// are reported beside the mask rather than filtered through it.
pub(super) const POLLERR: i16 = 0x008;
pub(super) const POLLHUP: i16 = 0x010;
/// `POLLNVAL`: the descriptor is not open. Counting it is what stops a
/// caller waiting on a closed fd from sleeping forever.
pub(super) const POLLNVAL: i16 = 0x020;

/// The ABI's ceiling on a descriptor number in an `fd_set`. Linux answers
/// `EINVAL` for anything past it, and so does this.
pub(super) const FD_SETSIZE: usize = 1024;

// ---------------------------------------------------------------------------
// mmap(2) / mprotect(2) / mremap(2) flag bits
// ---------------------------------------------------------------------------

pub(super) const MAP_SHARED: usize = 0x01;
pub(super) const MAP_FIXED: usize = 0x10;
pub(super) const MAP_ANONYMOUS: usize = 0x20;
/// `MAP_FIXED_NOREPLACE` (since Linux 4.17): the address is mandatory like
/// `MAP_FIXED`, but an occupied range is refused with `EEXIST` instead of being
/// replaced. The caller is asking "this address or fail", which the plain
/// no-`MAP_FIXED` hint cannot say.
pub(super) const MAP_FIXED_NOREPLACE: usize = 0x10_0000;
/// `MAP_HUGETLB`: back the mapping with huge pages. This kernel has no huge-page
/// pool, so the honest answer is the one Linux gives when the reserve is not
/// available - `ENOMEM` - never a silent 4 KiB mapping that pretends otherwise.
pub(super) const MAP_HUGETLB: usize = 0x4_0000;
/// `mprotect`'s protection bits. Readable (0x1) is what a present page is,
/// which is why only the other two are named here.
pub(super) const PROT_WRITE: usize = 0x2;
pub(super) const PROT_EXEC: usize = 0x4;
/// `mremap`'s flag bits: the move may go somewhere else, and the caller named
/// where.
pub(super) const MREMAP_MAYMOVE: usize = 0x1;
pub(super) const MREMAP_FIXED: usize = 0x2;

/// `madvise(2)`'s advice numbers. Only the one this kernel acts on is named;
/// the rest are the recognised no-ops the handler accepts without effect, and
/// spelling a number here for one of those would suggest it did something.
///
/// `MADV_DONTNEED` is the one whose contract is about the pages rather than
/// about a hint: after it, later accesses "will see a zero-filled page" for a
/// private anonymous range, and the pages of a shared one are taken from the
/// object behind it. Both are now answerable, because a mapping is an address
/// space and the next touch refills it.
pub(super) const MADV_DONTNEED: usize = 4;

/// Whether `advice` is one of the values `madvise(2)`'s vocabulary knows, and
/// so one the handler may answer at all. Linux's `madvise` rejects an advice
/// it does not recognise with `EINVAL`; a kernel that answers `0` for a number
/// no one gave it is the §4.2 success-for-a-thing-not-done, and the one shape a
/// probing caller cannot tell from a real acceptance. The set below is the
/// measured one — probed on a current host (`madvise` on a mapped anonymous
/// page, advice `0..=110`), which returns `0` for exactly these and `EINVAL`
/// for the gaps `5`, `6`, `7` (the deleted `MADV_*` numbers), `9`
/// (`MADV_REMOVE`, which faults on an anonymous range) and everything above
/// the recognised block except `102`/`103`. Of these only `MADV_DONTNEED`
/// (`4`) carries a promise this kernel keeps with work; the rest are best-effort
/// hints whose acceptance is truthful, and the handler gives `4` its page-drop.
pub(crate) fn madvise_advice_is_known(advice: usize) -> bool {
    matches!(advice, 0..=4 | 8 | 10..=24 | 102 | 103)
}

// ---------------------------------------------------------------------------
// eventfd(2) flag bits
// ---------------------------------------------------------------------------

pub(super) const EFD_SEMAPHORE: usize = 1;
pub(super) const EFD_NONBLOCK: usize = 0x800;

// ---------------------------------------------------------------------------
// Path normalization - the shape chdir(2) remembers and getcwd(2) owes
// ---------------------------------------------------------------------------

/// The normalized absolute path `path` names, relative components resolved
/// against `base`. `.` drops out, `..` pops one component, repeated slashes
/// collapse — the shape `getcwd` owes and `chdir` remembers.
pub(super) fn normalize_path(path: &str, base: &str) -> String {
    let joined = if path.starts_with('/') {
        String::from(path)
    } else if base.ends_with('/') {
        alloc::format!("{base}{path}")
    } else {
        alloc::format!("{base}/{path}")
    };
    let mut parts: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for component in joined.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return String::from("/");
    }
    let mut out = String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    out
}

// ---------------------------------------------------------------------------
// st_mode: the format bits are a type tag, the low bits are permissions
// ---------------------------------------------------------------------------

/// The mode bits `struct stat` and `struct statx` carry for a node.
pub(crate) fn mode_of(meta: &Metadata, stored: u32) -> u32 {
    // The format bits follow the kind - `st_mode`'s top nibble is a type
    // tag, not a permission. The permission bits are the filesystem's own
    // when it stores them (ext2 does, and OpenRC's `test -x` gate lives on
    // them); a filesystem without them gets the kind's default, which keeps
    // directories traversable and devices usable.
    let format = match meta.kind {
        NodeKind::File => 0o100_000,
        NodeKind::Dir => 0o040_000,
        NodeKind::CharDev => 0o020_000,
        NodeKind::BlockDev => 0o060_000,
        NodeKind::Symlink => 0o120_000,
        NodeKind::Socket => 0o140_000,
    };
    if stored != 0 {
        format | (stored & 0o7777)
    } else {
        match meta.kind {
            NodeKind::File => 0o100_644,     // regular file rw-r--r--
            NodeKind::Dir => 0o040_755,      // directory rwxr-xr-x
            NodeKind::CharDev => 0o020_666,  // character device rw-rw-rw-
            NodeKind::BlockDev => 0o060_660, // block device brw-rw----
            NodeKind::Symlink => 0o120_777,  // symbolic link rwxrwxrwx
            NodeKind::Socket => 0o140_755,   // socket srwxr-xr-x until chmod
        }
    }
}

// ---------------------------------------------------------------------------
// stat(2): the 144-byte struct stat of the x86_64 ABI
// ---------------------------------------------------------------------------

/// The three clocks `struct stat` and `struct statx` report. A filesystem that
/// keeps them separately (ext2) hands back three distinct values; one that
/// keeps none reports the epoch, and one that keeps a single clock reports it
/// for all three.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StatTimes {
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

/// The hard-link count `stat` reports for a node whose filesystem does not
/// track one (`nlink()` answered 0): a directory has the two names POSIX gives
/// it - `.` and the entry in its parent - and everything else has the one it
/// was created under.
pub(crate) fn nlink_of(meta: &Metadata, tracked: u64) -> u64 {
    if tracked != 0 {
        tracked
    } else if meta.kind == NodeKind::Dir {
        2
    } else {
        1
    }
}

/// Write the 144-byte Linux x86_64 `struct stat` for `meta`.
///
/// The field offsets are the ABI, so they are spelled as the byte offsets of
/// the struct rather than as indices into a `u64` view - an off-by-one word
/// here is invisible to every caller that only reads `st_mode`, and silently
/// answers "empty file" to the ones that ask for `st_size`.
pub(crate) fn write_stat(
    buf: usize,
    meta: &Metadata,
    dev: u64,
    rdev: Option<(u32, u32)>,
    stored: u32,
    nlink: u64,
    owner: (u32, u32),
    times: StatTimes,
) -> isize {
    if !buffer_ok(buf, 144, true, true) {
        return err::EFAULT;
    }
    let nlink = nlink_of(meta, nlink);
    let size = meta.size;
    let blocks: u64 = size.div_ceil(512);
    // The device number a character device node carries, packed the way
    // `makedev` lays it out: major in the high bytes, minor in the low.
    let packed_rdev = match rdev {
        Some((major, minor)) => ((major as u64) << 8) | (minor as u64 & 0xff) | 0,
        None => 0,
    };

    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 144);
        let at = |offset: usize| (buf + offset) as *mut u8;
        // st_dev names the *filesystem*, not the file: two files on one
        // filesystem share it, and a file on another has its own. That is
        // exactly the comparison `switch_root` makes before it deletes the
        // old root, and the one `df` makes to group what it reports.
        (at(0) as *mut u64).write_unaligned(dev);
        (at(8) as *mut u64).write_unaligned(meta.ino); // st_ino
        (at(16) as *mut u64).write_unaligned(nlink); // st_nlink
        (at(24) as *mut u32).write_unaligned(mode_of(meta, stored)); // st_mode
        // The owner is the node's, not the caller's: a filesystem that keeps
        // one answers with what it holds, and a node it never stamped answers
        // root's. st_rdev carries the node's own device number - the identity
        // libc's `ttyname` matches a descriptor by.
        (at(28) as *mut u32).write_unaligned(owner.0); // st_uid
        (at(32) as *mut u32).write_unaligned(owner.1); // st_gid
        (at(40) as *mut u64).write_unaligned(packed_rdev); // st_rdev
        (at(48) as *mut u64).write_unaligned(size); // st_size
        (at(56) as *mut u64).write_unaligned(512); // st_blksize
        (at(64) as *mut u64).write_unaligned(blocks); // st_blocks
        // The three clocks, each the value the filesystem kept for it. The
        // nanosecond halves stay 0: ext2's on-disk inode has no sub-second
        // field, and reporting a precise-looking zero the clock does not have
        // would be a lie of the second kind.
        (at(72) as *mut i64).write_unaligned(times.atime as i64); // st_atime
        (at(80) as *mut i64).write_unaligned(times.mtime as i64); // st_mtime
        (at(88) as *mut i64).write_unaligned(times.ctime as i64); // st_ctime
    }
    0
}

// ---------------------------------------------------------------------------
// statfs(2): the 120-byte struct statfs of the x86_64 ABI
// ---------------------------------------------------------------------------

/// The superblock magic of a mounted filesystem name, as Linux defines the
/// field. A caller that switches roots reads exactly this field - answering
/// "ext2" for a ramfs is what made `switch_root` refuse to switch.
pub(crate) fn fstype_magic(name: &str) -> u64 {
    match name {
        "ramfs" => 0x858458F6,
        // tmpfs and the devfs built over it are the same kind of tree here.
        "tmpfs" | "devfs" | "devtmpfs" => 0x01021994,
        "proc" => 0x00009FA0,
        "sysfs" => 0x62656572,
        "ext2" | "ext3" | "ext4" => 0x0000EF53,
        _ => 0,
    }
}

/// Write the 120-byte Linux x86_64 `struct statfs`: `f_type` and one word
/// per counter, with the two-`u32` `f_fsid` left zeroed. Every field is a
/// word except `f_fsid`, and a zero `f_frsize` reads as "same as f_bsize".
pub(crate) fn write_statfs(
    buf: usize,
    magic: u64,
    bsize: u64,
    blocks: u64,
    bfree: u64,
    files: u64,
    ffree: u64,
) -> isize {
    if !buffer_ok(buf, 120, true, true) {
        return err::EFAULT;
    }
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 120);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u64).write_unaligned(magic); // f_type
        (at(8) as *mut u64).write_unaligned(bsize); // f_bsize
        (at(16) as *mut u64).write_unaligned(blocks); // f_blocks
        (at(24) as *mut u64).write_unaligned(bfree); // f_bfree
        (at(32) as *mut u64).write_unaligned(bfree); // f_bavail
        (at(40) as *mut u64).write_unaligned(files); // f_files
        (at(48) as *mut u64).write_unaligned(ffree); // f_ffree
        (at(64) as *mut u64).write_unaligned(255); // f_namelen
        // f_fsid (56..64), f_frsize (72), f_flags (80) and the four spares
        // stay 0.
    }
    0
}

// ---------------------------------------------------------------------------
// statx(2): the 152-byte struct statx head userland reads
// ---------------------------------------------------------------------------

/// `STATX_BASIC_STATS`: every bit a `stat(2)`-shaped answer fills.
pub(super) const STATX_BASIC_STATS: u32 = 0x7ff;
/// The bytes of `struct statx` this kernel writes through `stx_mnt_id`;
/// fields the caller did not ask about read as absent.
pub(super) const STATX_SIZE: usize = 152;

/// Write `struct statx` for `meta`, reporting the `STATX_BASIC_STATS` bits
/// of `mask` as filled and everything else as absent.
pub(crate) fn write_statx(
    buf: usize,
    mask: u32,
    meta: &Metadata,
    stored: u32,
    rdev: Option<(u32, u32)>,
    nlink: u64,
    owner: (u32, u32),
    times: StatTimes,
) -> isize {
    if !buffer_ok(buf, STATX_SIZE, true, true) {
        return err::EFAULT;
    }
    let nlink = nlink_of(meta, nlink);
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, STATX_SIZE);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u32).write_unaligned(mask & STATX_BASIC_STATS); // stx_mask
        (at(4) as *mut u32).write_unaligned(512); // stx_blksize
        (at(16) as *mut u32).write_unaligned(nlink as u32); // stx_nlink
        (at(20) as *mut u32).write_unaligned(owner.0); // stx_uid
        (at(24) as *mut u32).write_unaligned(owner.1); // stx_gid
        (at(28) as *mut u16).write_unaligned(mode_of(meta, stored) as u16); // stx_mode
        (at(32) as *mut u64).write_unaligned(meta.ino); // stx_ino
        (at(40) as *mut u64).write_unaligned(meta.size); // stx_size
        (at(48) as *mut u64).write_unaligned(meta.size.div_ceil(512) as u64); // stx_blocks
        if let Some((major, minor)) = rdev {
            (at(72) as *mut u32).write_unaligned(major); // stx_rdev_major
            (at(76) as *mut u32).write_unaligned(minor); // stx_rdev_minor
        }
        // stx_atime / stx_mtime / stx_ctime at 96, 112, 128 - each the value
        // the filesystem kept for it, with a zero nanosecond half (ext2's
        // on-disk inode has no sub-second clock).
        (at(96) as *mut i64).write_unaligned(times.atime as i64); // stx_atime
        (at(104) as *mut i32).write_unaligned(0); // stx_atime_nsec
        (at(112) as *mut i64).write_unaligned(times.mtime as i64); // stx_mtime
        (at(120) as *mut i32).write_unaligned(0); // stx_mtime_nsec
        (at(128) as *mut i64).write_unaligned(times.ctime as i64); // stx_ctime
        (at(136) as *mut i32).write_unaligned(0); // stx_ctime_nsec
    }
    0
}

// ---------------------------------------------------------------------------
// getdents64(2): the record format
// ---------------------------------------------------------------------------

/// The `d_type` byte a node kind becomes, in the `DT_*` numbering.
pub(crate) fn dt_type_of(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::File => 8,     // DT_REG
        NodeKind::Dir => 4,      // DT_DIR
        NodeKind::CharDev => 2,  // DT_CHR
        NodeKind::BlockDev => 6, // DT_BLK
        NodeKind::Symlink => 10, // DT_LNK
        NodeKind::Socket => 12,  // DT_SOCK
    }
}

/// The `d_reclen` a name occupies: the 19-byte header plus the name and its
/// NUL, rounded up to the 8-byte alignment the format demands.
pub(crate) fn reclen_of(name_len: usize) -> usize {
    (19 + name_len + 1 + 7) & !7
}

// ---------------------------------------------------------------------------
// lseek(2): whence arithmetic
// ---------------------------------------------------------------------------

/// The position `lseek(fd, offset, whence)` asks for, or `None` for a
/// `whence` that is not one of the three - or a result that would land
/// before byte 0, which is `EINVAL` rather than wrapping.
pub(crate) fn lseek_target(cur: i64, size: i64, offset: i64, whence: i32) -> Option<i64> {
    let new_pos = match whence {
        0 => Some(offset),             // SEEK_SET
        1 => cur.checked_add(offset),  // SEEK_CUR
        2 => size.checked_add(offset), // SEEK_END
        _ => None,
    };
    match new_pos {
        Some(pos) if pos >= 0 => Some(pos),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// mknod(2): the S_IFMT field of the mode picks the node's kind
// ---------------------------------------------------------------------------

/// The node kind a `mknod` mode names. A FIFO is a plain file here: the
/// descriptor layer cannot tell. Anything else in the `S_IFMT` field -
/// sockets, and every mode with no type at all - is refused.
pub(crate) fn mknod_kind(mode: usize) -> Result<NodeKind, isize> {
    match mode & 0o170000 {
        0o020000 => Ok(NodeKind::CharDev),
        0o060000 => Ok(NodeKind::BlockDev),
        0o010000 => Ok(NodeKind::File),
        _ => Err(err::EINVAL),
    }
}

// ---------------------------------------------------------------------------
// wait(2): the status word
// ---------------------------------------------------------------------------

/// Encode an exit the way `WIFEXITED` and friends decode it.
///
/// mpos records a death by signal as the negated signal number, because that is
/// the shape the rest of the kernel wants; the word Linux reports is different,
/// so the sign is what carries the distinction here.
pub(crate) fn wait_status(code: isize) -> i32 {
    if code < 0 {
        // Killed by a signal: its number fills the low byte. The core-dump bit
        // stays clear - mpos writes no core files.
        (-code & 0x7f) as i32
    } else {
        // A normal exit: the low byte of the code, in bits 15:8.
        ((code & 0xff) << 8) as i32
    }
}

// ---------------------------------------------------------------------------
// ptrace(2): the request numbers, the options, and the shapes it reports
// ---------------------------------------------------------------------------

/// `ptrace(2)`'s request numbers, x86_64 column.
///
/// The low ones are the original pair-per-operation set and the `0x4200`s are
/// the ones added later; both numbers are what a libc passes and neither is
/// negotiable. Only the subset a tracer actually reaches is named here - the
/// kernel answers `EIO` for the rest, which is Linux's answer for a request it
/// does not know.
pub(crate) const PTRACE_TRACEME: usize = 0;
pub(crate) const PTRACE_PEEKTEXT: usize = 1;
pub(crate) const PTRACE_PEEKDATA: usize = 2;
pub(crate) const PTRACE_PEEKUSER: usize = 3;
pub(crate) const PTRACE_POKETEXT: usize = 4;
pub(crate) const PTRACE_POKEDATA: usize = 5;
pub(crate) const PTRACE_POKEUSER: usize = 6;
pub(crate) const PTRACE_CONT: usize = 7;
pub(crate) const PTRACE_KILL: usize = 8;
pub(crate) const PTRACE_SINGLESTEP: usize = 9;
pub(crate) const PTRACE_GETREGS: usize = 12;
pub(crate) const PTRACE_SETREGS: usize = 13;
pub(crate) const PTRACE_GETFPREGS: usize = 14;
pub(crate) const PTRACE_SETFPREGS: usize = 15;
pub(crate) const PTRACE_ATTACH: usize = 16;
pub(crate) const PTRACE_DETACH: usize = 17;
pub(crate) const PTRACE_SYSCALL: usize = 24;
pub(crate) const PTRACE_SETOPTIONS: usize = 0x4200;
pub(crate) const PTRACE_GETEVENTMSG: usize = 0x4201;
pub(crate) const PTRACE_GETSIGINFO: usize = 0x4202;
pub(crate) const PTRACE_SETSIGINFO: usize = 0x4203;
pub(crate) const PTRACE_GETREGSET: usize = 0x4204;
pub(crate) const PTRACE_SETREGSET: usize = 0x4205;
pub(crate) const PTRACE_GET_SYSCALL_INFO: usize = 0x420E;

/// `PTRACE_O_*`: what the tracer asked to be told about.
///
/// `TRACESYSGOOD` is the one that matters most in practice, and for a
/// cosmetic-looking reason that is not cosmetic: without it a syscall stop is
/// reported as a plain `SIGTRAP`, which a debugger cannot tell from a
/// breakpoint or a real `SIGTRAP` arriving. With it the reported signal is
/// `SIGTRAP | 0x80`, which cannot be anything else.
pub(crate) const PTRACE_O_TRACESYSGOOD: usize = 0x0000_0001;
pub(crate) const PTRACE_O_TRACEFORK: usize = 0x0000_0002;
pub(crate) const PTRACE_O_TRACEVFORK: usize = 0x0000_0004;
pub(crate) const PTRACE_O_TRACECLONE: usize = 0x0000_0008;
pub(crate) const PTRACE_O_TRACEEXEC: usize = 0x0000_0010;
pub(crate) const PTRACE_O_TRACEVFORKDONE: usize = 0x0000_0020;
pub(crate) const PTRACE_O_TRACEEXIT: usize = 0x0000_0040;
pub(crate) const PTRACE_O_EXITKILL: usize = 0x0010_0000;

/// The bit `TRACESYSGOOD` sets in the reported stop signal.
pub(crate) const SYSCALL_TRAP: u8 = 0x80;

/// `PTRACE_EVENT_*`: the value in bits 23:16 of a `SIGTRAP` stop's status
/// word. Zero means "an ordinary stop", which is every syscall boundary and
/// every signal delivery.
pub(crate) const PTRACE_EVENT_NONE: u32 = 0;

/// The stop a tracee makes to announce a child it has just created.
///
/// Which of the three it is comes from the flags the child was asked for, not
/// from the call: `fork`, `vfork` and a thread-creating `clone` are one syscall
/// with different flags, and a tracer that asked about threads but not about
/// processes needs to be told which one it got. At this stop
/// `PTRACE_GETEVENTMSG` answers the **new child's pid**.
pub(crate) const PTRACE_EVENT_FORK: u32 = 1;
pub(crate) const PTRACE_EVENT_VFORK: u32 = 2;
pub(crate) const PTRACE_EVENT_CLONE: u32 = 3;

/// The exec event: raised on the far side of an `execve` that succeeded. See
/// `sched::exec_trap`. `PTRACE_GETEVENTMSG` answers the tid the task had
/// *before* it exec'd at this stop, which is the question a tracer that
/// reassembles a thread group after the leader exec'd is asking.
pub(crate) const PTRACE_EVENT_EXEC: u32 = 4;

/// The events this kernel accepts a name for and does not produce.
///
/// `VFORK_DONE` and `EXIT` are each a separate stop a tracer can ask to be told
/// about with the matching `PTRACE_O_*`, and this kernel raises neither: an
/// `EXIT` stop would be a second stop at the very end of a task's life, in the
/// middle of the release path a terminating task is already inside, and
/// `VFORK_DONE` needs a parent that is suspended for the child's benefit - which
/// [`crate::sched::clone`] deliberately is not.
///
/// They are named rather than absent because `PTRACE_SETOPTIONS` refuses an
/// option it cannot name, and a tracer that died of `EINVAL` because this
/// kernel had never heard of `PTRACE_O_TRACEEXIT` would be worse served than one
/// that asked and was quietly not told. The allowance lives in one module so
/// that the set of "named but never produced" is one line to read.
#[allow(dead_code)]
pub(crate) mod unwatched_events {
    pub(crate) const VFORK_DONE: u32 = 5;
    pub(crate) const EXIT: u32 = 6;
}

/// `NT_PRSTATUS`, the register-set a `PTRACE_GETREGSET` asks for by name.
pub(crate) const NT_PRSTATUS: usize = 1;

/// The `wait(2)` option bits a tracer passes, beyond `WNOHANG`.
///
/// `__WALL` is the one `strace` waits with: it is documented as Linux-only and
/// "do not use", and every tracer uses it anyway, because it is the only way
/// to wait for a child that was not created with `SIGCHLD` as its exit signal.
pub(crate) const WNOHANG: usize = 0x0000_0001;
pub(crate) const WUNTRACED: usize = 0x0000_0002;
pub(crate) const WCONTINUED: usize = 0x0000_0008;
pub(crate) const __WNOTHREAD: usize = 0x2000_0000;
pub(crate) const __WALL: usize = 0x4000_0000;
pub(crate) const __WCLONE: usize = 0x8000_0000;

/// The status word `WIFSTOPPED` decodes: `0x7f` in the low byte, the signal
/// that caused the stop in bits 15:8, and a `PTRACE_EVENT_*` in bits 23:16.
///
/// The three fields share the word because the macros read it that way -
/// `WSTOPSIG(w)` is `(w >> 8) & 0xff` and `WIFSTOPPED(w)` is
/// `(w & 0xff) == 0x7f` - and a `ptrace` event is expressed as a `SIGTRAP`
/// whose high byte carries the event number. Writing the fields anywhere else
/// produces a status the libc decodes into a plausible, wrong answer.
pub(crate) fn stop_status(sig: u8, event: u32) -> i32 {
    0x7f | ((sig as i32) << 8) | ((event as i32) << 16)
}

/// The status word `WIFCONTINUED` decodes: all ones.
pub(crate) const CONTINUED_STATUS: i32 = 0xffff;

/// `struct user_regs_struct` on x86_64, as `PTRACE_GETREGS` fills it.
///
/// The order is the ABI's, not a choice: the structure is what `strace` and
/// `gdb` read `rip`, `rsp` and `orig_rax` out of by offset, and a field in
/// the wrong slot is a debugger looking at the wrong register.
pub(crate) mod uregs {
    pub const R15: usize = 0;
    pub const R14: usize = 1;
    pub const R13: usize = 2;
    pub const R12: usize = 3;
    pub const RBP: usize = 4;
    pub const RBX: usize = 5;
    pub const R11: usize = 6;
    pub const R10: usize = 7;
    pub const R9: usize = 8;
    pub const R8: usize = 9;
    pub const RAX: usize = 10;
    pub const RCX: usize = 11;
    pub const RDX: usize = 12;
    pub const RSI: usize = 13;
    pub const RDI: usize = 14;
    pub const ORIG_RAX: usize = 15;
    pub const RIP: usize = 16;
    pub const CS: usize = 17;
    pub const EFLAGS: usize = 18;
    pub const RSP: usize = 19;
    pub const SS: usize = 20;
    pub const FS_BASE: usize = 21;
    pub const GS_BASE: usize = 22;
    pub const DS: usize = 23;
    pub const ES: usize = 24;
    pub const FS: usize = 25;
    pub const GS: usize = 26;
    pub const COUNT: usize = 27;
    pub const BYTES: usize = COUNT * 8;
}

/// Copy a register file out to the tracer's buffer, in `user_regs_struct`'s
/// order. The caller has already checked the buffer is writable.
pub(crate) fn write_user_regs(at: usize, regs: &[u64; uregs::COUNT]) {
    for (slot, value) in regs.iter().enumerate() {
        unsafe { ((at + slot * 8) as *mut u64).write_unaligned(*value) };
    }
}

/// Read a register file back from the tracer's buffer. The caller has already
/// checked the buffer is readable.
pub(crate) fn read_user_regs(at: usize) -> [u64; uregs::COUNT] {
    let mut regs = [0u64; uregs::COUNT];
    for (slot, value) in regs.iter_mut().enumerate() {
        *value = unsafe { ((at + slot * 8) as *const u64).read_unaligned() };
    }
    regs
}

/// `sizeof(struct user_fpregs_struct)` on x86_64: the floating-point register
/// file `PTRACE_GETFPREGS` and `PTRACE_SETFPREGS` move.
///
/// It is 512 rather than "some sensible size" because on this architecture the
/// structure *is* the `fxsave` image - the x87 control/status/tag words, the
/// eight x87 registers, `MXCSR` and the sixteen XMM registers, in the order the
/// instruction writes them. That is what lets a tracer's buffer be filled
/// straight from, and stored straight into, the block a task carries.
pub(crate) const FPREGS_BYTES: usize = 512;

/// The floating-point image, for a tracer to read. Word at a time and unaligned
/// for the same reason the register file is: the buffer belongs to the tracer,
/// and its alignment is whatever the tracer chose.
pub(crate) fn write_user_fpregs(at: usize, bytes: &[u8; FPREGS_BYTES]) {
    for (word, chunk) in bytes.chunks_exact(8).enumerate() {
        let value = u64::from_le_bytes(chunk.try_into().unwrap());
        unsafe { ((at + word * 8) as *mut u64).write_unaligned(value) };
    }
}

/// And the reading half. The caller has checked the buffer is readable.
pub(crate) fn read_user_fpregs(at: usize) -> [u8; FPREGS_BYTES] {
    let mut bytes = [0u8; FPREGS_BYTES];
    for (word, chunk) in bytes.chunks_exact_mut(8).enumerate() {
        let value = unsafe { ((at + word * 8) as *const u64).read_unaligned() };
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// `struct ptrace_syscall_info`: what `PTRACE_GET_SYSCALL_INFO` reports about
/// the syscall a tracee stopped at.
///
/// ```text
///   0  __u8  op                  1 = entry, 2 = exit
///   1  __u8  pad[3]
///   4  __u32 arch                AUDIT_ARCH_X86_64
///   8  __u64 instruction_pointer
///  16  __u64 stack_pointer
///  24  union {
///        entry: { __u64 nr; __u64 args[6]; }   56 bytes
///        exit:  { __s64 rval; __u8 is_error; } 16 bytes, padded
///      }
/// ```
///
/// The union is why this is 80 bytes and not 24: the largest member decides
/// the size, and the exit shape is written into the same 56 bytes the entry
/// shape uses.
pub(crate) const SYSCALL_INFO_BYTES: usize = 80;
/// `PTRACE_SYSCALL_INFO_NONE`: the operation byte at a stop that is not a
/// syscall boundary. A tracer reads it to tell "here are the arguments" from
/// "there are no arguments to show you".
pub(crate) const SYSCALL_INFO_NONE: u8 = 0;
pub(crate) const SYSCALL_INFO_ENTRY: u8 = 1;
pub(crate) const SYSCALL_INFO_EXIT: u8 = 2;
/// `AUDIT_ARCH_X86_64` - what `syscall_get_arch()` answers on this machine,
/// and what the `arch` field carries.
pub(crate) const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// Write the `struct ptrace_syscall_info` describing a syscall boundary.
///
/// `nr`, `args`, `ip` and `sp` describe the *entry*; at an exit stop the only
/// field with anything new to say is the return value, and the entry half is
/// still written because the caller's buffer is 80 bytes either way and
/// leaving it untouched would hand back whatever was there before.
pub(crate) fn write_syscall_info(
    at: usize,
    entry: bool,
    nr: u64,
    args: &[u64; 6],
    ip: u64,
    sp: u64,
    ret: isize,
) {
    unsafe {
        core::ptr::write_bytes(at as *mut u8, 0, SYSCALL_INFO_BYTES);
        (at as *mut u8).write_unaligned(if entry {
            SYSCALL_INFO_ENTRY
        } else {
            SYSCALL_INFO_EXIT
        });
        ((at + 4) as *mut u32).write_unaligned(AUDIT_ARCH_X86_64);
        ((at + 8) as *mut u64).write_unaligned(ip);
        ((at + 16) as *mut u64).write_unaligned(sp);
        if entry {
            ((at + 24) as *mut u64).write_unaligned(nr);
            for (slot, value) in args.iter().enumerate() {
                ((at + 32 + slot * 8) as *mut u64).write_unaligned(*value);
            }
        } else {
            ((at + 24) as *mut i64).write_unaligned(ret as i64);
            // `IS_ERR_VALUE`: a return in the last 4095 slots below zero is an
            // errno, not a result. The boundary is what every libc uses, and
            // the caller reads `is_error` to decide whether to negate it.
            let is_error = ret < 0 && ret >= -4095;
            ((at + 32) as *mut u8).write_unaligned(is_error as u8);
        }
    }
}

// ---------------------------------------------------------------------------
// waitid(2): the siginfo_t an exit arrives as
// ---------------------------------------------------------------------------

/// `sizeof(siginfo_t)`: 128 bytes, the whole struct on x86_64.
pub(super) const SIGINFO_BYTES: usize = 128;
const SIGCHLD: i32 = 17;
const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;

/// Write the `siginfo_t` describing a collected child: `SIGCHLD` as the
/// signo, `CLD_EXITED` or `CLD_KILLED` as the code, and the pid and status
/// in the fields `si_pid` and `si_status` occupy.
pub(crate) fn write_waitid_info(infop: usize, pid: u32, code: isize) -> isize {
    let (si_code, status) = if code < 0 {
        (CLD_KILLED, -code)
    } else {
        (CLD_EXITED, code)
    };
    unsafe {
        core::ptr::write_bytes(infop as *mut u8, 0, SIGINFO_BYTES);
        (infop as *mut i32).write_unaligned(SIGCHLD); // si_signo
        ((infop + 8) as *mut i32).write_unaligned(si_code); // si_code
        ((infop + 16) as *mut i32).write_unaligned(pid as i32); // si_pid
        ((infop + 24) as *mut i32).write_unaligned(status as i32); // si_status
    }
    0
}

// ---------------------------------------------------------------------------
// clone(2): the flag word
// ---------------------------------------------------------------------------

/// Validate a `clone` flag word the way Linux does, before anything is
/// created. Returns `(exit_signal, flags)` with the low byte split off.
///
/// The low byte of the flag word is not a flag: it is the *signal* the
/// parent should be sent when the child exits, which is why a word like
/// `CLONE_VM|CLONE_VFORK|SIGCHLD` - exactly what a libc's `posix_spawn`
/// passes - has a plain signal number mixed into it.
pub(crate) fn validate_clone_flags(
    flags: usize,
    parent_tid: usize,
    child_tid: usize,
) -> Result<(usize, usize), isize> {
    const CSIGNAL: usize = 0x0000_00ff;
    let exit_signal = flags & CSIGNAL;
    let flags = flags & !CSIGNAL;

    // The bits this kernel understands. `CLONE_DETACHED` and `CLONE_UNTRACED`
    // are accepted and ignored exactly as Linux ignores them: both are
    // historical no-ops, and musl's `pthread_create` sends the first one in
    // every thread's flag word - refusing it refuses every pthread in the
    // system. Anything else is a request from a kernel newer than this one.
    const KNOWN: usize = crate::syscall::linux::nr::CLONE_VM
        | crate::syscall::linux::nr::CLONE_FS
        | crate::syscall::linux::nr::CLONE_FILES
        | crate::syscall::linux::nr::CLONE_SIGHAND
        | crate::syscall::linux::nr::CLONE_VFORK
        | crate::syscall::linux::nr::CLONE_PARENT
        | crate::syscall::linux::nr::CLONE_THREAD
        | crate::syscall::linux::nr::CLONE_SYSVSEM
        | crate::syscall::linux::nr::CLONE_SETTLS
        | crate::syscall::linux::nr::CLONE_PARENT_SETTID
        | crate::syscall::linux::nr::CLONE_CHILD_CLEARTID
        | crate::syscall::linux::nr::CLONE_CHILD_SETTID
        | crate::syscall::linux::nr::CLONE_DETACHED
        | crate::syscall::linux::nr::CLONE_UNTRACED;
    if flags & !KNOWN != 0 {
        return Err(err::EINVAL);
    }
    // A signal number that names nothing is not a request to deliver anything;
    // Linux refuses it rather than hiding a caller's mistake behind a working
    // call.
    if exit_signal > 31 {
        return Err(err::EINVAL);
    }
    // The dependency chain Linux enforces: a thread shares its creator's
    // signal handlers and address space, and a signal-handler table can only
    // be shared by processes that also share memory.
    if flags & crate::syscall::linux::nr::CLONE_THREAD != 0
        && (flags & crate::syscall::linux::nr::CLONE_SIGHAND == 0
            || flags & crate::syscall::linux::nr::CLONE_VM == 0)
    {
        return Err(err::EINVAL);
    }
    if flags & crate::syscall::linux::nr::CLONE_SIGHAND != 0
        && flags & crate::syscall::linux::nr::CLONE_VM == 0
    {
        return Err(err::EINVAL);
    }
    // A tid must land somewhere; a flag that asks for the write without a
    // place to put it is refused rather than ignored.
    if flags & crate::syscall::linux::nr::CLONE_PARENT_SETTID != 0 && parent_tid == 0 {
        return Err(err::EINVAL);
    }
    if flags
        & (crate::syscall::linux::nr::CLONE_CHILD_SETTID
            | crate::syscall::linux::nr::CLONE_CHILD_CLEARTID)
        != 0
        && child_tid == 0
    {
        return Err(err::EINVAL);
    }
    // Both tid pointers, when present, must be writable in the caller.
    if flags & crate::syscall::linux::nr::CLONE_PARENT_SETTID != 0
        && !buffer_ok(parent_tid, 4, true, true)
    {
        return Err(err::EFAULT);
    }
    if flags
        & (crate::syscall::linux::nr::CLONE_CHILD_SETTID
            | crate::syscall::linux::nr::CLONE_CHILD_CLEARTID)
        != 0
        && !buffer_ok(child_tid, 4, true, true)
    {
        return Err(err::EFAULT);
    }
    Ok((exit_signal, flags))
}

// ---------------------------------------------------------------------------
// reboot(2): the magic cmd words
// ---------------------------------------------------------------------------

/// The `cmd` words of `reboot(2)`, Linux x86_64. They are magic numbers, not
/// flag bits, and each is its own sentence.
pub(super) const RB_DISABLE_CAD: usize = 0x0000_0000;
pub(super) const RB_ENABLE_CAD: usize = 0x89ab_cdef;
pub(super) const RB_AUTOBOOT: usize = 0x0123_4567;
pub(super) const RB_HALT_SYSTEM: usize = 0xcdef_0123;
pub(super) const RB_POWER_OFF: usize = 0x4321_fedc;

/// What a `reboot` cmd asks of the machine: a policy word to accept, a stop
/// word to end it with, or something no kernel should be told.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RebootCmd {
    /// ctrl-alt-del's disposition: recorded nowhere, answered with success.
    Policy,
    /// One of the three stop words: the machine goes down.
    Stop,
    Unknown,
}

pub(crate) fn reboot_classify(cmd: usize) -> RebootCmd {
    match cmd {
        RB_DISABLE_CAD | RB_ENABLE_CAD => RebootCmd::Policy,
        RB_AUTOBOOT | RB_HALT_SYSTEM | RB_POWER_OFF => RebootCmd::Stop,
        _ => RebootCmd::Unknown,
    }
}

/// The two words that have to come before `cmd`, as `reboot(2)` declares them:
/// `reboot(int magic, int magic2, int cmd, void *arg)`. The command word is
/// the **third** argument, not the first - a handler that classified `args[0]`
/// classified `LINUX_REBOOT_MAGIC1` and answered `EINVAL`, which is why
/// `poweroff -f` said "Invalid argument" and never ended the machine.
pub(super) const RB_MAGIC1: usize = 0xfee1_dead;
pub(super) const RB_MAGIC2: usize = 0x2812_1969;
/// The two older `magic2` values Linux still accepts, from before the
/// constants were renamed: `LINUX_REBOOT_MAGIC2A` and `LINUX_REBOOT_MAGIC2B`.
const RB_MAGIC2A: usize = 0x2011_2000;
const RB_MAGIC2B: usize = 0x0511_1996;

/// Read `reboot`'s arguments the way the man page orders them: the magic
/// words first, since a call that does not name them is refused before its
/// command is even looked at, then the command itself.
///
/// `Ok(RebootCmd::Unknown)` is a call that named the magic and asked for
/// something no kernel knows: that is `EINVAL` too, and it is the one a
/// program sees when *it* passes a bad word rather than when we mislaid one.
pub(crate) fn reboot_command(args: [usize; crate::syscall::ARG_COUNT]) -> Result<RebootCmd, isize> {
    if args[0] != RB_MAGIC1 {
        return Err(err::EINVAL);
    }
    if args[1] != RB_MAGIC2 && args[1] != RB_MAGIC2A && args[1] != RB_MAGIC2B {
        return Err(err::EINVAL);
    }
    match reboot_classify(args[2]) {
        RebootCmd::Unknown => Err(err::EINVAL),
        other => Ok(other),
    }
}

// ---------------------------------------------------------------------------
// The timespec-carrying calls: one validation per man page
// ---------------------------------------------------------------------------

/// The millisecond duration `nanosleep`'s `struct timespec` names, with the
/// man page's validation: `tv_nsec` must stay below a second. The sub-second
/// field truncates - a request that names no whole milliseconds is a request
/// to sleep no measurable time.
pub(crate) fn nanosleep_ms(sec: u64, nsec: u64) -> Result<u64, isize> {
    if nsec >= 1_000_000_000 {
        return Err(err::EINVAL);
    }
    // Both folds saturate: a caller that hands over u64::MAX seconds asked
    // for a wait longer than the tick count can name, and that is a long
    // sleep, not an overflow - the tick conversion on the machine side
    // saturates the rest of the way.
    Ok(sec.saturating_mul(1000).saturating_add(nsec / 1_000_000))
}

/// The millisecond duration a `ppoll` timeout names. `nsec` must stay below
/// a second, and the sub-second field rounds *up*: a caller that asked for
/// one nanosecond must not get a zero-millisecond wait, or a poll that
/// expects "at least until then" returns early.
pub(crate) fn ppoll_ms(sec: u64, nsec: u64) -> Result<u64, isize> {
    if nsec >= 1_000_000_000 {
        return Err(err::EINVAL);
    }
    Ok(sec
        .saturating_mul(1000)
        .saturating_add(nsec.div_ceil(1_000_000)))
}

/// The millisecond duration a `select`/`pselect6` timeout names. `timespec`
/// picks the shape: `pselect6` hands a `struct timespec` (nanoseconds),
/// `select` a `struct timeval` (microseconds), and the sub-second field is
/// validated against its own unit before it is folded in. Beyond a few
/// centuries, or a fraction that does not fit its unit, is a corrupted
/// timeout - Linux clamps; a bounded answer beats wrapping.
pub(crate) fn select_timeout_ms(sec: u64, frac: u64, timespec: bool) -> Result<u64, isize> {
    let unit = if timespec { 1_000_000 } else { 1_000 };
    if sec >= (1 << 43) || frac >= unit * 1_000 {
        return Err(err::EINVAL);
    }
    Ok(sec * 1000 + frac.div_ceil(unit))
}

/// The `(value, interval)` milliseconds a `setitimer`'s `struct itimerval`
/// names, validated the way the man page words it: each `tv_usec` must stay
/// below a second's worth. A zero value is the "disarm" answer the caller
/// decides on, not an error.
pub(crate) fn itimerval_ms(
    vsec: u64,
    vusec: u64,
    isec: u64,
    iusec: u64,
) -> Result<(u64, u64), isize> {
    if vusec >= 1_000_000 || iusec >= 1_000_000 {
        return Err(err::EINVAL);
    }
    let value_ms = vsec
        .saturating_mul(1000)
        .saturating_add(vusec.div_ceil(1000));
    let interval_ms = isec
        .saturating_mul(1000)
        .saturating_add(iusec.div_ceil(1000));
    Ok((value_ms, interval_ms))
}

/// One `struct timespec` of a `timer_settime`'s `struct itimerspec`, as a tick
/// count: validated the way Linux validates it and rounded *up*, so a timer
/// armed for "half a tick from now" fires one tick from now rather than
/// immediately - the same never-early rule the other timeout conversions here
/// keep.
///
/// A negative `tv_sec`, or a `tv_nsec` outside `[0, 1e9)`, is the man page's
/// `EINVAL`; `tv_sec` large enough to overflow the tick arithmetic saturates
/// rather than wraps, because a caller that asks for a sleep past the end of
/// the machine's clock has asked for "as late as possible", not "now".
pub(crate) fn timespec_ticks(sec: i64, nsec: i64, hz: u64) -> Result<u64, isize> {
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(err::EINVAL);
    }
    let ticks = (sec as u64).saturating_mul(hz);
    let nanos_per_tick = 1_000_000_000 / hz;
    Ok(ticks.saturating_add((nsec as u64).div_ceil(nanos_per_tick)))
}

// ---------------------------------------------------------------------------
// prctl(2): the option this kernel models
// ---------------------------------------------------------------------------

/// Install one `SECCOMP_MODE_FILTER` program on the calling thread.
///
/// One implementation for the two doors Linux puts on the same operation -
/// `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER)` here and `seccomp(2)`'s
/// `SECCOMP_SET_MODE_FILTER` in `posix` - because a caller must not be able to
/// reach a different set of checks through each. It lives on this side of the
/// gate because `posix` is compiled only for the machine while this is compiled
/// for both, and the install path is exactly what a host test can argue with:
/// the validation is arithmetic over the caller's bytes.
///
/// The program is copied out of the caller's address space and validated *in
/// full* before anything is stored: a filter that was half-installed would be
/// worse than one that was refused, because by then the thread has been told it
/// is confined.
pub(super) fn install_seccomp_filter(prog_ptr: usize, flags: usize, from_user: bool) -> isize {
    // No flag is supported. `SECCOMP_FILTER_FLAG_TSYNC` would have to hand the
    // filter to every thread of the group - a decision about `clone` this
    // kernel has not made - and an unknown flag must be *refused* rather than
    // ignored: a caller that asked for one and did not get it believes
    // something about its own threads that is not true.
    if flags != 0 {
        return err::EINVAL;
    }
    // Linux lets a privileged caller install a filter, or anyone who has first
    // given up the right to gain privileges (`PR_SET_NO_NEW_PRIVS`, which this
    // kernel does not model). Every process in this image is root, so the arm
    // that matters is the first; a caller that is not root is refused rather
    // than handed a confinement it could then walk out of. A host build has no
    // identities to ask and its harness is root by construction - the same
    // reason `current_owner` answers root there.
    #[cfg(target_os = "none")]
    if crate::sched::current_identity().ruid != 0 {
        return err::EACCES;
    }
    // `struct sock_fprog`: a `u16` length, two bytes of padding, then the
    // pointer to the instructions.
    const PROG_BYTES: usize = 16;
    // The null check is not redundant with `buffer_ok`: this kernel's version
    // validates the range against the caller's own mappings, so address zero
    // fails it - but a host build has no mappings to ask and answers "fine" to
    // everything, and a program that then *read* address zero would fault the
    // harness. Every user-pointer reader here spells the guard out for exactly
    // that reason.
    if prog_ptr == 0 || !buffer_ok(prog_ptr, PROG_BYTES, false, from_user) {
        return err::EFAULT;
    }
    let len = unsafe { (prog_ptr as *const u16).read_unaligned() } as usize;
    let insns = unsafe { ((prog_ptr + 8) as *const u64).read_unaligned() } as usize;
    let Some(bytes) = len.checked_mul(8) else {
        return err::EINVAL;
    };
    if insns == 0 || !buffer_ok(insns, bytes, false, from_user) {
        return err::EFAULT;
    }
    // Safety: the range was just checked against the caller's own mappings, so
    // the bytes are readable - and they are read once, here, rather than
    // re-read per syscall on the fast path the filter then runs on.
    let raw = unsafe { core::slice::from_raw_parts(insns as *const u8, bytes) };
    match crate::seccomp::Program::parse(raw) {
        Ok(program) => {
            crate::sched::set_seccomp_filter(program);
            0
        }
        // Every way a program can be wrong is `EINVAL` - Linux's answer for a
        // malformed filter, and the one errno a caller cannot read as
        // "installed".
        Err(_) => err::EINVAL,
    }
}

/// `prctl(option, ...)` — the miscellaneous process-control call.
///
/// The name pair is what userland actually exercises here, and it round-trips:
/// `PR_SET_NAME` (15) stores the caller's bytes in the group's `comm` slot and
/// `PR_GET_NAME` (16) hands them back — or, when no one has set one, the image
/// basename `comm_of` derives, which is what Linux reports too. The seccomp
/// pair answers and latches what `seccomp(2)` does. Every other option - the
/// privilege, dump-core and endianness flags, the capability bounding set - is
/// refused with `EINVAL`, exactly as Linux's `prctl` default arm (kernel/sys.c):
/// an option this kernel does not model is not silently reported as done. An
/// option is accepted only once MPOS backs it with real state, never with an
/// empty `0` (§4.2). The gate that boots the whole initramfs is the evidence
/// that no running program relies on one of these being a silent no-op.
pub(crate) fn handle_prctl(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    const PR_SET_NAME: usize = 15;
    const PR_GET_SECCOMP: usize = 21;
    const PR_SET_SECCOMP: usize = 22;
    const PR_GET_NAME: usize = 16;
    // `TASK_COMM_LEN`: the fixed slot both name options speak of.
    const NAME_LEN: usize = 16;
    const SECCOMP_MODE_STRICT: usize = 1;
    const SECCOMP_MODE_FILTER: usize = 2;
    match args[0] {
        // Linux reads at most `TASK_COMM_LEN` bytes of the caller's string,
        // stores what fits and forces the last byte to NUL: a name that runs the
        // full sixteen without a terminator is *truncated and accepted*, not
        // refused (measured: sixteen `'a'` with a non-NUL seventeenth answers `0`,
        // and `PR_GET_NAME` then reads back fifteen characters). The arm used to
        // answer `EFAULT` there, which sends a program that named itself sixteen
        // bytes long down a fallback it would not take on Linux. A name that ends
        // inside the slot is unaffected: the copy stops at its NUL and the tail
        // stays zero.
        PR_SET_NAME => {
            let buf = args[1];
            if buf == 0 || !buffer_ok(buf, 1, false, true) {
                return err::EFAULT;
            }
            let mut name = [0u8; NAME_LEN];
            let mut i = 0;
            while i < NAME_LEN {
                // Revalidate only at a page crossing: within one page the first
                // validated byte stands for all of them, exactly as the path
                // reader assumes, and a contiguous scan reaches every boundary
                // address on its way in. A name whose bytes run off the mapping
                // faults there instead of reading unbacked memory.
                if (buf + i) & 0xfff == 0 && !buffer_ok(buf + i, 1, false, true) {
                    return err::EFAULT;
                }
                let byte = unsafe { ((buf + i) as *const u8).read_unaligned() };
                if byte == 0 {
                    break;
                }
                name[i] = byte;
                i += 1;
            }
            // Whether the loop ended at a NUL or at the slot's edge, the last
            // byte is forced to zero: that is the truncation, and it is why no
            // length bookkeeping is needed below.
            name[NAME_LEN - 1] = 0;
            crate::sched::set_current_comm(&name);
            0
        }
        PR_GET_NAME => {
            let buf = args[1];
            if buf == 0 || !buffer_ok(buf, NAME_LEN, true, true) {
                return err::EFAULT;
            }
            // The group's `comm` verbatim: the `PR_SET_NAME` bytes if one was
            // set, else the derived basename — the name Linux reads back, not
            // the sixteen zeroes that used to stand for "we have no name".
            let name = crate::sched::current_comm_bytes();
            unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), buf as *mut u8, NAME_LEN) };
            0
        }
        // The seccomp pair is the older spelling of what the `seccomp(2)`
        // syscall does. `PR_GET_SECCOMP` answers 0 while unconfined, 1
        // (`SECCOMP_MODE_STRICT`) once latched and 2 (`SECCOMP_MODE_FILTER`)
        // once a program is installed - and a thread cannot be in both at
        // once, because strict mode would have killed the `prctl` that tried
        // to add a filter.
        PR_GET_SECCOMP => {
            if crate::sched::has_seccomp_filter() {
                2
            } else {
                crate::sched::current_is_seccomp_strict() as isize
            }
        }
        PR_SET_SECCOMP => match args[1] {
            SECCOMP_MODE_STRICT => {
                crate::sched::set_seccomp_strict();
                0
            }
            // The same operation `seccomp(2)` performs, through the other
            // door: it takes no flags here, because the legacy spelling has
            // nowhere to put them.
            SECCOMP_MODE_FILTER => install_seccomp_filter(args[2], 0, true),
            _ => err::EINVAL,
        },
        // Every option not named above is refused with `EINVAL`, exactly as
        // Linux's `prctl` `default:` arm (kernel/sys.c). Returning `0` here -
        // the old catch-all - was the §4.2 "success for a thing not done" this
        // batch exists to remove: a caller of `PR_SET_DUMPABLE`, `PR_CAPBSET_*`,
        // `PR_SET_NO_NEW_PRIVS` and friends was told the kernel had done
        // something it had not. `EINVAL` sends it down the same fallback Linux
        // gives an option this kernel does not model; a specific option is only
        // accepted once MPOS backs it with real state (as the name and seccomp
        // pairs above are), never with an empty success.
        _ => err::EINVAL,
    }
}

// ---------------------------------------------------------------------------
// umask(2): the creation mask
// ---------------------------------------------------------------------------

/// `umask(mask)` — set the creation mask, and report what it was.
///
/// The mask is the process's, not the machine's: it is inherited across
/// `fork` and preserved by `exec`, so a program that narrows its own must not
/// narrow what its neighbours may create. Only the low twelve bits are kept,
/// because those are the bits a mode has.
pub(crate) fn handle_umask(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let old = crate::sched::current_umask();
    crate::sched::set_current_umask(args[0] as u32 & 0o7777);
    old as isize
}

// ---------------------------------------------------------------------------
// getrandom(2)
// ---------------------------------------------------------------------------

/// `getrandom(buf, len, flags)` — fill `buf` with random bytes.
///
/// The same stream `/dev/urandom` reads from. `GRND_NONBLOCK`, `GRND_RANDOM`
/// and `GRND_AUTOCONF` are accepted and ignored: there is one generator, it is
/// always ready, and it never blocks - which is the behaviour all three flags
/// are asking about, so none of them can be violated. The flag *word*, though,
/// is a closed set: a bit outside the three Linux refuses with `EINVAL`
/// (measured on the host: `0x100` is rejected, and the flags are checked before
/// the zero-length short-circuit), and a caller that handed one here used a
/// spelling this kernel was never asked to honour. Silently filling the buffer
/// regardless of the flag word - which the old handler did, discarding `flags`
/// entirely - was a §4.2 accept-and-drop.
pub(crate) fn handle_getrandom(args: [usize; crate::syscall::ARG_COUNT], from_user: bool) -> isize {
    const GRND_NONBLOCK: usize = 0x1;
    const GRND_RANDOM: usize = 0x2;
    const GRND_AUTOCONF: usize = 0x4;
    const KNOWN: usize = GRND_NONBLOCK | GRND_RANDOM | GRND_AUTOCONF;
    let (buf, len, flags) = (args[0], args[1], args[2]);
    if flags & !KNOWN != 0 {
        return err::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    if !buffer_ok(buf, len, true, from_user) {
        return err::EFAULT;
    }
    // Filled straight into the caller's buffer: a kernel buffer here would be
    // a heap allocation per call for a request that is usually 32 bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len) };
    crate::rand::fill(out);
    len as isize
}

// ---------------------------------------------------------------------------
// The rlimit family: getrlimit(2) / setrlimit(2) / prlimit64(2)
// ---------------------------------------------------------------------------

/// `RLIM_NLIMITS`: the resource table's length. A number past it is
/// `EINVAL`, the errno Linux uses for one it has no row for.
pub const RLIM_NLIMITS: usize = 16;
/// `RLIM_INFINITY`: what an unlimited limit reads as.
pub const RLIM_INFINITY: u64 = u64::MAX;

/// One resource limit pair: the soft ceiling a process is held to and the hard
/// ceiling it may raise the soft one to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

/// The limits a fresh process starts with: an 8 MiB stack, a descriptor
/// ceiling, a process count, and infinity for the rows nothing budgets. Every
/// other resource reads these until `setrlimit` says otherwise.
pub(crate) fn default_rlimits() -> [Rlimit; RLIM_NLIMITS] {
    let mut t = [Rlimit {
        cur: RLIM_INFINITY,
        max: RLIM_INFINITY,
    }; RLIM_NLIMITS];
    t[3] = Rlimit {
        cur: 8 * 1024 * 1024,
        max: 8 * 1024 * 1024,
    }; // RLIMIT_STACK
    t[5] = Rlimit {
        cur: 1024,
        max: 1_048_576,
    }; // RLIMIT_NOFILE
    t[7] = Rlimit {
        cur: 4096,
        max: 4096,
    }; // RLIMIT_NPROC
    t
}

/// `setrlimit`'s one rule, returned as the errno a refusal carries (or the
/// accepted pair): the soft limit may not exceed the hard, and only root may
/// lift the hard ceiling above where it stands.
pub(crate) fn rlimit_admit(old: Rlimit, new: Rlimit, is_root: bool) -> Result<Rlimit, isize> {
    if new.cur > new.max {
        return Err(err::EINVAL);
    }
    if !is_root && new.max > old.max {
        return Err(err::EPERM);
    }
    Ok(new)
}

/// The `(rlim_cur, rlim_max)` pair a resource reports. The numbers are the
/// defaults a static musl binary needs to start: an 8 MiB stack, a generous
/// descriptor ceiling, and infinity for the rest.
pub(crate) fn rlimit_pair(resource: usize) -> (u64, u64) {
    let r = default_rlimits()[resource.min(RLIM_NLIMITS - 1)];
    (r.cur, r.max)
}

/// The current process's stored limit for `resource`, as a `(cur, max)` pair.
/// On the kernel it comes from the task's own table; a host build has no
/// table, so it reads the startup defaults - which is what a test wants.
fn current_pair(resource: usize) -> (u64, u64) {
    #[cfg(target_os = "none")]
    if let Some(r) = crate::sched::current_rlimit(resource) {
        return (r.cur, r.max);
    }
    #[cfg(not(target_os = "none"))]
    let _ = resource;
    rlimit_pair(resource)
}

/// Write the `struct rlimit64 { rlim_cur, rlim_max }` pair into `out`.
fn rlimit_of(resource: usize, out: usize, from_user: bool) -> isize {
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if out == 0 {
        return 0;
    }
    if !buffer_ok(out, 16, true, from_user) {
        return err::EFAULT;
    }
    let (cur, max) = current_pair(resource);
    unsafe {
        (out as *mut u64).write_unaligned(cur);
        ((out + 8) as *mut u64).write_unaligned(max);
    }
    0
}

/// Apply a new soft/hard pair to the current process's `resource`, or report
/// the errno Linux would. A host build has no table, so a set is a no-op.
fn set_current(resource: usize, cur: u64, max: u64) -> isize {
    #[cfg(target_os = "none")]
    {
        let is_root = crate::sched::current_owner().0 == 0;
        let new = Rlimit { cur, max };
        match crate::sched::set_current_rlimit(resource, new, is_root) {
            Ok(()) => 0,
            Err(e) => e,
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (resource, cur, max);
        0
    }
}

/// `prlimit64(pid, resource, new_limit, old_limit)` — get/set one limit of a
/// process. `pid` 0 (or the caller's own) acts on the current process with the
/// full rule; another pid can be read (its stored table) but not written here,
/// which answers a cross-process set with `EPERM` rather than pretending.
pub(crate) fn handle_prlimit64(args: [usize; crate::syscall::ARG_COUNT], from_user: bool) -> isize {
    let (pid, resource, new_ptr, old_ptr) = (args[0], args[1], args[2], args[3]);
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if old_ptr != 0 && !buffer_ok(old_ptr, 16, true, from_user) {
        return err::EFAULT;
    }
    let new = if new_ptr != 0 {
        if !buffer_ok(new_ptr, 16, false, from_user) {
            return err::EFAULT;
        }
        Some((
            unsafe { (new_ptr as *const u64).read_unaligned() },
            unsafe { ((new_ptr + 8) as *const u64).read_unaligned() },
        ))
    } else {
        None
    };

    #[cfg(target_os = "none")]
    let is_self = pid == 0 || pid as usize == crate::sched::current_tgid().0;
    // A host build has no processes, so every request is the current one.
    #[cfg(not(target_os = "none"))]
    let is_self = true;
    if !is_self {
        // Reading another process: it must exist.
        let table = rlimits_of_pid(pid as usize);
        let Some(table) = table else {
            return err::ESRCH;
        };
        if new.is_some() {
            return err::EPERM;
        }
        let r = table[resource];
        if old_ptr != 0 {
            unsafe {
                (old_ptr as *mut u64).write_unaligned(r.cur);
                ((old_ptr + 8) as *mut u64).write_unaligned(r.max);
            }
        }
        return 0;
    }
    if let Some((ncur, nmax)) = new {
        let code = set_current(resource, ncur, nmax);
        if code != 0 {
            return code;
        }
    }
    if old_ptr != 0 {
        let (cur, max) = current_pair(resource);
        unsafe {
            (old_ptr as *mut u64).write_unaligned(cur);
            ((old_ptr + 8) as *mut u64).write_unaligned(max);
        }
    }
    0
}

/// Look up another process's stored table (a host build has none).
fn rlimits_of_pid(pid: usize) -> Option<[Rlimit; RLIM_NLIMITS]> {
    #[cfg(target_os = "none")]
    {
        crate::sched::rlimits_of(pid)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = pid;
        None
    }
}

/// `getrlimit(resource, rlim)` — read one resource limit.
pub(crate) fn handle_getrlimit(args: [usize; crate::syscall::ARG_COUNT], from_user: bool) -> isize {
    rlimit_of(args[0], args[1], from_user)
}

/// `setrlimit(resource, rlim)` — set one resource limit on the current
/// process. Out-of-range is `EINVAL`, an unwritable pointer `EFAULT`, and a
/// change the caller's privilege will not allow is `EPERM`.
pub(crate) fn handle_setrlimit(args: [usize; crate::syscall::ARG_COUNT], from_user: bool) -> isize {
    let (resource, rlim) = (args[0], args[1]);
    if resource >= RLIM_NLIMITS {
        return err::EINVAL;
    }
    if rlim == 0 {
        return err::EFAULT;
    }
    if !buffer_ok(rlim, 16, false, from_user) {
        return err::EFAULT;
    }
    let cur = unsafe { (rlim as *const u64).read_unaligned() };
    let max = unsafe { ((rlim + 8) as *const u64).read_unaligned() };
    set_current(resource, cur, max)
}

// ---------------------------------------------------------------------------
// sched_setaffinity(2) / sched_getaffinity(2)
// ---------------------------------------------------------------------------

/// `sched_getaffinity(pid, len, mask)` — which CPUs the caller may run on.
///
/// One CPU, so the mask is always bit 0 and the answer is the CPU count
/// Linux returns: the byte count a caller scans for bits. Bytes beyond the
/// first word are zeroed, the way Linux clears what it does not fill.
pub(crate) fn handle_sched_getaffinity(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let (_pid, len, mask) = (args[0], args[1], args[2]);
    if len < core::mem::size_of::<u64>() {
        return err::EINVAL;
    }
    if !buffer_ok(mask, len, true, true) {
        return err::EFAULT;
    }
    unsafe {
        (mask as *mut u64).write_unaligned(1);
        if len > core::mem::size_of::<u64>() {
            core::ptr::write_bytes(
                (mask + core::mem::size_of::<u64>()) as *mut u8,
                0,
                len - core::mem::size_of::<u64>(),
            );
        }
    }
    core::mem::size_of::<u64>() as isize
}

/// `sched_setaffinity(pid, len, mask)` - pin the caller to the CPUs named in
/// `mask`. This machine has one, so the only acceptable request is one that
/// keeps it; anything else asks for a machine that is not here.
pub(crate) fn handle_sched_setaffinity(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let (_pid, len, mask) = (args[0], args[1], args[2]);
    if len < core::mem::size_of::<u64>() {
        return err::EINVAL;
    }
    if !buffer_ok(mask, len, true, true) {
        return err::EFAULT;
    }
    let requested = unsafe { (mask as *const u64).read_unaligned() };
    if requested & 1 == 0 {
        return err::EINVAL;
    }
    0
}

// ---------------------------------------------------------------------------
// The registration calls a libc's startup makes and this kernel files away
// ---------------------------------------------------------------------------

/// `set_robust_list(head, len)` — register the robust-futex list head.
///
/// The head is filed away so `get_robust_list` can read it back, and Linux pins
/// the size to `sizeof(struct robust_list_head)` = 24, rejecting anything else.
/// What is *not* yet done is walking the list to release held futexes when the
/// thread leaves - that is the feature `set` arms on Linux, and it is a named gap
/// (`PLAN.md` §12.2 B3); storing the head is what makes the registration +
/// `get` pair truthful rather than a `set` that claims a list it cannot return.
pub(crate) fn handle_set_robust_list(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let (head, len) = (args[0], args[1]);
    if len != 24 {
        return err::EINVAL;
    }
    crate::sched::set_robust_head(head);
    0
}

/// `get_robust_list(pid, head, len)` — the read half of the robust-futex list.
///
/// Answers the head `set_robust_list` registered (and the size Linux pins, 24).
/// `pid` of 0 or the caller's own tgid names the running process; threads are
/// not implemented, so any other pid has no list here and `ESRCH` is the honest
/// answer, the same way `prlimit64` treats a request for a process it will not
/// reach.
pub(crate) fn handle_get_robust_list(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let (pid, head_out, len_out) = (args[0] as i32, args[1], args[2]);
    #[cfg(target_os = "none")]
    let is_self = pid == 0 || pid as usize == crate::sched::current_tgid().0;
    // A host build has no processes, so every request is the current one.
    #[cfg(not(target_os = "none"))]
    let is_self = true;
    if !is_self {
        return err::ESRCH;
    }
    // Both output pointers must name somewhere to write. `buffer_ok` does not
    // refuse null on its own (the machine build's user-range check passes a
    // null, which is not a user address), so - like `PR_GET_NAME` - the null is
    // caught explicitly before anything is written through it.
    if head_out == 0
        || len_out == 0
        || !buffer_ok(head_out, 8, true, true)
        || !buffer_ok(len_out, 8, true, true)
    {
        return err::EFAULT;
    }
    let head = crate::sched::robust_head();
    unsafe {
        (head_out as *mut usize).write_unaligned(head);
        // `sizeof(struct robust_list_head)` - what Linux writes back as `len`.
        (len_out as *mut usize).write_unaligned(24);
    }
    0
}

/// `membarrier(2)`: the promise one thread makes to the others.
///
/// The command word is a set of bits with two halves that mean different
/// things. A `REGISTER_*` command is a *declaration* - the process says it will
/// use the matching barrier, which is what lets a kernel commit to the cost of
/// one up front - and the barrier commands are the barrier itself. `QUERY` is a
/// third thing again: it asks which of the rest this kernel supports, and is
/// the only command a caller may use before registering anything.
pub(super) const MEMBARRIER_CMD_QUERY: usize = 0;
pub(super) const MEMBARRIER_CMD_GLOBAL: usize = 1 << 0;
pub(super) const MEMBARRIER_CMD_GLOBAL_EXPEDITED: usize = 1 << 1;
pub(super) const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: usize = 1 << 2;
pub(super) const MEMBARRIER_CMD_PRIVATE_EXPEDITED: usize = 1 << 3;
pub(super) const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: usize = 1 << 4;

/// The one flag the `flags` word carries: "the `cpu_id` argument is a CPU to
/// act on, not padding".
pub(super) const MEMBARRIER_CMD_FLAG_CPU: usize = 1;

/// Which commands this kernel answers yes to.
///
/// `QUERY` reports this word and the handler below is driven by it, so what is
/// advertised and what is accepted cannot drift apart. That coupling matters
/// more here than in most places: a caller told "no" is *expected* to fall back
/// to a slower mechanism, while a caller told yes and given nothing has lost the
/// guarantee it asked for without being told.
///
/// Two of Linux's commands are deliberately absent. `..._SYNC_CORE` asks that
/// every thread of the process execute a *core serializing* instruction - the
/// promise a just-in-time compiler needs after writing code another thread may
/// already be running - and this kernel has no way to make an arbitrary thread
/// execute one. `..._RSEQ` asks for a barrier over per-thread `rseq` state this
/// kernel does not carry at all. Both are answers that would be plausible and
/// wrong, which is the kind this kernel has paid for before.
const MEMBARRIER_SUPPORTED: usize = MEMBARRIER_CMD_GLOBAL
    | MEMBARRIER_CMD_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
    | MEMBARRIER_CMD_PRIVATE_EXPEDITED
    | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

/// Decode the `flags` and `cpu_id` pair: which processor the call names, if any.
///
/// A `cpu_id` that arrives without the flag that makes it meaningful is a
/// caller bug, and Linux answers it with `EINVAL` rather than quietly ignoring
/// it - which is the right way round, because a caller that meant to name a
/// processor and silently got all of them has asked for something weaker than
/// it thinks.
fn membarrier_target(flags: usize, cpu_id: usize) -> Result<Option<usize>, isize> {
    match flags {
        0 => Ok(None),
        MEMBARRIER_CMD_FLAG_CPU => {
            // One processor, numbered zero. Any other number names something
            // that is not there, which is `EINVAL` on Linux as well.
            if cpu_id == 0 {
                Ok(Some(0))
            } else {
                Err(err::EINVAL)
            }
        }
        _ => Err(err::EINVAL),
    }
}

/// `membarrier(cmd, flags, cpu_id)` - "before this returns, the threads I name
/// have each executed a memory barrier".
///
/// The promise is about *other processors*, and this machine has one. While a
/// thread runs, no thread of the process is running anywhere else, so every
/// store any of them has made is already visible to the caller: x86 orders a
/// store before any later load from the same processor, and the scheduler's own
/// switch is a serializing event besides. So the barrier is a `fence`, which is
/// what the architecture asks for and all that is left for this call to do.
///
/// That reasoning is the whole implementation, and it is also the one thing
/// that stops being true if this kernel ever brings up a second processor: an
/// *expedited* barrier would then have to interrupt each of them and have it
/// execute the fence on the caller's behalf, and [`MEMBARRIER_SUPPORTED`] is
/// where that day would have to be answered for too.
///
/// The registration half is bookkeeping rather than machinery, and it is kept
/// honest rather than waved through: Linux answers an unregistered private or
/// global *expedited* barrier with `EPERM`, and a caller that finds out here has
/// a bug it can see instead of a guarantee it does not have.
pub(crate) fn handle_membarrier(args: [usize; crate::syscall::ARG_COUNT]) -> isize {
    let (command, flags, cpu_id) = (args[0], args[1], args[2]);

    if command == MEMBARRIER_CMD_QUERY {
        // QUERY names no processor even when the flag is set: it is a question
        // about this kernel, not an instruction to it.
        return if flags == 0 {
            MEMBARRIER_SUPPORTED as isize
        } else {
            err::EINVAL
        };
    }

    // One command, not a set of them, and one this kernel knows. A word with a
    // bit outside `SUPPORTED` is a command from a newer kernel; a word with two
    // of our own bits is two commands in one, which Linux refuses too.
    if command & !MEMBARRIER_SUPPORTED != 0 || !command.is_power_of_two() {
        return err::EINVAL;
    }
    let target = match membarrier_target(flags, cpu_id) {
        Ok(target) => target,
        Err(e) => return e,
    };
    // Only the global expedited barrier takes a processor to act on - the
    // others are about *this process's* threads, which are wherever they are.
    if target.is_some() && command != MEMBARRIER_CMD_GLOBAL_EXPEDITED {
        return err::EINVAL;
    }

    let registered = crate::sched::membarrier_registrations();
    match command {
        MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED => {
            crate::sched::register_membarrier(MEMBARRIER_CMD_GLOBAL_EXPEDITED);
            0
        }
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            crate::sched::register_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED);
            0
        }
        // The deprecated `GLOBAL` names no registration: it predates them, and
        // it was the only barrier there was, so every caller is assumed to want
        // it. Its modern spelling is the expedited one above, which does check.
        MEMBARRIER_CMD_GLOBAL => barrier(),
        MEMBARRIER_CMD_GLOBAL_EXPEDITED if registered & MEMBARRIER_CMD_GLOBAL_EXPEDITED != 0 => {
            barrier()
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED if registered & MEMBARRIER_CMD_PRIVATE_EXPEDITED != 0 => {
            barrier()
        }
        MEMBARRIER_CMD_GLOBAL_EXPEDITED | MEMBARRIER_CMD_PRIVATE_EXPEDITED => err::EPERM,
        _ => err::EINVAL,
    }
}

/// The barrier itself, and the reason the comment above is short: on one
/// processor there is nothing for it to do but order *this* thread's memory
/// accesses, which is what a sequentially consistent fence is.
fn barrier() -> isize {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    0
}

// ---------------------------------------------------------------------------
// The ENOSYS flood control: one line per missing call, not one per attempt
// ---------------------------------------------------------------------------

/// One bit per syscall number, set the first time the call answers ENOSYS.
/// Four words cover 0..=255, which is every number this kernel answers to;
/// anything above that is not a call it has ever been asked for, and
/// reporting it every time would be a log flood rather than a signal.
static MISSING_CALLS: [core::sync::atomic::AtomicU64; 4] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 4];

/// Record that `nr` answered ENOSYS, answering `true` the first time only.
///
/// Two ways to get here, and the log does not distinguish them because a
/// caller cannot: a number with no handler at all, and a handler that
/// refuses what it was asked. Both are a program being told this kernel will
/// not do that, which is worth saying once and not once per call: a libc in
/// a startup loop asks for the same thing dozens of times.
pub(crate) fn mark_missing(nr: u64) -> bool {
    use core::sync::atomic::Ordering;
    let Some(word) = MISSING_CALLS.get((nr / 64) as usize) else {
        return false;
    };
    let mask = 1u64 << (nr % 64);
    word.fetch_or(mask, Ordering::Relaxed) & mask == 0
}

/// One bit per syscall number, set the first time the call answers EOPNOTSUPP.
/// Its own table, because a number can legitimately answer both - ENOSYS for a
/// spelling this kernel has no handler for and EOPNOTSUPP for an option it
/// refuses - and one table shared between them would report whichever came
/// first and then go quiet about the other.
static UNSUPPORTED_CALLS: [core::sync::atomic::AtomicU64; 4] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 4];

/// Record that `nr` answered EOPNOTSUPP, answering `true` the first time only.
pub(crate) fn mark_unsupported(nr: u64) -> bool {
    use core::sync::atomic::Ordering;
    let Some(word) = UNSUPPORTED_CALLS.get((nr / 64) as usize) else {
        return false;
    };
    let mask = 1u64 << (nr % 64);
    word.fetch_or(mask, Ordering::Relaxed) & mask == 0
}

/// The same, for the *operations* of one call rather than calls.
///
/// A futex operation arrives inside a system call this kernel implements, so it
/// has no number of its own to be reported under. Two words cover 0..=127, which
/// is every operation the command's low seven bits can name; the flags above
/// them - `FUTEX_PRIVATE_FLAG`, `FUTEX_CLOCK_REALTIME` - are not part of what is
/// being asked for and are folded off before the report, so one operation is one
/// line however it was spelled.
static MISSING_FUTEX_OPS: [core::sync::atomic::AtomicU64; 2] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 2];

/// Record that futex operation `op` answered ENOSYS, true the first time only.
pub(crate) fn mark_missing_futex_op(op: usize) -> bool {
    use core::sync::atomic::Ordering;
    let op = op & 0x7f;
    let Some(word) = MISSING_FUTEX_OPS.get(op / 64) else {
        return false;
    };
    let mask = 1u64 << (op % 64);
    word.fetch_or(mask, Ordering::Relaxed) & mask == 0
}

// ---------------------------------------------------------------------------
// The OSS dsp family (`'P'`-typed requests), for `/dev/dsp`
// ---------------------------------------------------------------------------
//
// The request words were read off this machine's `linux/soundcard.h`, which
// spells them with the `_SIO`/`_SIOR`/`_SIOWR` macros - aliases of the
// standard `_IOC` layout (`dir << 30 | size << 16 | type << 8 | nr`) - so the
// magic byte the words carry is `'P'`, not the `'M'` a BSD port once used.
// A binary compiled against that header asks with exactly these numbers, and
// the whole point of the family is that the *writer* owns the parameters:
// rate, channels and format are negotiated here, not chosen at boot.

/// `_SIO('P', 0)`: drop whatever the stream holds.
pub const SNDCTL_DSP_RESET: u32 = 0x0000_5000;
/// `_SIO('P', 1)`: wait for held audio to play out.
pub const SNDCTL_DSP_SYNC: u32 = 0x0000_5001;
/// `_SIOWR('P', 2, int)`: get/set the sample rate, in Hz.
pub const SNDCTL_DSP_SPEED: u32 = 0xC004_5002;
/// `_SIOWR('P', 3, int)`: set 0 = mono, 1 = stereo; the answer is what was set.
pub const SNDCTL_DSP_STEREO: u32 = 0xC004_5003;
/// `_SIOWR('P', 4, int)`: the current fragment (block) size in bytes.
pub const SNDCTL_DSP_GETBLKSIZE: u32 = 0xC004_5004;
/// `_SIOWR('P', 5, int)`: select one `AFMT_*` format; the answer is the one
/// the device runs, or 0 for none of them.
pub const SNDCTL_DSP_SETFMT: u32 = 0xC004_5005;
/// `_SIOWR('P', 6, int)`: get/set the channel count.
pub const SNDCTL_DSP_CHANNELS: u32 = 0xC004_5006;
/// `_SIO('P', 8)`: kick a held output - a no-op where writes go straight out.
pub const SNDCTL_DSP_POST: u32 = 0x0000_5008;
/// `_SIOWR('P', 9, int)`: subdivide the fragments. Unimplemented: EINVAL.
pub const SNDCTL_DSP_SUBDIVIDE: u32 = 0xC004_5009;
/// `_SIOWR('P', 10, int)`: `fragsize | fragcount << 16`. Unimplemented: EINVAL.
pub const SNDCTL_DSP_SETFRAGMENT: u32 = 0xC004_500A;
/// `_SIOR('P', 11, int)`: the mask of formats this device speaks.
pub const SNDCTL_DSP_GETFMTS: u32 = 0x8004_500B;
/// `_SIOR('P', 12, audio_buf_info)`: free space on the output side.
pub const SNDCTL_DSP_GETOSPACE: u32 = 0x8010_500C;
/// `_SIOR('P', 13, audio_buf_info)`: free space on the input side - which
/// this device's stream does not have, and says so with an empty one.
pub const SNDCTL_DSP_GETISPACE: u32 = 0x8010_500D;
/// `_SIO('P', 14)`: turn non-blocking writes on. Unimplemented: EINVAL -
/// the write here blocks *inside* the device hand-off, and no flag on the
/// descriptor changes that; accepting the word while writes went on blocking
/// would be the false answer this kernel's rules forbid.
pub const SNDCTL_DSP_NONBLOCK: u32 = 0x0000_500E;

/// `AFMT_S16_LE`, the one format this device's writers are promised.
pub const AFMT_S16_LE: u32 = 0x0000_0010;

/// The size of `struct audio_buf_info`: four host-order `int`s - fragments,
/// fragstotal, fragsize, bytes.
pub const AUDIO_BUF_INFO_SIZE: usize = 16;

/// The rate to hand a device that was asked for `requested` Hz and supports
/// the listed ones - the rounding OSS does rather than refusing, and the
/// caller is answered with what was *settled on*, which is the same contract.
/// `None` when the device offers nothing to round to.
pub fn oss_round_rate(requested: u32, supported: &[u32]) -> Option<u32> {
    /// Relative distance, in thousandths: 8 kHz away from 44.1 kHz and 48 kHz
    /// away from 44.1 kHz are not the same complaint, and a linear-absolute
    /// minimum would always answer a 44100 request with 48000 - the rounding
    /// the measured device does not make.
    fn rel(a: u32, b: u32) -> u64 {
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        if lo == 0 {
            return u64::MAX;
        }
        (hi - lo) as u64 * 1000 / lo as u64
    }
    supported
        .iter()
        .min_by_key(|rate| rel(**rate, requested))
        .copied()
}

// ---------------------------------------------------------------------------
// Tests: every item above, against the man page that specifies it
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::NodeKind;

    fn meta(kind: NodeKind, ino: u64, size: u64) -> Metadata {
        Metadata { ino, kind, size }
    }

    /// `path_resolution(7)`: `.` is silent, `..` pops one component,
    /// repeated slashes collapse, and a result of nothing is `/`.
    #[test]
    fn normalize_path_resolves_dot_components_per_path_resolution() {
        assert_eq!(normalize_path("etc/motd", "/"), "/etc/motd");
        assert_eq!(normalize_path("./etc/motd", "/initrd"), "/initrd/etc/motd");
        assert_eq!(
            normalize_path("../etc/motd", "/initrd/mnt"),
            "/initrd/etc/motd"
        );
        // `..` above the base pops what is there, one component at a time.
        assert_eq!(normalize_path("../../..", "/a/b/c"), "/");
        // Repeated and trailing slashes collapse; an empty path is the root.
        assert_eq!(normalize_path("a//b///c", "/"), "/a/b/c");
        assert_eq!(normalize_path("", "/"), "/");
        assert_eq!(normalize_path(".", "/"), "/");
        assert_eq!(normalize_path("/", "/initrd"), "/");
        // Absolute paths ignore the base entirely.
        assert_eq!(
            normalize_path("/etc/passwd", "/somewhere/else"),
            "/etc/passwd"
        );
        // A base with a trailing slash does not gain a second one.
        assert_eq!(normalize_path("etc", "/initrd/"), "/initrd/etc");
    }

    /// `stat(2)`/`inode(7)`: the `S_IFMT` field is a type tag, the low
    /// twelve bits are permissions from the filesystem when it has them and
    /// the kind's default when it does not.
    #[test]
    fn mode_of_tags_the_kind_and_honours_stored_permissions() {
        // Format bits per inode(7): S_IFREG, S_IFDIR, S_IFCHR, S_IFBLK, S_IFLNK.
        assert_eq!(mode_of(&meta(NodeKind::File, 1, 0), 0), 0o100_644);
        assert_eq!(mode_of(&meta(NodeKind::Dir, 1, 0), 0), 0o040_755);
        assert_eq!(mode_of(&meta(NodeKind::CharDev, 1, 0), 0), 0o020_666);
        assert_eq!(mode_of(&meta(NodeKind::BlockDev, 1, 0), 0), 0o060_660);
        assert_eq!(mode_of(&meta(NodeKind::Symlink, 1, 0), 0), 0o120_777);

        // Stored permission bits replace the defaults, setuid included. In
        // the stat(2) spelling a setuid regular file is `104755`: the type
        // nibble stays `10` and the stored low byte keeps its setuid bit.
        assert_eq!(mode_of(&meta(NodeKind::File, 1, 0), 0o755), 0o100_755);
        assert_eq!(mode_of(&meta(NodeKind::File, 1, 0), 0o4755), 0o104_755);
        // The format field of a stored mode does not leak through: only the
        // permission nibble is kept.
        assert_eq!(mode_of(&meta(NodeKind::Dir, 1, 0), 0o100_700), 0o040_700);
    }

    /// `stat(2)`: the x86_64 `struct stat` field offsets - st_dev at 0,
    /// st_ino at 8, st_nlink at 16, st_mode at 24, st_rdev at 40, st_size
    /// at 48, st_blksize at 56, st_blocks at 64 - and 144 bytes in all.
    #[test]
    fn write_stat_lays_out_struct_stat_at_the_x86_64_offsets() {
        let mut buf = [0u8; 144];
        let written = write_stat(
            buf.as_mut_ptr() as usize,
            &meta(NodeKind::File, 0x1234, 257),
            0x8,          // st_dev: a filesystem id
            Some((4, 3)), // st_rdev: /dev/ttyS-style 4:3? no - a char dev
            0o640,
            // A hard-linked file: three names reach the inode, and the
            // filesystem kept three distinct clocks for it.
            3,
            // And an owner, so the test can say where st_uid and st_gid land
            // rather than trusting two fields nobody had written yet.
            (1000, 100),
            StatTimes {
                atime: 1_700_000_400,
                mtime: 1_700_000_123,
                ctime: 1_700_000_200,
            },
        );
        assert_eq!(written, 0);
        let word = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        assert_eq!(word(0), 0x8); // st_dev
        assert_eq!(word(8), 0x1234); // st_ino
        assert_eq!(word(16), 3); // st_nlink: the count the filesystem reported
        let dword = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
        assert_eq!(dword(28), 1000); // st_uid
        assert_eq!(dword(32), 100); // st_gid
        assert_eq!(
            u32::from_le_bytes(buf[24..28].try_into().unwrap()),
            0o100_640
        ); // st_mode
        assert_eq!(word(40), (4u64 << 8) | 3); // st_rdev, makedev-packed
        assert_eq!(word(48), 257); // st_size
        assert_eq!(word(56), 512); // st_blksize
        assert_eq!(word(64), 1); // st_blocks: ceil(257/512)
        // Each clock carries its own value - not one collapsed into three -
        // and the nanosecond halves stay zero.
        assert_eq!(word(72), 1_700_000_400); // st_atime
        assert_eq!(word(80), 1_700_000_123); // st_mtime
        assert_eq!(word(88), 1_700_000_200); // st_ctime
        assert_eq!(&buf[76..80], &[0u8; 4]);
        assert_eq!(&buf[84..88], &[0u8; 4]);
        assert_eq!(&buf[92..96], &[0u8; 4]);

        // An untracked count (a filesystem that stores no `i_links_count`)
        // falls back to the kind's convention: two names for a directory, the
        // one it was created under for a file.
        let mut dbuf = [0u8; 144];
        assert_eq!(
            write_stat(
                dbuf.as_mut_ptr() as usize,
                &meta(NodeKind::Dir, 9, 4096),
                0,
                None,
                0,
                0,      // nlink untracked
                (0, 0), // and no owner: root's, which is the default
                StatTimes {
                    atime: 0,
                    mtime: 1_700_000_123,
                    ctime: 0
                },
            ),
            0
        );
        assert_eq!(u64::from_le_bytes(dbuf[16..24].try_into().unwrap()), 2);
        // Zero permissions stored fall back to the kind's defaults, so a
        // directory statts traversable.
        assert_eq!(
            u32::from_le_bytes(dbuf[24..28].try_into().unwrap()),
            0o040_755
        );
    }

    /// `statfs(2)`: `f_type` carries the superblock magic the filesystem
    /// reports on Linux - the field `switch_root` branches on - and the
    /// counters sit one word apart from `f_bsize` on.
    #[test]
    fn write_statfs_reports_the_superblock_magic_per_filesystem() {
        assert_eq!(fstype_magic("ext2"), 0x0000EF53);
        assert_eq!(fstype_magic("ext3"), 0x0000EF53);
        assert_eq!(fstype_magic("ext4"), 0x0000EF53);
        assert_eq!(fstype_magic("proc"), 0x00009FA0); // PROC_SUPER_MAGIC
        assert_eq!(fstype_magic("sysfs"), 0x62656572); // SYSFS_MAGIC
        assert_eq!(fstype_magic("ramfs"), 0x858458F6); // RAMFS_MAGIC
        assert_eq!(fstype_magic("tmpfs"), 0x01021994); // TMPFS_MAGIC
        assert_eq!(fstype_magic("devfs"), 0x01021994);
        assert_eq!(fstype_magic("devtmpfs"), 0x01021994);
        assert_eq!(fstype_magic("iso9660"), 0); // not spoken: absent

        let mut buf = [0u8; 120];
        assert_eq!(
            write_statfs(buf.as_mut_ptr() as usize, 0x0000EF53, 4096, 100, 60, 7, 5),
            0
        );
        let word = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        assert_eq!(word(0), 0xEF53); // f_type
        assert_eq!(word(8), 4096); // f_bsize
        assert_eq!(word(16), 100); // f_blocks
        assert_eq!(word(24), 60); // f_bfree
        assert_eq!(word(32), 60); // f_bavail: no root-reserved pool, same as bfree
        assert_eq!(word(40), 7); // f_files
        assert_eq!(word(48), 5); // f_ffree
        assert_eq!(word(64), 255); // f_namelen
        // f_fsid and f_frsize stay zero; a zero frsize reads as bsize.
        assert_eq!(word(56), 0);
        assert!(buf[72..].iter().all(|&b| b == 0));
    }

    /// `statx(2)`: the fields uutils reads - mask, nlink, mode, ino, size,
    /// blocks, rdev - at their x86_64 offsets, with unasked fields absent.
    #[test]
    fn write_statx_reports_only_the_basic_stats_the_mask_asked_for() {
        let mut buf = [0u8; 152];
        assert_eq!(
            write_statx(
                buf.as_mut_ptr() as usize,
                0x7ff,
                &meta(NodeKind::File, 77, 1024),
                0,
                Some((1, 9)),
                // A tracked count and three distinct clocks, as ext2 reports.
                5,
                (0, 0),
                StatTimes {
                    atime: 1_700_000_400,
                    mtime: 1_700_000_123,
                    ctime: 1_700_000_200,
                },
            ),
            0
        );
        let word = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 0x7ff); // stx_mask
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 512); // stx_blksize
        assert_eq!(u32::from_le_bytes(buf[16..20].try_into().unwrap()), 5); // stx_nlink
        assert_eq!(
            u16::from_le_bytes(buf[28..30].try_into().unwrap()),
            0o100_644
        ); // stx_mode
        assert_eq!(word(32), 77); // stx_ino
        assert_eq!(word(40), 1024); // stx_size
        assert_eq!(word(48), 2); // stx_blocks: ceil(1024/512)
        assert_eq!(u32::from_le_bytes(buf[72..76].try_into().unwrap()), 1); // stx_rdev_major
        assert_eq!(u32::from_le_bytes(buf[76..80].try_into().unwrap()), 9); // stx_rdev_minor
        // Each clock carries its own seconds value, zero in the nanosecond
        // half; the mount id beyond them stays absent.
        assert_eq!(word(96), 1_700_000_400); // stx_atime
        assert_eq!(word(112), 1_700_000_123); // stx_mtime
        assert_eq!(word(128), 1_700_000_200); // stx_ctime
        assert!(buf[144..].iter().all(|&b| b == 0));

        // A mask wider than the basic stats is trimmed: only what is filled
        // is reported, so a caller does not read absent fields as present.
        let mut buf2 = [0u8; 152];
        assert_eq!(
            write_statx(
                buf2.as_mut_ptr() as usize,
                0xfff,
                &meta(NodeKind::Dir, 1, 0),
                0,
                None,
                0, // untracked -> the directory's two names
                (0, 0),
                StatTimes::default(),
            ),
            0
        );
        assert_eq!(u32::from_le_bytes(buf2[0..4].try_into().unwrap()), 0x7ff);
        assert_eq!(u32::from_le_bytes(buf2[16..20].try_into().unwrap()), 2);
    }

    /// `getdents64(2)`: `d_type` in the `DT_*` numbering, `d_reclen` the
    /// 8-aligned span of header plus name plus NUL.
    #[test]
    fn getdents_records_match_the_linux_directory_format() {
        assert_eq!(dt_type_of(NodeKind::File), 8); // DT_REG
        assert_eq!(dt_type_of(NodeKind::Dir), 4); // DT_DIR
        assert_eq!(dt_type_of(NodeKind::CharDev), 2); // DT_CHR
        assert_eq!(dt_type_of(NodeKind::BlockDev), 6); // DT_BLK
        assert_eq!(dt_type_of(NodeKind::Symlink), 10); // DT_LNK

        // Header (19) + name + NUL, padded to 8: "foo" spans 24 bytes.
        assert_eq!(reclen_of(3), 24);
        // A name that exactly fills an 8-multiple must not gain padding.
        assert_eq!(reclen_of(4), 24);
        assert_eq!(reclen_of(5), 32);
        // The one-byte name: 19 + 1 + 1 -> 24.
        assert_eq!(reclen_of(1), 24);
    }

    /// `lseek(2)`: SEEK_SET/CUR/END name the position three ways, an
    /// unknown whence and a position before byte 0 are `EINVAL`.
    #[test]
    fn lseek_whence_arithmetic_refuses_out_of_range_results() {
        assert_eq!(lseek_target(100, 500, 10, 0), Some(10)); // SEEK_SET
        assert_eq!(lseek_target(100, 500, -40, 1), Some(60)); // SEEK_CUR
        assert_eq!(lseek_target(100, 500, -40, 2), Some(460)); // SEEK_END
        // Before byte 0: refused, not wrapped.
        assert_eq!(lseek_target(100, 500, -101, 1), None);
        assert_eq!(lseek_target(100, 500, -501, 2), None);
        // The only three whence values exist.
        assert_eq!(lseek_target(100, 500, 0, 3), None);
        assert_eq!(lseek_target(100, 500, 0, -1), None);
    }

    /// The socket vocabulary, pinned against `<linux/stat.h>` and
    /// `<dirent.h>`: `st_mode`'s type tag, its default permission when the
    /// filesystem stored none, and the directory-entry type that lets
    /// `ls -F` mark the name. `mknod` still refuses S_IFSOCK on purpose -
    /// only `bind(2)` plants a socket node - and that refusal is pinned
    /// beside this test.
    #[test]
    fn a_socket_reports_its_kind_in_every_door() {
        use crate::vfs::{Metadata, NodeKind};
        let meta = Metadata {
            ino: 7,
            kind: NodeKind::Socket,
            size: 0,
        };
        assert_eq!(mode_of(&meta, 0) & 0o170_000, 0o140_000, "S_IFSOCK");
        assert_eq!(mode_of(&meta, 0), 0o140_755, "srwxr-xr-x by default");
        assert_eq!(
            mode_of(&meta, 0o700),
            0o140_700,
            "a stored mode keeps the tag"
        );
        assert_eq!(dt_type_of(NodeKind::Socket), 12, "DT_SOCK");
    }

    /// `mknod(2)`: the `S_IFMT` field picks the kind; a mode with no type
    /// is not a node anybody asked for.
    #[test]
    fn mknod_mode_field_picks_the_node_kind() {
        assert_eq!(mknod_kind(0o020_666), Ok(NodeKind::CharDev)); // S_IFCHR
        assert_eq!(mknod_kind(0o060_660), Ok(NodeKind::BlockDev)); // S_IFBLK
        assert_eq!(mknod_kind(0o010_644), Ok(NodeKind::File)); // S_IFIFO
        // S_IFREG, S_IFDIR, S_IFLNK and a plain permission word name nothing
        // `mknod` makes: sockets have no filesystem presence here, and
        // directories have their own call.
        assert!(mknod_kind(0o100_644).is_err());
        assert!(mknod_kind(0o040_755).is_err());
        assert!(mknod_kind(0o120_777).is_err());
        assert!(mknod_kind(0o644).is_err());
    }

    /// `wait(2)`: a normal exit puts its code in bits 15:8 (WEXITSTATUS),
    /// a death by signal puts the signal in bits 6:0 (WTERMSIG).
    #[test]
    fn wait_status_encodes_exits_the_wmacros_decode_them() {
        // Exit code 3: WIFEXITED true, WEXITSTATUS 3.
        let s = wait_status(3);
        assert_eq!(s, 3 << 8);
        assert_eq!(s & 0x7f, 0, "WIFEXITED requires the low 7 bits clear");
        assert_eq!((s >> 8) & 0xff, 3);

        // Killed by SIGKILL (mpos records it as -9): WIFSIGNALED, WTERMSIG 9.
        let s = wait_status(-9);
        assert_eq!(s & 0x7f, 9);
        assert_ne!(s & 0x7f, 0);

        // Exit codes carry only their low byte, as WEXITSTATUS reads them.
        assert_eq!(wait_status(256), 0);
        assert_eq!(wait_status(257), 1 << 8);

        // No core-dump bit is ever set: mpos writes no core files.
        for code in [-1isize, -9, 0, 7, 255] {
            assert_eq!(
                wait_status(code) & 0x80,
                0,
                "WCOREDUMP must stay clear for {code}"
            );
        }
    }

    /// `waitid(2)`: the siginfo_t fields `si_signo`, `si_code`, `si_pid`
    /// and `si_status` at their struct offsets, with the CLD_* code
    /// distinguishing an exit from a kill.
    #[test]
    fn waitid_info_reports_the_child_the_siginfo_way() {
        let mut buf = [0u8; 128];
        assert_eq!(write_waitid_info(buf.as_mut_ptr() as usize, 42, 7), 0);
        let word = |at: usize| i32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
        assert_eq!(word(0), 17); // si_signo: SIGCHLD
        assert_eq!(word(8), 1); // si_code: CLD_EXITED
        assert_eq!(word(16), 42); // si_pid
        assert_eq!(word(24), 7); // si_status: the exit code

        // Killed: the status is the signal number, the code says so.
        let mut buf2 = [0u8; 128];
        assert_eq!(write_waitid_info(buf2.as_mut_ptr() as usize, 5, -15), 0);
        let word2 = |at: usize| i32::from_le_bytes(buf2[at..at + 4].try_into().unwrap());
        assert_eq!(word2(8), 2); // CLD_KILLED
        assert_eq!(word2(24), 15);
        // Everything else is zeroed, so no stale caller byte survives.
        assert!(buf2[28..].iter().all(|&b| b == 0));
    }

    /// `clone(2)`: the flag word's low byte is an exit signal, unknown bits
    /// are refused, the THREAD/SIGHAND/VM chain is enforced, and a tid flag
    /// without a tid pointer is refused.
    #[test]
    fn clone_flags_are_validated_in_linuxs_order() {
        use crate::syscall::linux::nr as n;

        // Plain fork: zero flags, no exit signal.
        assert_eq!(validate_clone_flags(0, 0, 0), Ok((0, 0)));

        // posix_spawn's word: SIGCHLD (17) in the low byte rides along with
        // the flags, and the rest of the word is what gets checked.
        let word = n::CLONE_VM | n::CLONE_VFORK | 17;
        assert_eq!(
            validate_clone_flags(word, 0, 0),
            Ok((17, n::CLONE_VM | n::CLONE_VFORK))
        );

        // A signal byte past the real-time signals is refused.
        assert_eq!(
            validate_clone_flags(64, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );

        // An unknown bit - CLONE_NEWNS (0x20000) among them - is refused
        // before anything is created.
        assert_eq!(
            validate_clone_flags(n::CLONE_VM | 0x2_0000, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );

        // The dependency chain: THREAD implies SIGHAND implies VM.
        assert_eq!(
            validate_clone_flags(n::CLONE_THREAD, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_THREAD | n::CLONE_SIGHAND, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_THREAD | n::CLONE_VM, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_THREAD | n::CLONE_SIGHAND | n::CLONE_VM, 0, 0),
            Ok((0, n::CLONE_THREAD | n::CLONE_SIGHAND | n::CLONE_VM))
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_SIGHAND, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );

        // A tid flag without a place to put the tid is refused.
        assert_eq!(
            validate_clone_flags(n::CLONE_PARENT_SETTID, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_CHILD_SETTID, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_CHILD_CLEARTID, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_PARENT_SETTID, 0x1000, 0),
            Ok((0, n::CLONE_PARENT_SETTID))
        );
        assert_eq!(
            validate_clone_flags(n::CLONE_CHILD_SETTID, 0, 0x1000),
            Ok((0, n::CLONE_CHILD_SETTID))
        );

        // The historical no-ops musl still sends are accepted, not refused.
        assert_eq!(
            validate_clone_flags(n::CLONE_DETACHED | n::CLONE_UNTRACED | n::CLONE_FILES, 0, 0),
            Ok((0, n::CLONE_DETACHED | n::CLONE_UNTRACED | n::CLONE_FILES))
        );
    }

    /// `reboot(2)`: the five magic words, and nothing else.
    #[test]
    fn reboot_commands_are_the_five_magic_words() {
        // The values are the ABI - not flag bits, each its own sentence.
        assert_eq!(RB_DISABLE_CAD, 0x0000_0000);
        assert_eq!(RB_ENABLE_CAD, 0x89ab_cdef);
        assert_eq!(RB_AUTOBOOT, 0x0123_4567);
        assert_eq!(RB_HALT_SYSTEM, 0xcdef_0123);
        assert_eq!(RB_POWER_OFF, 0x4321_fedc);

        assert_eq!(reboot_classify(RB_DISABLE_CAD), RebootCmd::Policy);
        assert_eq!(reboot_classify(RB_ENABLE_CAD), RebootCmd::Policy);
        assert_eq!(reboot_classify(RB_AUTOBOOT), RebootCmd::Stop);
        assert_eq!(reboot_classify(RB_HALT_SYSTEM), RebootCmd::Stop);
        assert_eq!(reboot_classify(RB_POWER_OFF), RebootCmd::Stop);
        assert_eq!(reboot_classify(0xdead_beef), RebootCmd::Unknown);
    }

    /// `reboot(2)` takes `magic, magic2, cmd, arg`, so the command word is the
    /// **third** argument. `poweroff -f` sends the magics musl wraps around the
    /// command, and a handler that classified `args[0]` read `LINUX_REBOOT_MAGIC1`
    /// instead - answered `EINVAL`, printed "Invalid argument", and left the
    /// machine (and every test run after it) hanging.
    #[test]
    fn reboot_reads_its_command_from_the_third_argument() {
        let call = |magic: usize, magic2: usize, cmd: usize| [magic, magic2, cmd, 0, 0, 0];

        assert_eq!(
            reboot_command(call(RB_MAGIC1, RB_MAGIC2, RB_POWER_OFF)),
            Ok(RebootCmd::Stop)
        );
        assert_eq!(
            reboot_command(call(RB_MAGIC1, RB_MAGIC2, RB_HALT_SYSTEM)),
            Ok(RebootCmd::Stop)
        );
        assert_eq!(
            reboot_command(call(RB_MAGIC1, RB_MAGIC2, RB_ENABLE_CAD)),
            Ok(RebootCmd::Policy)
        );
        // The magic words come first: a call that does not name them is
        // refused before its command is looked at, which is how the shape
        // above gets noticed instead of silently classifying the wrong word.
        assert_eq!(
            reboot_command(call(RB_POWER_OFF, RB_MAGIC2, RB_POWER_OFF)),
            Err(err::EINVAL)
        );
        assert_eq!(
            reboot_command(call(RB_MAGIC1, 0, RB_POWER_OFF)),
            Err(err::EINVAL)
        );
        // A magic-named call asking for something no kernel knows.
        assert_eq!(
            reboot_command(call(RB_MAGIC1, RB_MAGIC2, 0xdead_beef)),
            Err(err::EINVAL)
        );
        // The two older `magic2` values Linux still accepts.
        assert_eq!(
            reboot_command(call(RB_MAGIC1, 0x2011_2000, RB_AUTOBOOT)),
            Ok(RebootCmd::Stop)
        );
        assert_eq!(
            reboot_command(call(RB_MAGIC1, 0x0511_1996, RB_AUTOBOOT)),
            Ok(RebootCmd::Stop)
        );
    }

    /// `nanosleep(2)`: `tv_nsec` below a billion, sub-second truncation.
    #[test]
    fn nanosleep_validates_nsec_and_truncates_the_subsecond() {
        assert_eq!(nanosleep_ms(0, 0), Ok(0));
        assert_eq!(nanosleep_ms(2, 0), Ok(2000));
        // 999_999 ns is no whole millisecond: the request truncates.
        assert_eq!(nanosleep_ms(1, 999_999), Ok(1000));
        assert_eq!(nanosleep_ms(1, 1_500_000), Ok(1001));
        // The one field the man page validates.
        assert_eq!(
            nanosleep_ms(0, 1_000_000_000),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            nanosleep_ms(u64::MAX, 999_999_999),
            Ok(u64::MAX),
            "saturates, never wraps"
        );
    }

    /// `ppoll(2)`: like `nanosleep`'s validation, but the sub-second field
    /// rounds up - a timeout is a lower bound on the wait.
    #[test]
    fn ppoll_validates_nsec_and_rounds_the_subsecond_up() {
        assert_eq!(ppoll_ms(0, 0), Ok(0));
        assert_eq!(ppoll_ms(1, 500_000_000), Ok(1500));
        // One nanosecond is still a wait: div_ceil, not truncation.
        assert_eq!(ppoll_ms(1, 1), Ok(1001));
        assert_eq!(ppoll_ms(0, 999_999_999), Ok(1000));
        assert_eq!(ppoll_ms(1, 1_000_000_000), Err(crate::syscall::err::EINVAL));
        assert_eq!(
            ppoll_ms(u64::MAX, 999_999_999),
            Ok(u64::MAX),
            "saturates, never wraps"
        );
    }

    /// `timer_settime(2)`: a `struct timespec` to ticks at the machine's
    /// 250 Hz, validated like Linux's and rounded up - an armed timer must
    /// never fire before the duration its caller named has passed.
    #[test]
    fn a_timer_timespec_validates_and_rounds_up_to_whole_ticks() {
        // The PIT's rate, spelled here rather than read from `arch` because
        // this module is host-compiled and `arch` is not; the machine-side
        // callers pass `arch::timer::TICK_HZ`, which is this number.
        const HZ: u64 = 250;
        // Zero is the disarm spelling, not an error - the caller decides.
        assert_eq!(timespec_ticks(0, 0, HZ), Ok(0));
        // A whole second is exactly HZ ticks.
        assert_eq!(timespec_ticks(2, 0, HZ), Ok(2 * HZ));
        // 50 ms at 250 Hz is 12.5 ticks: the answer is 13, never 12, because
        // 12 ticks would fire half a millisecond early. vim arms 50 ms
        // timeouts and a timer that fires early is one that never times out.
        assert_eq!(timespec_ticks(0, 50_000_000, HZ), Ok(13));
        // 4 ms is exactly one tick; 4 ms minus one nanosecond is still two -
        // rounding up is the whole point.
        assert_eq!(timespec_ticks(0, 4_000_000, HZ), Ok(1));
        assert_eq!(timespec_ticks(0, 3_999_999, HZ), Ok(1));
        // The man page's EINVALs: a negative second, a nanosecond at or past
        // a billion, a negative nanosecond.
        assert_eq!(timespec_ticks(-1, 0, HZ), Err(crate::syscall::err::EINVAL));
        assert_eq!(
            timespec_ticks(0, 1_000_000_000, HZ),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(timespec_ticks(0, -1, HZ), Err(crate::syscall::err::EINVAL));
        // A second count large enough to overflow saturates.
        assert_eq!(timespec_ticks(i64::MAX, 999_999_999, HZ), Ok(u64::MAX));
    }

    /// `select(2)`/`pselect6(2)`: the sub-second field is validated against
    /// its own unit, and a clock that has gone wrong is refused rather than
    /// wrapped into the past.
    #[test]
    fn select_timeouts_are_validated_against_their_own_unit() {
        // pselect6 hands a timespec: nanoseconds.
        assert_eq!(select_timeout_ms(2, 999_999_999, true), Ok(3000));
        assert_eq!(select_timeout_ms(0, 1, true), Ok(1));
        assert_eq!(
            select_timeout_ms(1, 1_000_000_000, true),
            Err(crate::syscall::err::EINVAL)
        );
        // select hands a timeval: microseconds, and the ceiling is one
        // second's worth of them - 999 µs folds into a millisecond, a
        // million is the first refusal.
        assert_eq!(select_timeout_ms(1, 500, false), Ok(1001));
        assert_eq!(
            select_timeout_ms(1, 1_000_000, false),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(select_timeout_ms(1, 999_999, false), Ok(2000));
        // Both refuse a timeout further out than a few centuries.
        assert_eq!(
            select_timeout_ms(1 << 43, 0, true),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            select_timeout_ms((1 << 43) - 1, 0, false),
            Ok(((1 << 43) - 1) * 1000)
        );
    }

    /// `setitimer(2)`: each `tv_usec` must stay below a second's worth; a
    /// zero `it_value` is the disarm spelling, not an error.
    #[test]
    fn itimerval_fields_are_validated_per_field() {
        assert_eq!(itimerval_ms(1, 500_000, 0, 0), Ok((1500, 0)));
        assert_eq!(itimerval_ms(0, 999_999, 2, 1), Ok((1000, 2001)));
        assert_eq!(
            itimerval_ms(0, 1_000_000, 0, 0),
            Err(crate::syscall::err::EINVAL)
        );
        assert_eq!(
            itimerval_ms(0, 0, 0, 1_000_000),
            Err(crate::syscall::err::EINVAL)
        );
        // A zero timer is how a caller disarms: a valid (0, 0) answer.
        assert_eq!(itimerval_ms(0, 0, 0, 0), Ok((0, 0)));
    }

    /// `prctl(2)`: the name pair round-trips through the `comm` slot - a name
    /// `PR_SET_NAME` stores is what `PR_GET_NAME` hands back; a null faults for
    /// both, an unterminated name is refused, and an unmodelled option is still
    /// accepted rather than refused.
    #[test]
    fn prctl_stores_and_reports_the_process_name() {
        const PR_SET_NAME: usize = 15;
        const PR_GET_NAME: usize = 16;

        // A terminated name copies in and reads back verbatim, NUL-padded.
        let mut src = [0u8; 16];
        src[..6].copy_from_slice(b"worker");
        assert_eq!(
            handle_prctl([PR_SET_NAME, src.as_ptr() as usize, 0, 0, 0, 0]),
            0
        );

        let mut buf = [0xABu8; 16];
        assert_eq!(
            handle_prctl([PR_GET_NAME, buf.as_mut_ptr() as usize, 0, 0, 0, 0]),
            0
        );
        assert_eq!(&buf[..6], b"worker", "the name round-trips");
        assert_eq!(buf[6], 0, "the slot past the name is the terminator");

        // A null buffer faults in either direction.
        assert_eq!(handle_prctl([PR_GET_NAME, 0, 0, 0, 0, 0]), err::EFAULT);
        assert_eq!(handle_prctl([PR_SET_NAME, 0, 0, 0, 0, 0]), err::EFAULT);

        // A name with no NUL inside the sixteen-byte slot is *truncated and
        // accepted*, not refused: measured on Linux, sixteen `'a'` with a non-NUL
        // seventeenth answers `0` and `PR_GET_NAME` then reads back fifteen
        // characters. The arm used to answer `EFAULT`, which sends a caller that
        // named itself sixteen bytes long down a fallback it would not take on
        // Linux.
        let long = [b'x'; 17];
        assert_eq!(
            handle_prctl([PR_SET_NAME, long.as_ptr() as usize, 0, 0, 0, 0]),
            0,
            "an unterminated-in-slot name is accepted"
        );
        let mut buf = [0xABu8; 16];
        assert_eq!(
            handle_prctl([PR_GET_NAME, buf.as_mut_ptr() as usize, 0, 0, 0, 0]),
            0
        );
        assert_eq!(
            &buf[..15],
            &[b'x'; 15],
            "fifteen characters survive the cut"
        );
        assert_eq!(buf[15], 0, "and the slot's last byte is the terminator");

        // An option this kernel does not model is refused with `EINVAL`, the
        // way Linux's `prctl` default arm answers a number it does not know -
        // never the old accepted-and-dropped `0` that claimed a thing it did not do.
        assert_eq!(
            handle_prctl([0x1337, 0, 0, 0, 0, 0]),
            crate::syscall::err::EINVAL
        );
    }

    /// The `PR_SET_SECCOMP` / `PR_GET_SECCOMP` pair latches strict mode the
    /// same one-way way the `seccomp(2)` syscall does, and refuses a filter
    /// mode asked for with no program to install.
    #[test]
    fn prctl_seccomp_latches_strict_and_refuses_a_filter_with_no_program() {
        const PR_GET_SECCOMP: usize = 21;
        const PR_SET_SECCOMP: usize = 22;
        const STRICT: usize = 1;
        const FILTER: usize = 2;

        // Asking to enter the filter mode with no program to install is refused
        // rather than ignored: a caller that believes it installed a BPF program
        // must not be left thinking it is confined. What it is refused *with*
        // is a bad-pointer error rather than "unsupported" - the mode exists now,
        // and the interpreter behind it is argued with in `seccomp`'s tests.
        //
        // A real install is deliberately not done here: the host harness keeps
        // one filter list for the whole test binary, so a filter installed by
        // this test would change the answer every later test in the binary reads
        // back from `PR_GET_SECCOMP`.
        assert_eq!(
            handle_prctl([PR_SET_SECCOMP, FILTER, 0, 0, 0, 0]),
            crate::syscall::err::EFAULT
        );

        // Strict mode takes, the read-back says so, and it stays said - the
        // latch has no off switch.
        assert_eq!(handle_prctl([PR_SET_SECCOMP, STRICT, 0, 0, 0, 0]), 0);
        assert_eq!(handle_prctl([PR_GET_SECCOMP, 0, 0, 0, 0, 0]), 1);
        assert_eq!(handle_prctl([PR_GET_SECCOMP, 0, 0, 0, 0, 0]), 1);
    }

    /// `umask(2)`: the previous mask is the answer, and only the low
    /// twelve bits of the request are kept.
    #[test]
    fn umask_reports_the_previous_mask_and_keeps_twelve_bits() {
        // Init's own call, against the 0o022 the kernel starts with.
        assert_eq!(handle_umask([0o022, 0, 0, 0, 0, 0]), 0o022);
        assert_eq!(handle_umask([0o027, 0, 0, 0, 0, 0]), 0o022);
        assert_eq!(handle_umask([0, 0, 0, 0, 0, 0]), 0o027);
        // Bits outside the mode word do not exist here.
        assert_eq!(handle_umask([0o7777, 0, 0, 0, 0, 0]), 0);
        assert_eq!(handle_umask([0o17777, 0, 0, 0, 0, 0]), 0o7777);
        // Leave the conventional 0o022 in place for whoever reads next.
        handle_umask([0o022, 0, 0, 0, 0, 0]);
    }

    /// `getrandom(2)`: the full length is the answer, zero asks for
    /// nothing, and two draws differ - the one property the generator owes.
    #[test]
    fn getrandom_fills_the_buffer_and_answers_its_length() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let args = [a.as_mut_ptr() as usize, 64, 0, 0, 0, 0];
        assert_eq!(handle_getrandom(args, false), 64);
        let args = [b.as_mut_ptr() as usize, 64, 0, 0, 0, 0];
        assert_eq!(handle_getrandom(args, false), 64);
        assert_ne!(a, b, "two draws of 64 bytes must differ");
        assert!(a.iter().any(|&x| x != 0));

        // A zero length asks for nothing and costs nothing.
        assert_eq!(
            handle_getrandom([a.as_mut_ptr() as usize, 0, 0, 0, 0, 0], false),
            0
        );
        // GRND_RANDOM / GRND_NONBLOCK change nothing: the generator is
        // always ready and never blocks, which is what both ask about.
        let args = [a.as_mut_ptr() as usize, 8, 1 | 2, 0, 0, 0];
        assert_eq!(handle_getrandom(args, false), 8);
        // The flag word is a closed set: a bit outside {NONBLOCK, RANDOM,
        // AUTOCONF} is EINVAL, checked before the zero-length short-circuit.
        // Both counterfactual - the old handler discarded `flags`.
        assert_eq!(
            handle_getrandom([a.as_mut_ptr() as usize, 8, 0x100, 0, 0, 0], false),
            err::EINVAL,
            "an unknown GRND_* bit is EINVAL"
        );
        assert_eq!(
            handle_getrandom([a.as_mut_ptr() as usize, 0, 0x100, 0, 0, 0], false),
            err::EINVAL,
            "flags are checked before the len==0 short-circuit"
        );
        // The new GRND_AUTOCONF bit is accepted (measured on the host).
        assert_eq!(
            handle_getrandom([a.as_mut_ptr() as usize, 8, 4, 0, 0, 0], false),
            8,
            "GRND_AUTOCONF is a known bit and is accepted"
        );
    }

    /// `getrlimit(2)`/`prlimit64(2)`: the resources a static binary reads -
    /// an 8 MiB stack, a descriptor ceiling, a process ceiling, infinity for
    /// the rest - written as the `rlim_cur, rlim_max` pair the struct
    /// spells, with `EINVAL` past the table.
    #[test]
    fn rlimits_report_the_startup_defaults_and_refuse_unknown_rows() {
        let mut buf = [0u8; 16];
        let pair = |resource: usize, out: usize| {
            let args = [0, resource, 0, out, 0, 0];
            (
                handle_prlimit64(args, false),
                handle_getrlimit([resource, out, 0, 0, 0, 0], false),
            )
        };
        let words = |b: &[u8]| {
            (
                u64::from_le_bytes(b[0..8].try_into().unwrap()),
                u64::from_le_bytes(b[8..16].try_into().unwrap()),
            )
        };

        // RLIMIT_STACK (3): 8 MiB, hard and soft.
        buf = [0u8; 16];
        assert_eq!(pair(3, buf.as_mut_ptr() as usize), (0, 0));
        assert_eq!(words(&buf), (8 * 1024 * 1024, 8 * 1024 * 1024));
        // RLIMIT_NOFILE (5): 1024 soft, 1048576 hard.
        buf = [0u8; 16];
        assert_eq!(pair(5, buf.as_mut_ptr() as usize), (0, 0));
        assert_eq!(words(&buf), (1024, 1048576));
        // RLIMIT_NPROC (7): 4096 both ways.
        buf = [0u8; 16];
        assert_eq!(pair(7, buf.as_mut_ptr() as usize), (0, 0));
        assert_eq!(words(&buf), (4096, 4096));
        // An unlisted resource reports RLIM_INFINITY both ways.
        buf = [0u8; 16];
        assert_eq!(pair(0, buf.as_mut_ptr() as usize), (0, 0));
        assert_eq!(words(&buf), (u64::MAX, u64::MAX));
        buf = [0u8; 16];
        assert_eq!(pair(15, buf.as_mut_ptr() as usize), (0, 0));
        assert_eq!(words(&buf), (u64::MAX, u64::MAX));

        // Past the table: EINVAL, before any pointer is touched.
        assert_eq!(
            handle_prlimit64([0, 16, 0, buf.as_mut_ptr() as usize, 0, 0], false),
            crate::syscall::err::EINVAL
        );
        assert_eq!(
            handle_getrlimit([16, buf.as_mut_ptr() as usize, 0, 0, 0, 0], false),
            crate::syscall::err::EINVAL
        );

        // A null out-pointer is "not interested", not an error.
        assert_eq!(handle_prlimit64([0, 3, 0, 0, 0, 0], false), 0);
        assert_eq!(handle_getrlimit([3, 0, 0, 0, 0, 0], false), 0);

        // setrlimit rejects an unknown resource first, then a null `rlim`
        // (EFAULT, since a real set dereferences it); the storage lives in the
        // task table, so a host build accepts an in-range set and changes
        // nothing.
        assert_eq!(
            handle_setrlimit([16, 0, 0, 0, 0, 0], false),
            crate::syscall::err::EINVAL
        );
        assert_eq!(
            handle_setrlimit([3, 0, 0, 0, 0, 0], false),
            crate::syscall::err::EFAULT
        );
    }

    /// The `setrlimit` rule, on its own: soft may not exceed hard, and only
    /// root may lift a hard ceiling - lowering is always allowed.
    #[test]
    fn setrlimit_admits_by_privilege() {
        let e_einval = crate::syscall::err::EINVAL;
        let e_eperm = crate::syscall::err::EPERM;
        // Soft above hard is EINVAL, root or not.
        assert_eq!(
            rlimit_admit(
                Rlimit { cur: 10, max: 20 },
                Rlimit { cur: 30, max: 20 },
                true
            )
            .err(),
            Some(e_einval)
        );
        // An unprivileged process may not raise the hard limit...
        assert_eq!(
            rlimit_admit(
                Rlimit { cur: 10, max: 20 },
                Rlimit { cur: 10, max: 40 },
                false
            )
            .err(),
            Some(e_eperm)
        );
        // ...but root may, and anyone may lower it or hold it.
        assert!(
            rlimit_admit(
                Rlimit { cur: 10, max: 20 },
                Rlimit { cur: 10, max: 40 },
                true
            )
            .is_ok()
        );
        assert!(
            rlimit_admit(
                Rlimit { cur: 10, max: 20 },
                Rlimit { cur: 5, max: 10 },
                false
            )
            .is_ok()
        );
        assert!(
            rlimit_admit(
                Rlimit { cur: 20, max: 20 },
                Rlimit { cur: 20, max: 20 },
                false
            )
            .is_ok()
        );
        // The startup table: an 8 MiB stack, a 1024/1M descriptor ceiling.
        let d = default_rlimits();
        assert_eq!((d[3].cur, d[3].max), (8 * 1024 * 1024, 8 * 1024 * 1024));
        assert_eq!((d[5].cur, d[5].max), (1024, 1_048_576));
        assert_eq!((d[0].cur, d[0].max), (u64::MAX, u64::MAX));
    }

    /// `sched_setaffinity(2)`/`sched_getaffinity(2)`: one CPU, so the mask
    /// is bit 0 and the only acceptable pin keeps it; the get half answers
    /// the byte count Linux returns.
    #[test]
    fn affinity_masks_name_exactly_the_one_cpu() {
        let mut mask = [0u8; 8];
        // Get: bit 0 set, byte count is the answer, the rest is cleared.
        assert_eq!(
            handle_sched_getaffinity([0, 8, mask.as_mut_ptr() as usize, 0, 0, 0]),
            8
        );
        assert_eq!(u64::from_le_bytes(mask), 1);

        // A short mask cannot name a CPU set: EINVAL.
        assert_eq!(
            handle_sched_getaffinity([0, 4, mask.as_mut_ptr() as usize, 0, 0, 0]),
            crate::syscall::err::EINVAL
        );
        assert_eq!(
            handle_sched_setaffinity([0, 4, mask.as_mut_ptr() as usize, 0, 0, 0]),
            crate::syscall::err::EINVAL
        );

        // Set: keeping CPU 0 is fine; asking for a machine that is not here
        // is refused.
        let keep = 1u64.to_le_bytes();
        assert_eq!(
            handle_sched_setaffinity([0, 8, keep.as_ptr() as usize, 0, 0, 0]),
            0
        );
        let drop_zero = 2u64.to_le_bytes();
        assert_eq!(
            handle_sched_setaffinity([0, 8, drop_zero.as_ptr() as usize, 0, 0, 0]),
            crate::syscall::err::EINVAL
        );

        // A long mask gets its tail zeroed, as Linux clears what it does not fill.
        let mut long = [0xFFu8; 16];
        assert_eq!(
            handle_sched_getaffinity([0, 16, long.as_mut_ptr() as usize, 0, 0, 0]),
            8
        );
        assert_eq!(u64::from_le_bytes(long[0..8].try_into().unwrap()), 1);
        assert!(long[8..].iter().all(|&b| b == 0));
    }

    /// The registration calls a libc's startup makes: `set_robust_list` pins its
    /// length to 24 and its head round-trips through `get_robust_list`. The
    /// robust pair is the point: a `set` that filed nothing and a `get` that only
    /// ever answered `ENOSYS` could not round-trip, so the read half returning
    /// what the write half stored is the contract this pins. (`rseq` is not here
    /// on purpose: the kernel carries no registration for it and the dispatch
    /// answers `ENOSYS`, so there is no handler to unit-test - `t-rseq` pins that
    /// refusal at the syscall boundary.)
    #[test]
    fn startup_registrations_check_their_abi_sizes() {
        assert_eq!(handle_set_robust_list([0x1000, 24, 0, 0, 0, 0]), 0);
        assert_eq!(
            handle_set_robust_list([0x1000, 32, 0, 0, 0, 0]),
            crate::syscall::err::EINVAL
        );

        // The head `set` filed comes back through `get`, with Linux's pinned
        // `sizeof(struct robust_list_head)` = 24 as the length.
        let mut head_out = 0usize;
        let mut len_out = 0usize;
        assert_eq!(
            handle_get_robust_list([
                0,
                &raw mut head_out as usize,
                &raw mut len_out as usize,
                0,
                0,
                0,
            ]),
            0
        );
        assert_eq!(head_out, 0x1000, "the registered head reads back");
        assert_eq!(len_out, 24, "get reports the list head's size");

        // Null output pointers are EFAULT, not a silent answer.
        assert_eq!(
            handle_get_robust_list([0, 0, 0, 0, 0, 0]),
            crate::syscall::err::EFAULT
        );
    }

    /// `membarrier(2)`: what `QUERY` advertises and what the handler accepts are
    /// one word, so these checks are really about the *refusals* - the commands
    /// that are named in Linux's ABI, are plausible, and would be promises this
    /// kernel cannot keep.
    #[test]
    fn membarrier_advertises_exactly_what_it_accepts() {
        let query = handle_membarrier([MEMBARRIER_CMD_QUERY, 0, 0, 0, 0, 0]);
        assert_eq!(query as usize, MEMBARRIER_SUPPORTED);
        // QUERY names no processor, so flags are not its business.
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_QUERY, MEMBARRIER_CMD_FLAG_CPU, 0, 0, 0, 0]),
            err::EINVAL
        );

        // Every advertised command is answered, registration ones included -
        // which is the part that used to be refused, and the part musl's
        // `pthread_create` actually asks for.
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, 0, 0, 0, 0]),
            0
        );
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED, 0, 0, 0, 0, 0]),
            0
        );
        assert_eq!(handle_membarrier([MEMBARRIER_CMD_GLOBAL, 0, 0, 0, 0, 0]), 0);

        // The two commands named in the ABI and deliberately not advertised:
        // `SYNC_CORE` (32) and `RSEQ` (128), with their registrations.
        for command in [32usize, 64, 128, 256] {
            assert_eq!(
                handle_membarrier([command, 0, 0, 0, 0, 0]),
                err::EINVAL,
                "command {command}"
            );
            assert_eq!(
                query as usize & command,
                0,
                "command {command} must not be advertised"
            );
        }

        // A command word is one command: two of our own bits is two at once.
        assert_eq!(
            handle_membarrier([
                MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                0,
                0,
                0,
                0,
                0
            ]),
            err::EINVAL
        );
    }

    /// An expedited barrier this process never registered for is `EPERM`, not a
    /// shrug: the caller asked for a guarantee and has not done the bookkeeping
    /// that buys it. The host has nothing registered, so both arms are here.
    #[test]
    fn membarrier_refuses_an_unregistered_expedited_barrier() {
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0, 0, 0, 0]),
            err::EPERM
        );
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_GLOBAL_EXPEDITED, 0, 0, 0, 0, 0]),
            err::EPERM
        );
    }

    /// `MEMBARRIER_CMD_FLAG_CPU` makes the third argument meaningful, and only
    /// the global expedited barrier takes a processor to act on.
    #[test]
    fn membarrier_only_lets_the_global_barrier_name_a_cpu() {
        // The machine has one processor, numbered zero.
        assert_eq!(
            handle_membarrier([
                MEMBARRIER_CMD_GLOBAL_EXPEDITED,
                MEMBARRIER_CMD_FLAG_CPU,
                0,
                0,
                0,
                0
            ]),
            err::EPERM, // named correctly, but this process registered nothing
        );
        // A processor that is not there, and a private barrier that has no
        // business naming one at all.
        assert_eq!(
            handle_membarrier([
                MEMBARRIER_CMD_GLOBAL_EXPEDITED,
                MEMBARRIER_CMD_FLAG_CPU,
                1,
                0,
                0,
                0
            ]),
            err::EINVAL
        );
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_GLOBAL, MEMBARRIER_CMD_FLAG_CPU, 0, 0, 0, 0]),
            err::EINVAL
        );
        // A flag word nobody defined.
        assert_eq!(
            handle_membarrier([MEMBARRIER_CMD_GLOBAL, 1 << 8, 0, 0, 0, 0]),
            err::EINVAL
        );
    }

    /// A missing call is reported once: the second attempt and every one
    /// after it stay quiet, and a number past the table is never reported
    /// at all - there is no word to set a bit in.
    #[test]
    fn enosys_is_recorded_once_and_out_of_range_never() {
        // A number inside the four-word table that no other test touches.
        let nr = 250u64;
        assert!(mark_missing(nr), "the first ENOSYS is reported");
        assert!(!mark_missing(nr), "the second is a duplicate");
        assert!(!mark_missing(nr), "and so is every one after");

        // Past the four-word table: no report, ever, and no panic.
        assert!(!mark_missing(256));
        assert!(!mark_missing(500));
        assert!(!mark_missing(1 << 40));
        assert!(!mark_missing(u64::MAX));

        // Distinct numbers are reported independently.
        assert!(
            mark_missing(251),
            "a different number is its own first report"
        );
    }

    /// The flag-bit and structure constants of this module are the ABI, not
    /// a choice: each one is the value the man page or the kernel headers
    /// spell.
    #[test]
    fn abi_flag_bits_match_the_man_pages() {
        // open(2)
        assert_eq!(O_CREAT, 0o100);
        assert_eq!(O_EXCL, 0o200);
        assert_eq!(O_TRUNC, 0o1000);
        assert_eq!(O_CLOEXEC, 0o200_0000);
        assert_eq!(O_ACCMODE, 0o3);
        assert_eq!(O_DIRECTORY, 0o20_0000);
        assert_eq!(O_TMPFILE, 0o2000_0000);
        assert_eq!(O_TMPFILE | O_DIRECTORY, 0x410000);
        // fcntl(2)
        for (cmd, value) in [
            (F_DUPFD, 0),
            (F_GETFD, 1),
            (F_SETFD, 2),
            (F_GETFL, 3),
            (F_SETFL, 4),
            (F_GETLK, 5),
            (F_SETLK, 6),
            (F_SETLKW, 7),
        ] {
            assert_eq!(cmd, value);
        }
        // The Linux-specific block starts at 1024, and this is its seventh.
        assert_eq!(F_DUPFD_CLOEXEC, 1024 + 6);
        assert_eq!(F_DUPFD_CLOEXEC, 1030);
        assert_eq!(FD_CLOEXEC, 1);
        // fcntl(2)'s AT_* and *at relatives
        assert_eq!(AT_FDCWD, -100);
        assert_eq!(AT_SYMLINK_NOFOLLOW, 0x100);
        assert_eq!(AT_SYMLINK_FOLLOW, 0x400);
        assert_eq!(AT_EMPTY_PATH, 0x1000);
        assert_eq!(AT_REMOVEDIR, 0x200);
        // poll(2)
        assert_eq!(POLLIN, 0x001);
        assert_eq!(POLLOUT, 0x004);
        assert_eq!(POLLERR, 0x008);
        assert_eq!(POLLHUP, 0x010);
        assert_eq!(POLLNVAL, 0x020);
        assert_eq!(POLLFD_SIZE, 8);
        assert_eq!(POLLFD_EVENTS, 4);
        assert_eq!(POLLFD_REVENTS, 6);
        assert_eq!(FD_SETSIZE, 1024);
        // mmap(2)/mprotect(2)/mremap(2)
        assert_eq!(MAP_SHARED, 0x01);
        assert_eq!(MAP_FIXED, 0x10);
        assert_eq!(MAP_ANONYMOUS, 0x20);
        assert_eq!(PROT_WRITE, 0x2);
        assert_eq!(PROT_EXEC, 0x4);
        assert_eq!(MREMAP_MAYMOVE, 0x1);
        assert_eq!(MREMAP_FIXED, 0x2);
        assert_eq!(MADV_DONTNEED, 4);
        // eventfd(2)
        assert_eq!(EFD_SEMAPHORE, 1);
        assert_eq!(EFD_NONBLOCK, 0x800);
        // statx(2)
        assert_eq!(STATX_BASIC_STATS, 0x7ff);
        assert_eq!(STATX_SIZE, 152);
        assert_eq!(SIGINFO_BYTES, 128);
        assert_eq!(RLIM_NLIMITS, 16);
        assert_eq!(RLIM_INFINITY, u64::MAX);
    }

    /// `madvise`'s advice vocabulary is a closed set: Linux answers `EINVAL`
    /// for a number it does not name. The predicate below is what the handler
    /// gates on, so a garbage advice can no longer be answered with a silent
    /// success. The counterfactual is the deleted/absent numbers the real host
    /// refuses — `5`, `6`, `7` (the retired `MADV_*`), `9` (`MADV_REMOVE`, which
    /// faults on an anonymous range), and a value past the block like `9999` —
    /// all `false`, against the recognised hints (`0..=4`, `8`, `10..=24`) all
    /// `true`.
    #[test]
    fn madvise_advice_gate_matches_linux_known_set() {
        // The recognised hints and the one that does work (DONTNEED = 4).
        for a in [0usize, 1, 2, 3, 4, 8, 10, 11, 14, 21, 23, 24, 102, 103] {
            assert!(madvise_advice_is_known(a), "advice {a} is a known value");
        }
        // The retired/absent numbers Linux rejects with EINVAL.
        for a in [5usize, 6, 7, 9, 25, 26, 50, 99, 100, 101, 104, 9999] {
            assert!(
                !madvise_advice_is_known(a),
                "advice {a} is not a known value"
            );
        }
    }

    /// `statx`'s flag word is a closed set, the same discipline. The handler
    /// gated `_ => EINVAL`-style now on this predicate; the counterfactual is
    /// the unknown bits the old handler discarded. Measured on the host:
    /// `{0x100, 0x800, 0x1000, 0x2000, 0x4000}` (and any combination of them)
    /// are accepted; `0x10000` and `0x100000` are `EINVAL`.
    #[test]
    fn statx_flag_gate_matches_linux_known_set() {
        // The recognised bits, singly and combined.
        for f in [
            0usize,
            0x100,
            0x800,
            0x1000,
            0x2000,
            0x4000,
            0x100 | 0x800,
            0x1000 | 0x4000,
        ] {
            assert!(statx_flags_are_known(f), "flag {f:#x} is a known value");
        }
        // A bit outside the set is not.
        for f in [0x10000usize, 0x100000, 0x200, 0x400, 0x80, 0x100 | 0x10000] {
            assert!(
                !statx_flags_are_known(f),
                "flag {f:#x} is not a known value"
            );
        }
    }

    /// The `ptrace` request numbers, against the ptrace(2) man page's x86_64
    /// column. These are worse than most to get wrong: a `TRACEME` that lands
    /// on `PEEKTEXT` reads the tracer's own memory instead of handing the
    /// child over, and nothing reports it - the tracer just never sees a stop.
    #[test]
    fn ptrace_requests_and_options_match_the_man_page() {
        assert_eq!(PTRACE_TRACEME, 0);
        assert_eq!(PTRACE_PEEKTEXT, 1);
        assert_eq!(PTRACE_PEEKDATA, 2);
        assert_eq!(PTRACE_PEEKUSER, 3);
        assert_eq!(PTRACE_POKETEXT, 4);
        assert_eq!(PTRACE_POKEDATA, 5);
        assert_eq!(PTRACE_POKEUSER, 6);
        assert_eq!(PTRACE_CONT, 7);
        assert_eq!(PTRACE_KILL, 8);
        assert_eq!(PTRACE_SINGLESTEP, 9);
        assert_eq!(PTRACE_GETREGS, 12);
        assert_eq!(PTRACE_SETREGS, 13);
        assert_eq!(PTRACE_ATTACH, 16);
        assert_eq!(PTRACE_DETACH, 17);
        assert_eq!(PTRACE_SYSCALL, 24);
        assert_eq!(PTRACE_SETOPTIONS, 0x4200);
        assert_eq!(PTRACE_GETEVENTMSG, 0x4201);
        assert_eq!(PTRACE_GETSIGINFO, 0x4202);
        assert_eq!(PTRACE_GETREGSET, 0x4204);
        assert_eq!(PTRACE_SETREGSET, 0x4205);
        assert_eq!(PTRACE_GET_SYSCALL_INFO, 0x420E);
        assert_eq!(PTRACE_O_TRACESYSGOOD, 1);
        assert_eq!(PTRACE_O_TRACEEXEC, 0x10);
        assert_eq!(PTRACE_O_EXITKILL, 0x0010_0000);
        assert_eq!(PTRACE_EVENT_EXEC, 4);
        // The fork family, and the last two this kernel names but never raises.
        // Pinned for the same reason as the rest: a tracer's switch on these
        // numbers is written against them, and a wrong one there is a stop read
        // as the wrong kind.
        assert_eq!(PTRACE_EVENT_FORK, 1);
        assert_eq!(PTRACE_EVENT_VFORK, 2);
        assert_eq!(PTRACE_EVENT_CLONE, 3);
        assert_eq!(unwatched_events::VFORK_DONE, 5);
        assert_eq!(unwatched_events::EXIT, 6);
        assert_eq!(NT_PRSTATUS, 1);
        assert_eq!(SYSCALL_TRAP, 0x80);
        // The wait(2) options a tracer passes.
        assert_eq!(WNOHANG, 1);
        assert_eq!(WUNTRACED, 2);
        assert_eq!(WCONTINUED, 8);
        assert_eq!(__WALL, 0x4000_0000);
        assert_eq!(__WCLONE, 0x8000_0000);
    }

    /// The stop word, decoded the way the libc decodes it. Writing the three
    /// fields anywhere but where `WSTOPSIG` and `WIFSTOPPED` look produces a
    /// status a tracer reads as a plausible, wrong answer - the failure mode
    /// this kernel has already met once, in the exit-status word.
    #[test]
    fn stop_status_is_readable_by_the_w_macros() {
        // A plain syscall stop: SIGTRAP with TRACESYSGOOD's bit, no event.
        let word = stop_status(5 | SYSCALL_TRAP, PTRACE_EVENT_NONE);
        assert_eq!(word & 0xff, 0x7f, "WIFSTOPPED");
        assert_eq!((word >> 8) & 0xff, 0x85, "WSTOPSIG carries the 0x80");
        assert_eq!(word >> 16, 0);

        // An exec event: the event number in the high byte, WSTOPSIG SIGTRAP.
        let word = stop_status(5, PTRACE_EVENT_EXEC);
        assert_eq!(word & 0xff, 0x7f);
        assert_eq!((word >> 8) & 0xff, 5);
        assert_eq!(word >> 16, PTRACE_EVENT_EXEC as i32);

        // A signal-delivery stop reports the signal itself.
        let word = stop_status(17, PTRACE_EVENT_NONE);
        assert_eq!((word >> 8) & 0xff, 17);
        assert_eq!(CONTINUED_STATUS, 0xffff);
    }

    /// `struct user_regs_struct`'s offsets. `strace` reads `rip`, `rsp` and
    /// `orig_rax` out of these by index; a field one slot over is a tracer
    /// reporting on a register nothing touched.
    #[test]
    fn user_regs_struct_field_order_is_the_abis() {
        use uregs::*;
        // The first ten are the caller-saved general registers in the order
        // ptrace(2) lists them.
        assert_eq!((R15, R14, R13, R12, RBP, RBX), (0, 1, 2, 3, 4, 5));
        assert_eq!((R11, R10, R9, R8), (6, 7, 8, 9));
        assert_eq!((RAX, RCX, RDX, RSI, RDI), (10, 11, 12, 13, 14));
        assert_eq!(ORIG_RAX, 15);
        assert_eq!((RIP, CS, EFLAGS, RSP, SS), (16, 17, 18, 19, 20));
        assert_eq!((FS_BASE, GS_BASE, DS, ES, FS, GS), (21, 22, 23, 24, 25, 26));
        assert_eq!(COUNT, 27);
        assert_eq!(BYTES, 216);

        // A round trip through the two accessors has to be the identity, and
        // the buffer is not guaranteed aligned - the same rule the rest of
        // this module follows.
        let mut buffer = [0u8; BYTES + 1];
        let base = buffer.as_mut_ptr() as usize + 1;
        let mut regs = [0u64; COUNT];
        for (slot, value) in regs.iter_mut().enumerate() {
            *value = 0x1000 + slot as u64;
        }
        write_user_regs(base, &regs);
        assert_eq!(read_user_regs(base), regs);
        assert_eq!(read_user_regs(base)[RIP], 0x1000 + RIP as u64);
    }

    /// `struct ptrace_syscall_info`: the size, and where the two shapes put
    /// their fields. `strace` decides whether a stop was an entry or an exit
    /// from `op` alone, and reads the syscall number and its arguments from
    /// the entry half - so both have to be exactly where the header says.
    #[test]
    fn ptrace_syscall_info_has_the_layout_the_header_declares() {
        assert_eq!(SYSCALL_INFO_BYTES, 80);
        assert_eq!(AUDIT_ARCH_X86_64, 0xC000_003E);

        let mut buffer = [0u8; SYSCALL_INFO_BYTES + 3];
        let base = buffer.as_mut_ptr() as usize + 3;

        // An entry stop: op, arch, ip, sp, nr and six arguments.
        write_syscall_info(
            base,
            true,
            59, // execve
            &[1, 2, 3, 4, 5, 6],
            0xdead_beef,
            0x7fff_0000,
            0,
        );
        assert_eq!(buffer[3], SYSCALL_INFO_ENTRY);
        assert_eq!(
            u32::from_le_bytes(buffer[3 + 4..3 + 8].try_into().unwrap()),
            AUDIT_ARCH_X86_64
        );
        assert_eq!(
            u64::from_le_bytes(buffer[3 + 8..3 + 16].try_into().unwrap()),
            0xdead_beef
        );
        assert_eq!(
            u64::from_le_bytes(buffer[3 + 16..3 + 24].try_into().unwrap()),
            0x7fff_0000
        );
        assert_eq!(
            u64::from_le_bytes(buffer[3 + 24..3 + 32].try_into().unwrap()),
            59
        );
        assert_eq!(
            u64::from_le_bytes(buffer[3 + 32..3 + 40].try_into().unwrap()),
            1
        );
        assert_eq!(
            u64::from_le_bytes(buffer[3 + 72..3 + 80].try_into().unwrap()),
            6
        );

        // An exit stop reuses the same bytes: the return value where the
        // entry shape keeps the number, and the error flag beside it.
        write_syscall_info(base, false, 59, &[0; 6], 0xbeef, 0x1000, -14);
        assert_eq!(buffer[3], SYSCALL_INFO_EXIT);
        assert_eq!(
            i64::from_le_bytes(buffer[3 + 24..3 + 32].try_into().unwrap()),
            -14
        );
        assert_eq!(buffer[3 + 32], 1, "EFAULT is an error");
        // A result at the very edge of the errno range is still an error, and
        // one below it is a legitimate (if odd) value.
        write_syscall_info(base, false, 0, &[0; 6], 0, 0, -4095);
        assert_eq!(buffer[3 + 32], 1);
        write_syscall_info(base, false, 0, &[0; 6], 0, 0, -4096);
        assert_eq!(buffer[3 + 32], 0);
        write_syscall_info(base, false, 0, &[0; 6], 0, 0, 7);
        assert_eq!(buffer[3 + 32], 0);
    }
}

/// The OSS request words, recomputed from the `_IOC` layout they are spelled
/// with in `linux/soundcard.h` - the header on a real machine that says these
/// are what a compiled binary sends.
#[cfg(test)]
mod oss_tests {
    use super::*;

    const fn ioc(dir: u32, size: u32, type_: u8, nr: u8) -> u32 {
        (dir << 30) | (size << 16) | ((type_ as u32) << 8) | nr as u32
    }

    #[test]
    fn the_dsp_request_words_are_the_soundcard_ones() {
        assert_eq!(SNDCTL_DSP_RESET, ioc(0, 0, b'P', 0));
        assert_eq!(SNDCTL_DSP_SYNC, ioc(0, 0, b'P', 1));
        assert_eq!(SNDCTL_DSP_SPEED, ioc(3, 4, b'P', 2));
        assert_eq!(SNDCTL_DSP_STEREO, ioc(3, 4, b'P', 3));
        assert_eq!(SNDCTL_DSP_GETBLKSIZE, ioc(3, 4, b'P', 4));
        assert_eq!(SNDCTL_DSP_SETFMT, ioc(3, 4, b'P', 5));
        assert_eq!(SNDCTL_DSP_CHANNELS, ioc(3, 4, b'P', 6));
        assert_eq!(SNDCTL_DSP_POST, ioc(0, 0, b'P', 8));
        assert_eq!(SNDCTL_DSP_SUBDIVIDE, ioc(3, 4, b'P', 9));
        assert_eq!(SNDCTL_DSP_SETFRAGMENT, ioc(3, 4, b'P', 10));
        assert_eq!(SNDCTL_DSP_GETFMTS, ioc(2, 4, b'P', 11));
        assert_eq!(SNDCTL_DSP_GETOSPACE, ioc(2, 16, b'P', 12));
        assert_eq!(SNDCTL_DSP_GETISPACE, ioc(2, 16, b'P', 13));
        assert_eq!(SNDCTL_DSP_NONBLOCK, ioc(0, 0, b'P', 14));
        assert_eq!(AUDIO_BUF_INFO_SIZE, 16);
        // The numbers themselves, as the header's expansion gives them: a
        // regression here changes which word reaches which device family.
        assert_eq!(SNDCTL_DSP_SPEED, 0xC004_5002);
        assert_eq!(SNDCTL_DSP_GETOSPACE, 0x8010_500C);
    }

    #[test]
    fn a_requested_rate_rounds_to_the_nearest_the_device_has() {
        // Measured against the rounding `snd-pcm-oss` reports back: 44100 onto
        // a device that has it is 44100; onto one that does not, it is the
        // closest - by relative distance, so 48000 answers here and not some
        // lower rate that is further in percent.
        assert_eq!(oss_round_rate(44100, &[44100, 48000, 96000]), Some(44100));
        assert_eq!(oss_round_rate(44100, &[8000, 22050, 48000]), Some(48000));
        assert_eq!(oss_round_rate(10000, &[8000, 11025]), Some(11025));
        assert_eq!(oss_round_rate(0, &[8000]), Some(8000));
        assert_eq!(oss_round_rate(44100, &[]), None);
    }
}
