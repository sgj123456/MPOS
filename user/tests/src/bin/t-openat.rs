//! t-openat - one test for openat, the strace's file door: the dynamic
//! loader probes /etc/ld.so.cache with it and static binaries open their files
//! through it too (`openat(AT_FDCWD, path, O_RDONLY|O_CLOEXEC)`).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path, flags,
//! mode, with no length word; AT_FDCWD relative opens; real-dirfd opens (EBADF
//! for a dirfd that is not open and ENOTDIR for one that is not a directory,
//! both with a *relative* name - an absolute one does not consult the
//! descriptor at all, and the same two claims pin that half); the
//! lowest-free-fd
//! guarantee, measured rather than assumed to be 3; O_CREAT making a missing file
//! and O_CREAT|O_EXCL answering EEXIST for one that is there; close and its fd
//! reuse; ENOENT for a path that does not exist; and O_NOFOLLOW refusing a
//! symlink's final component with ELOOP while the same open without it still
//! follows.
//!
//! The "existing file" is one this test writes itself under `/tmp`, not the
//! no-std version's `/etc/motd`: the second machine's payload carries `/tmp` and
//! no motd at all (`NOTES.md` §15.11), and the claim is about `openat`, not about
//! which file it is opening. `/bin/sh` is the one path this test reads that it
//! does not create itself, and both machines carry it as a static executable -
//! measured on the second machine's disk with `debugfs` before the move (479176
//! bytes, ELF magic), because "the image starts with ELF magic" is a claim about a
//! real program rather than about a fixture this test just made.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat, symlink, unlinkat};
use rustix::io::{Errno, dup, read};

/// The file this test owns, in place of the no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-openat_file";
/// Made by `O_CREAT` below, and the target the symlink points at.
const NEW: &str = "/tmp/t-openat_new";
const LINK: &str = "/tmp/t-openat_link";
/// The one path this test reads but does not create: a directory and the
/// executable inside it, which both machines' payloads carry.
const BIN: &str = "/bin";
const SH: &str = "sh";
const NO_SUCH: &str = "/no/such";
/// A name that is not in any directory, for the two claims that need a
/// *relative* one: the bad-dirfd claim and the ENOTDIR one. An absolute path
/// stops the dirfd being a question at all, so it cannot spell either.
const NOT_DIR: &str = "x";

