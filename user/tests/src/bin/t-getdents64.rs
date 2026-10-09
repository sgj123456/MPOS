//! t-getdents64 - one test for getdents64, the raw directory stream ls and
//! every glob walk record by record.
//!
//! Pinned here: the loop terminates - after the last record a further call
//! answers 0, which is what the descriptor's entry cursor owes every
//! readdir loop; the records parse in the Linux shape (`d_reclen` strides the
//! buffer, `d_type` labels each name); the labels are the ones the filesystem
//! itself gives (`d_type` agrees with `lstat`, so a symlink is `DT_LNK` and a
//! directory is `DT_DIR`); seeking back to 0 restarts the stream; /etc lists at
//! least one entry; and a buffer too small for the first record answers EINVAL.
//!
//! # The migration, and the one claim that moved
//!
//! The stream is read through the raw door - `libc::syscall(SYS_getdents64, ..)`
//! - because the record's *shape* is what half these claims are: `d_reclen`
//! strides record to record and `d_type` labels each name, and a wrapper that
//! hands back parsed entries hides both. That is `t-waitid`'s reason for owning
//! its `siginfo_t`, applied to a directory. `SYS_getdents64` is `libc`'s own
//! per-machine number (217 here, 61 on the second machine).
//!
//! What the raw door must not carry is the repository's memory of the record's
//! *layout*. The no-std version read the fields at the literals 0, 8, 16, 18 and
//! 19 through a hand-written parser; the migrated program takes the offsets from
//! the machine's own `libc::dirent64` (`offset_of!`) and pins them against those
//! same numbers as compile-time claims, so `dirent64` drifting would be a build
//! failure rather than a silently wrong stride - the shape `t-waitid` gives
//! `siginfo_t`. `dirent64` is the *64-bit* spelling because that is the record
//! `getdents64` writes; its `d_name` is a fixed 256-byte array in the C header,
//! but the wire record is variable-length and NUL-terminated, so the name is
//! still read as a C string inside `d_reclen`.
//!
//! **The file-type claims moved.** The no-std version wrote the busybox layout
//! down - "sh" and "cat" turn up as `DT_LNK`, "/bin" at the root as `DT_DIR` -
//! which is a fact about the root the program happens to run in, not about the
//! syscall: it is true of both machines' roots and false of a merged-`/usr`
//! host, where `/bin` is a symlink to `usr/bin` and `/bin/cat` a regular file.
//! A claim that only holds on the author's root is not a conformance claim, so
//! what is pinned now is that the kernel's `d_type` **is the type `lstat`
//! reports** - the property a client's `ls -F` actually compares - and that
//! both names are *listed* at all. That is the same rule, stated so it can be
//! wrong anywhere: a kernel that labelled every entry `DT_REG`, or that let a
//! symlink answer as its target, fails it on any root including this one.
//!
//! The rest of the program is the named calls: the opens and the rewind are
//! `rustix::fs::{open, seek}` with `SeekFrom::Start`, the labels compared
//! against are `libc::DT_*`, the filesystem's own answer is `lstat` through
//! `std::fs::symlink_metadata` (which is `lstat`, not `stat` - following the
//! link would answer for the target), and the lowest free descriptor is
//! discovered with `rustix::io::dup` rather than assumed to be 3 - `t-dup`'s
//! probe, because what this process inherited is not this program's to know.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd};
use rustix::fs::{Mode, OFlags, SeekFrom, open, seek};

/// The uapi's `struct linux_dirent64` field offsets, taken from the machine's
/// own `libc::dirent64` and pinned to the numbers `linux/dirent.h` fixes. The
/// pins are the point: a `dirent64` whose layout drifted would otherwise walk
/// the buffer at the wrong stride and parse garbage as names.
const OFF_INO: usize = core::mem::offset_of!(libc::dirent64, d_ino);
const OFF_OFF: usize = core::mem::offset_of!(libc::dirent64, d_off);
const OFF_RECLEN: usize = core::mem::offset_of!(libc::dirent64, d_reclen);
const OFF_TYPE: usize = core::mem::offset_of!(libc::dirent64, d_type);
const OFF_NAME: usize = core::mem::offset_of!(libc::dirent64, d_name);
const _: () = assert!(OFF_INO == 0);
const _: () = assert!(OFF_OFF == 8);
const _: () = assert!(OFF_RECLEN == 16);
const _: () = assert!(OFF_TYPE == 18);
const _: () = assert!(OFF_NAME == 19);

/// The read buffer, and the cap on how many calls one pass may make before it
/// decides the stream is not going to end.
const BUF: usize = 4096;
const PASS_LIMIT: usize = 64;

/// `getdents64(fd, buf, len)` through the raw door. `-errno` on failure, the
/// byte count on success, `0` at end of stream - the syscall's own answers.
fn getdents64(fd: BorrowedFd<'_>, buf: &mut [u8]) -> isize {
    // SAFETY: `buf` outlives the call and the kernel writes only inside it.
    let r = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd.as_raw_fd() as libc::c_long,
            buf.as_mut_ptr() as libc::c_long,
            buf.len() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// One parsed record: the field a caller branches on, and the name.
struct Record<'a> {
    d_type: u8,
    name: &'a [u8],
}

