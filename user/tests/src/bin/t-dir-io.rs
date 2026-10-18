//! t-dir-io — a directory is not a byte stream, and *which* door says so.
//!
//! `cat` on a directory used to answer `EOPNOTSUPP` here, and that is a
//! different sentence from Linux's. Measured on a current host first
//! (`.workbuddy/probe_dir_io.c`, Linux 7.2.9, one line per door — the whole
//! table, because the two cases `docs/uapi/` §5.2 carried were slices of it):
//!
//!   * `read(dir, buf, 16)` is **`EISDIR`** (21), and so are `read(dir, buf, 0)`,
//!     `pread64`, `readv` and `preadv` — the *kind* is asked before the count, so
//!     a zero-length read of a directory is refused where the same zero-length
//!     read of a file answers 0;
//!   * `sendfile(out, dir, NULL, 16)` is **`EINVAL`** (22) — the copy goes
//!     through the *source's* splice path, and a directory has none, so the
//!     caller hears "this source cannot be sent" rather than "that is a
//!     directory";
//!   * `sendfile(out, dir, NULL, 0)` is **0**: that call's own count is asked
//!     before its kind, which is why the kernel's answer is not the read's.
//!
//! Linux's owner for the read half is the directory's own file operations
//! (`read = generic_read_dir`, which returns `-EISDIR`), so the rule is the
//! kind's and one door can carry it — `read`, `pread64`, `readv` and `preadv`
//! all arrive at a description's read. `EOPNOTSUPP` reads as "this kind of file
//! cannot be read that way"; `EISDIR` reads as "you named a directory", and a
//! program that probes a path with a read branches on the two differently
//! (`AGENTS.md` §3's rule 2).
//!
//! What this kernel did *not* get wrong, and what the claims below therefore
//! hold as controls: the write half, which has no `EISDIR` of its own to give
//! because a directory cannot be opened writable at all (`open(dir, O_RDWR)`
//! and `O_WRONLY` are both `EISDIR`, refused where the description is made —
//! the write a caller can reach is the read-only description's `EBADF`), and
//! `copy_file_range`/`splice`, which ask the kind first already.
//!
//! The controls are a regular file this test makes itself under `/tmp` — which
//! both machines' payloads carry, `t-openat`'s reason — so a red claim below is
//! the kind and not the fixture.

use mpos_user_tests::Report;
use rustix::fs::{CWD, Mode, OFlags, openat, sendfile};
use rustix::io::{Errno, IoSliceMut, pread, preadv, read, readv, write};

/// The directory under test: one both machines' payloads carry, since the claim
/// is about the *kind* rather than about which directory it is.
const DIR: &str = "/tmp";
/// The regular file this test writes, for the controls.
const FILE: &str = "/tmp/t-dir-io_file";
/// An output that swallows whatever is copied into it.
const SINK: &str = "/dev/null";

/// The errno of a call that was meant to fail, or `None` if it answered — so a
/// claim prints `FAIL` instead of panicking and taking the rest of the program
/// with it.
fn refused<T>(call: Result<T, Errno>) -> Option<Errno> {
    match call {
        Ok(_) => None,
        Err(e) => Some(e),
    }
}

fn main() {
    let mut r = Report::new();

    // ---- the fixtures ------------------------------------------------------
    let _ = std::fs::remove_file(FILE);
    let Ok(file) = openat(CWD, FILE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o644)) else {
        r.check("this test can make the file its controls need", false);
        r.finish()
    };
    let Ok(dir) = openat(CWD, DIR, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()) else {
        r.check("the directory under test opens read-only", false);
        r.finish()
    };
    let Ok(sink) = openat(CWD, SINK, OFlags::WRONLY, Mode::empty()) else {
        r.check("the sink opens", false);
        r.finish()
    };

    let mut buf = [0u8; 16];
    // A buffer with no room in it, named rather than written inline: the claim
    // is the *count* the call is made with, and an empty slice literal leaves
    // its element type ambiguous.
    let mut none = [0u8; 0];

    // ---- the read doors, which is what `cat` on a directory reaches --------
    r.check(
        "read on a directory is EISDIR",
        refused(read(&dir, &mut buf)) == Some(Errno::ISDIR),
    );
    r.check(
        "and with no room asked for, EISDIR too",
        refused(read(&dir, &mut none)) == Some(Errno::ISDIR),
    );
    r.check(
        "pread64 on a directory is EISDIR",
        refused(pread(&dir, &mut buf, 0)) == Some(Errno::ISDIR),
    );
    let mut iov = [IoSliceMut::new(&mut buf)];
    r.check(
        "readv on a directory is EISDIR",
        refused(readv(&dir, &mut iov)) == Some(Errno::ISDIR),
    );
    let mut iov = [IoSliceMut::new(&mut buf)];
    r.check(
        "preadv on a directory is EISDIR",
        refused(preadv(&dir, &mut iov, 0)) == Some(Errno::ISDIR),
    );

    // ---- the copy door -----------------------------------------------------
    r.check(
        "sendfile from a directory is EINVAL, not the read's EISDIR",
        refused(sendfile(&sink, &dir, None, 16)) == Some(Errno::INVAL),
    );
    r.check(
        "and for no bytes at all it is 0",
        matches!(sendfile(&sink, &dir, None, 0), Ok(0)),
    );

    // ---- the write half, and why it has no EISDIR to give ------------------
    r.check(
        "opening a directory for writing is EISDIR",
        refused(openat(CWD, DIR, OFlags::RDWR, Mode::empty())) == Some(Errno::ISDIR),
    );
    r.check(
        "...and for write-only too",
        refused(openat(CWD, DIR, OFlags::WRONLY, Mode::empty())) == Some(Errno::ISDIR),
    );
    r.check(
        "so the write a caller can reach is the read-only description's EBADF",
        refused(write(&dir, b"x")) == Some(Errno::BADF),
    );

    // ---- the controls: the same doors against a regular file ---------------
    r.check(
        "a regular file takes the bytes",
        matches!(write(&file, b"abc"), Ok(3)),
    );
    r.check(
        "and pread answers them back",
        matches!(pread(&file, &mut buf, 0), Ok(3)) && &buf[..3] == b"abc",
    );
    r.check(
        "its zero-length read is 0, which is the answer a directory refuses",
        matches!(read(&file, &mut none), Ok(0)),
    );
    r.check(
        "and sendfile copies from it",
        matches!(sendfile(&sink, &file, Some(&mut 0u64), 3), Ok(3)),
    );

    // The descriptions stay open to here on purpose: every claim above is about
    // a *live* description of each kind.
    drop(dir);
    drop(sink);
    drop(file);
    let _ = std::fs::remove_file(FILE);

    r.finish()
}