fn main() {
    println!("t-openat");
    let mut r = Report::new();

    // The standard descriptors are open by the runner's own contract, and the
    // lowest free one is discovered rather than assumed to be 3.
    let stdout = unsafe { BorrowedFd::borrow_raw(1) };
    let Ok(probe) = dup(stdout) else {
        r.check("openat(AT_FDCWD) hands out the lowest free fd", false);
        r.finish()
    };
    let base = probe.as_raw_fd();
    drop(probe);

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, b"mpos t-openat fixture\n").is_err() {
        r.check("openat(AT_FDCWD) hands out the lowest free fd", false);
        r.finish()
    }

    let Ok(fd) = openat(CWD, FILE, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()) else {
        r.check("openat(AT_FDCWD) hands out the lowest free fd", false);
        r.finish()
    };
    r.check(
        "openat(AT_FDCWD) hands out the lowest free fd",
        fd.as_raw_fd() == base,
    );

    // `rustix::io::close` takes a raw fd and returns nothing, so the "close
    // answers 0" claim is `libc::close`'s - the clause `t-dup`, `t-dup2` and
    // `t-dup3` share (`user/tests/Cargo.toml`). The ownership is taken back out
    // of the `OwnedFd` first, so nothing closes the number a second time.
    let raw = fd.into_raw_fd();
    r.check(
        "close of the open fd succeeds",
        unsafe { libc::close(raw) } == 0,
    );

    let Ok(fd) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("the closed fd is reused for the next open", false);
        r.finish()
    };
    r.check(
        "the closed fd is reused for the next open",
        fd.as_raw_fd() == base,
    );
    drop(fd);

    r.check(
        "a path that does not exist answers ENOENT",
        matches!(
            openat(CWD, NO_SUCH, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NOENT)
        ),
    );

    // A real dirfd: /bin names a directory, and "sh" resolves inside it.
    let Ok(binfd) = openat(CWD, BIN, OFlags::RDONLY, Mode::empty()) else {
        r.check("open /bin gives a directory fd", false);
        r.finish()
    };
    r.check("open /bin gives a directory fd", binfd.as_raw_fd() == base);

    let Ok(shfd) = openat(&binfd, SH, OFlags::RDONLY, Mode::empty()) else {
        r.check("openat(binfd, \"sh\") opens", false);
        r.finish()
    };
    r.check("openat(binfd, \"sh\") opens", shfd.as_raw_fd() == base + 1);

    let mut buf = [0u8; 64];
    let Ok(n) = read(&shfd, &mut buf) else {
        r.check("the sh image reads back bytes", false);
        r.finish()
    };
    r.check("the sh image reads back bytes", n > 0);
    r.check(
        "the sh image starts with ELF magic",
        n >= 4 && buf[0] == 0x7F && buf[1] == b'E' && buf[2] == b'L' && buf[3] == b'F',
    );
    drop(shfd);
    drop(binfd);

    // A number nothing is open on. The no-std version used descriptor 77, and
    // the number is not part of the claim: 999 is the one `t-read`/`t-write` use,
    // because `BorrowedFd::borrow_raw` refuses a negative raw fd by contract and a
    // closed number is the same kernel branch - the descriptor table has no such
    // row.
    //
    // The name is *relative* on purpose: an absolute path does not reach the
    // descriptor at all, so an absolute one would make this claim about the path
    // rather than about the descriptor. The claim after it is that half, and
    // `t-newfstatat` pins the same pair where the divergence was found.
    let missing = unsafe { BorrowedFd::borrow_raw(999) };
    r.check(
        "a dirfd that is not open answers EBADF",
        matches!(
            openat(missing, NOT_DIR, OFlags::RDONLY, Mode::empty()),
            Err(Errno::BADF)
        ),
    );
    r.check(
        "an absolute path with a bad dirfd opens",
        openat(missing, FILE, OFlags::RDONLY, Mode::empty()).is_ok(),
    );

    // A dirfd that names a regular file is not a directory to resolve in - for
    // a relative name. An absolute one is served through it, because the
    // descriptor's *kind* stops being a question too.
    let Ok(fd) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("a dirfd that names a regular file answers ENOTDIR", false);
        r.finish()
    };
    r.check(
        "a dirfd that names a regular file answers ENOTDIR",
        matches!(
            openat(&fd, NOT_DIR, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NOTDIR)
        ),
    );
    r.check(
        "an absolute path through a regular-file dirfd opens",
        openat(&fd, FILE, OFlags::RDONLY, Mode::empty()).is_ok(),
    );
    drop(fd);

    let _ = std::fs::remove_file(NEW);
    let Ok(fd) = openat(CWD, NEW, OFlags::CREATE | OFlags::RDWR, Mode::from(0o644)) else {
        r.check("O_CREAT makes the missing file", false);
        r.finish()
    };
    r.check("O_CREAT makes the missing file", fd.as_raw_fd() == base);
    r.check(
        "O_CREAT|O_EXCL on the made file answers EEXIST",
        matches!(
            openat(
                CWD,
                NEW,
                OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
                Mode::from(0o644)
            ),
            Err(Errno::EXIST)
        ),
    );
    drop(fd);

    // ---- O_NOFOLLOW is honoured ----
    //
    // A symlink to the file made above: O_NOFOLLOW must refuse the final
    // component with ELOOP, while the same open without it still follows. The old
    // handler defined no O_NOFOLLOW, so the flagged open followed the link and
    // answered success - a dropped security guarantee.
    let _ = unlinkat(CWD, LINK, AtFlags::empty());
    r.check(
        "the symlink to the made file is created",
        symlink(NEW, LINK) == Ok(()),
    );
    r.check(
        "openat on a symlink with O_NOFOLLOW answers ELOOP",
        matches!(
            openat(CWD, LINK, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()),
            Err(Errno::LOOP)
        ),
    );
    r.check(
        "the same open without O_NOFOLLOW still follows the link",
        openat(CWD, LINK, OFlags::RDONLY, Mode::empty()).is_ok(),
    );
    r.check(
        "O_NOFOLLOW on a regular file still opens it",
        openat(CWD, NEW, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).is_ok(),
    );

    let _ = unlinkat(CWD, LINK, AtFlags::empty());
    let _ = unlinkat(CWD, NEW, AtFlags::empty());
    let _ = std::fs::remove_file(FILE);
    r.finish()
}