/// Parse the first record of a buffer, with the `d_reclen` that steps to the
/// next. The offsets are `dirent64`'s; the length and the name are the wire
/// record's, which is variable and NUL-terminated inside `d_reclen`.
fn parse(buf: &[u8]) -> Option<(Record<'_>, usize)> {
    if buf.len() < OFF_NAME {
        return None;
    }
    let reclen = u16::from_le_bytes(buf[OFF_RECLEN..OFF_RECLEN + 2].try_into().ok()?) as usize;
    if reclen < OFF_NAME || reclen > buf.len() {
        return None;
    }
    let d_type = buf[OFF_TYPE];
    let name = &buf[OFF_NAME..reclen];
    let stop = name.iter().position(|&b| b == 0)?;
    Some((
        Record {
            d_type,
            name: &name[..stop],
        },
        reclen,
    ))
}

/// The `d_type` the filesystem itself would give `path`. `symlink_metadata` is
/// `lstat`, so a symlink answers for the link and not its target - the whole
/// point of the comparison. `None` for a kind `d_type` names none of here.
fn expected_d_type(path: &str) -> Option<u8> {
    let ft = std::fs::symlink_metadata(path).ok()?.file_type();
    if ft.is_symlink() {
        Some(libc::DT_LNK)
    } else if ft.is_dir() {
        Some(libc::DT_DIR)
    } else if ft.is_file() {
        Some(libc::DT_REG)
    } else {
        None
    }
}

/// One claim: the entry named `name` in the pass is listed, and the kernel's
/// `d_type` for it is the one `lstat` gives its full path.
fn check_type(r: &mut Report, listed: Option<u8>, path: &str) {
    let claim = format!("d_type for {path} is the type lstat reports");
    match (listed, expected_d_type(path)) {
        (Some(got), Some(want)) => r.check(&claim, got == want),
        (Some(got), None) => r.check(
            &format!("{claim} (lstat names no kind here; d_type {got})"),
            false,
        ),
        (None, _) => r.check(&format!("{claim} ({path} was not listed)"), false),
    }
}

/// One full pass over the directory `fd` names: counts records, remembers the
/// `d_type` of the two names it is asked about, and says whether the stream
/// ended with the terminating 0.
fn scan(
    fd: BorrowedFd<'_>,
    want1: &[u8],
    want2: &[u8],
    types: &mut [Option<u8>; 2],
) -> (usize, bool) {
    let mut buf = [0u8; BUF];
    let mut count = 0usize;
    let mut eof = false;
    for _ in 0..PASS_LIMIT {
        let n = getdents64(fd, &mut buf);
        if n < 0 {
            break;
        }
        if n == 0 {
            eof = true;
            break;
        }
        let chunk = &buf[..n as usize];
        let mut off = 0usize;
        while off < chunk.len() {
            if let Some((d, reclen)) = parse(&chunk[off..]) {
                if d.name == want1 {
                    types[0] = Some(d.d_type);
                }
                if d.name == want2 {
                    types[1] = Some(d.d_type);
                }
                count += 1;
                off += reclen;
            } else {
                off = chunk.len();
            }
        }
    }
    (count, eof)
}

/// The lowest free descriptor, discovered rather than assumed: `dup` hands one
/// back, and dropping it frees the number so the opens below land on it.
fn free_fd() -> i32 {
    let fd = rustix::io::dup(std::io::stdin()).expect("dup(stdin)");
    let n = fd.as_raw_fd();
    drop(fd);
    n
}

fn main() {
    println!("t-getdents64");
    let mut r = Report::new();
    let base = free_fd();

    let bin = open(c"/bin", OFlags::RDONLY, Mode::empty()).expect("opening /bin");
    r.check(
        "the /bin directory opens on the lowest free fd",
        bin.as_raw_fd() == base,
    );

    let mut types = [None, None];
    let (count, eof) = scan(bin.as_fd(), b"sh", b"cat", &mut types);
    r.check("the readdir loop terminates with a final 0", eof);
    r.check("/bin lists records", count >= 2);
    check_type(&mut r, types[0], "/bin/sh");
    check_type(&mut r, types[1], "/bin/cat");

    r.check(
        "seeking the directory back to 0 succeeds",
        seek(&bin, SeekFrom::Start(0)).is_ok(),
    );
    let mut buf = [0u8; BUF];
    let n = getdents64(bin.as_fd(), &mut buf);
    r.check("the rewind restarts the stream, entries come again", n > 0);
    drop(bin);

    let etc = open(c"/etc", OFlags::RDONLY, Mode::empty()).expect("opening /etc");
    r.check(
        "the /etc directory opens on the lowest free fd",
        etc.as_raw_fd() == base,
    );
    let mut types = [None, None];
    let (count, eof) = scan(etc.as_fd(), b"motd", b"", &mut types);
    r.check("the /etc pass terminates", eof);
    r.check("/etc lists at least one entry", count >= 1);
    drop(etc);

    let root = open(c"/", OFlags::RDONLY, Mode::empty()).expect("opening /");
    r.check(
        "the root directory opens on the lowest free fd",
        root.as_raw_fd() == base,
    );
    let mut types = [None, None];
    let (_count, eof) = scan(root.as_fd(), b"etc", b"bin", &mut types);
    r.check("the root pass terminates", eof);
    check_type(&mut r, types[0], "/etc");
    check_type(&mut r, types[1], "/bin");
    drop(root);

    let bin = open(c"/bin", OFlags::RDONLY, Mode::empty()).expect("opening /bin again");
    r.check(
        "/bin opens again on the lowest free fd",
        bin.as_raw_fd() == base,
    );
    let mut tiny = [0u8; 8];
    let n = getdents64(bin.as_fd(), &mut tiny);
    r.check(
        "a buffer too small for one record answers EINVAL",
        n == -(libc::EINVAL as isize),
    );
    drop(bin);

    r.finish()
}
