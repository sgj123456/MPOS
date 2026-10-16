//! t-truncate - the resize, and the trap a descriptor's cursor sets for it.
//!
//! Pinned here, per truncate(2)/ftruncate(2): the resize is real in both
//! directions - a shorter file keeps no tail, and reading it back answers
//! only what is left; a negative length is EINVAL before anything is
//! touched; a path that does not exist is ENOENT; and the trap the man page
//! is built around - after reading to end-of-file, `ftruncate` to zero
//! leaves the *descriptor's* offset where it was, so the next write lands
//! past the new end and the file grows back with a hole of zeros in front of
//! it. Linux behaves exactly this way, and so does mpos: the hole reads as
//! zeros and the size is the write's end, not the write's length.
//!
//! Every claim in this file is Linux's answer on both machines, with no
//! per-machine branch and no named divergence of its own. `ftruncate` answers
//! `EINVAL` on *both* ends of a pipe here, as it does on real Linux (measured
//! through `syscall(2)`, `.workbuddy/probe_fsize.c`) - the read end because the
//! mode refuses first (that kernel's `!(f_mode & FMODE_WRITE)`), the write end
//! because a pipe has no `setattr` for the resize to reach. A pipe is built out
//! of a `File` inode here, so the handler's "not a regular file" test cannot see
//! it and asks the *node* as well; before that it fell through to the VFS
//! default and answered `EOPNOTSUPP`. The path spelling has its own pair of
//! answers, and they are not the descriptor spelling's: a directory is `EISDIR`
//! and everything else that is not a regular file `EINVAL` (measured the same
//! way), where the node's default `truncate` answered `EOPNOTSUPP` for both.
//! The one file-shaped inode this handler could not tell apart is gone rather
//! than named: a FIFO made by `mknod` was a `NodeKind::File` node here, and
//! `mknodat(S_IFIFO)` now makes a real `S_IFIFO` node (`docs/uapi/` §5.1
//! item 26), which this handler answers `EINVAL` like any other non-file. So
//! no node this kernel can make reports `File` without being one.
//!
//! Pure rustix beside one raw door: `rustix::fs::{truncate, ftruncate}`
//! bind the calls, but the *negative length* claim cannot be spelled
//! through them (their length is a `u64`/`Off`, and the ABI's EINVAL is
//! about the signed word the syscall sees) - clause (c), so that one probe
//! is `libc::syscall(SYS_ftruncate, ..)` with libc's number (77 / 46).

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, SeekFrom, fstat, openat, seek, unlinkat};
use rustix::io::{Errno, read, write};
use rustix::pipe::{PipeFlags, pipe_with};

const PATH: &str = "/tmp/t-truncate";
const MISSING: &str = "/tmp/t-truncate-gone";
/// A directory and a character device, for the path spelling's two refusals:
/// every payload carries both.
const DIR: &str = "/tmp";
const NULL_DEV: &str = "/dev/null";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn size_of<Fd: rustix::fd::AsFd>(fd: Fd) -> Option<u64> {
    fstat(fd.as_fd()).ok().map(|st| st.st_size as u64)
}

fn main() {
    println!("t-truncate");
    let mut r = Report::new();

    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    let opened = openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from_bits_retain(0o644),
    );
    r.check("the file is created", opened.is_ok());
    let fd = match opened {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };
    let raw = fd.as_raw_fd();

    r.check("ten bytes are written", write(&fd, b"0123456789") == Ok(10));
    r.check("the file is ten bytes long", size_of(&fd) == Some(10));

    // A shorter file keeps no tail.
    r.check("ftruncate to 4 succeeds", ftrunc(&fd, 4) == Ok(()));
    r.check(
        "the shrunk file is four bytes long",
        size_of(&fd) == Some(4),
    );
    r.check(
        "the offset is rewound",
        seek(&fd, SeekFrom::Start(0)) == Ok(0),
    );
    let mut buf = [0u8; 16];
    let n = read(&fd, &mut buf[..4]);
    r.check("a read answers the four bytes that are left", n == Ok(4));
    r.check(
        "and they are the front of the old contents",
        n == Ok(4) && buf[..4] == *b"0123",
    );

    // The length's own validation, before the descriptor is looked at.
    let neg = unsafe { libc::syscall(libc::SYS_ftruncate, raw, -1i64) };
    r.check(
        "a negative length answers EINVAL",
        refused(neg as isize, libc::EINVAL),
    );

    // A descriptor that is not a regular file, asked about *both* ends of a
    // pipe - and both answers are Linux's now (measured through `syscall(2)`
    // on a current host, `.workbuddy/probe_fsize.c`). The read end is
    // read-only, so the mode refuses it first: `EINVAL`, which is that
    // kernel's `!(f_mode & FMODE_WRITE)` answer for `ftruncate`. The write end
    // is writable and reaches the inode, and `EINVAL` is what Linux answers
    // there too - a pipe has no `setattr` for the resize to reach, so its
    // refusal is the same word a terminal's and a socket's is.
    let pipes = pipe_with(PipeFlags::empty());
    match &pipes {
        Ok(p) => {
            r.check("the pipe pair is made", true);
            r.check(
                "a pipe's read end refuses ftruncate with EINVAL, as Linux does",
                ftrunc(&p.0, 4).err() == Some(Errno::INVAL),
            );
            r.check(
                "a pipe's write end refuses it with EINVAL too",
                ftrunc(&p.1, 4).err() == Some(Errno::INVAL),
            );
        }
        Err(_) => {
            r.check("the pipe pair is made", false);
            r.check(
                "a pipe's read end refuses ftruncate with EINVAL, as Linux does",
                false,
            );
            r.check("a pipe's write end refuses it with EINVAL too", false);
        }
    }
    drop(pipes);

    // The path spelling: a resize to zero, and the empty file reads as empty.
    r.check("truncate(path, 0) succeeds", truncate(PATH, 0).is_ok());
    r.check(
        "the emptied file is zero bytes long",
        size_of(&fd) == Some(0),
    );
    r.check(
        "the offset is rewound",
        seek(&fd, SeekFrom::Start(0)) == Ok(0),
    );
    let mut buf = [0u8; 16];
    r.check(
        "a read at offset 0 answers end-of-file",
        read(&fd, &mut buf) == Ok(0),
    );

    // A path that names nothing cannot be resized.
    r.check(
        "truncate on a missing path answers ENOENT",
        truncate(MISSING, 0).err() == Some(Errno::NOENT),
    );

    // The path spelling's own two answers for what is not a regular file, and
    // they are not the descriptor spelling's: Linux gives a directory `EISDIR`
    // and everything else that is not a regular file `EINVAL` (both measured
    // through `syscall(2)`, `.workbuddy/probe_fsize.c`; `ftruncate` gives
    // `EINVAL` for the directory, because its write-mode check answers first
    // and a directory cannot be opened for writing). The fixtures are a
    // directory and a device both payloads carry.
    r.check(
        "truncate on a directory answers EISDIR",
        truncate(DIR, 0).err() == Some(Errno::ISDIR),
    );
    r.check(
        "truncate on a character device answers EINVAL",
        truncate(NULL_DEV, 0).err() == Some(Errno::INVAL),
    );

    // The trap: the descriptor's cursor is not the file's length. Read to
    // end-of-file (offset 10), resize to zero, and write three bytes - they
    // land at offset 10, so the file is 13 bytes with a ten-byte hole.
    let _ = seek(&fd, SeekFrom::Start(0));
    let _ = write(&fd, b"0123456789");
    let _ = seek(&fd, SeekFrom::Start(0));
    let mut buf = [0u8; 16];
    r.check(
        "reading to end-of-file leaves the offset at 10",
        read(&fd, &mut buf) == Ok(10),
    );
    r.check("ftruncate to 0 succeeds", ftrunc(&fd, 0) == Ok(()));
    r.check(
        "a write after the resize lands at the unchanged offset",
        write(&fd, b"XYZ") == Ok(3),
    );
    r.check(
        "the file is 13 bytes: the hole plus the write",
        size_of(&fd) == Some(13),
    );
    let _ = seek(&fd, SeekFrom::Start(0));
    let mut buf = [0u8; 16];
    let n = read(&fd, &mut buf[..13]);
    r.check("the whole file reads back", n == Ok(13));
    r.check(
        "the ten-byte hole reads as zeros",
        n == Ok(13) && buf[..10].iter().all(|&b| b == 0),
    );
    r.check(
        "the write follows the hole",
        n == Ok(13) && buf[10..13] == *b"XYZ",
    );

    // ftruncate on a read-only descriptor is EINVAL, and the file is left
    // whole - the resize is a write, so a descriptor that may not write is
    // refused. Measured on the host: Linux answers EINVAL (not EBADF) and
    // does not change the size. Counterfactual: the old handler resized it
    // happily.
    let ro = openat(CWD, PATH, OFlags::RDONLY, Mode::empty());
    r.check("the file reopens read-only", ro.is_ok());
    if let Ok(ro) = &ro {
        let e = ftrunc(ro, 5).err();
        r.check(
            "ftruncate on an O_RDONLY fd answers EINVAL",
            e == Some(Errno::INVAL),
        );
        r.check(
            "the refused ftruncate leaves the size unchanged",
            size_of(ro) == Some(13),
        );
    } else {
        r.check("ftruncate on an O_RDONLY fd answers EINVAL", false);
        r.check("the refused ftruncate leaves the size unchanged", false);
    }
    // A write-opened descriptor is still resized, so the gate is the access
    // mode, not a blanket refusal.
    let wo = openat(CWD, PATH, OFlags::WRONLY, Mode::empty());
    r.check("the file reopens write-only", wo.is_ok());
    if let Ok(wo) = &wo {
        r.check(
            "ftruncate on an O_WRONLY fd succeeds",
            ftrunc(wo, 7).is_ok(),
        );
    } else {
        r.check("ftruncate on an O_WRONLY fd succeeds", false);
    }
    drop((ro, wo, fd));
    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}

/// The path spelling through libc: rustix names no `truncate` for a path at
/// all on this version, and the claim is the plain two-word call.
fn truncate(path: &str, len: i64) -> rustix::io::Result<()> {
    let c = std::ffi::CString::new(path).unwrap();
    if unsafe { libc::truncate(c.as_ptr(), len) } == 0 {
        Ok(())
    } else {
        Err(rustix::io::Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

/// `ftruncate` through rustix, whose typed length cannot spell the negative
/// word the ABI validates - that claim is the raw door above.
fn ftrunc<Fd: rustix::fd::AsFd>(fd: Fd, len: u64) -> rustix::io::Result<()> {
    rustix::fs::ftruncate(fd, len)
}
