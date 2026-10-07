//! t-link - a second name for one inode, and a name that stands for another.
//!
//! Pinned here, per link(2)/linkat(2): a hard link is a second directory
//! entry for the *same* inode - both names report one `st_ino`, `st_nlink`
//! rises to 2 for both, a read through either answers the same bytes, and
//! unlinking one leaves the other reading exactly what it read before while
//! `st_nlink` falls back to 1. A link to a name that does not exist is
//! ENOENT, and a link onto a name already taken is EEXIST. A missing source
//! is refused before anything is created, which is why ENOENT and not EPERM
//! is the answer.
//!
//! And per symlink(2)/readlink(2): the target is *stored, not checked*, so a
//! link to a path that does not exist is made happily; `readlink` hands back
//! exactly the bytes of the target with no NUL and no termination, which is
//! the whole of the call; `newfstatat(AT_SYMLINK_NOFOLLOW)` sees S_IFLNK
//! where a following stat would see the target's kind; and a link onto a
//! name already taken is EEXIST.
//!
//! # What the two machines reach
//!
//! Both `link`-family and `symlink`-family calls here are served on both
//! machines, and two of them are worth saying out loud because they are not the
//! spellings the no-std version used: `readlink` and `symlink` have **no arm64
//! number** (`nr_aarch64.rs`: `READLINK = 64015`, `SYMLINK = 64016`, both
//! "unreachable on aarch64"), so musl implements them over `readlinkat`/
//! `symlinkat` there - and `rustix::fs::readlink`/`symlink` are written that way
//! on *every* machine, measured in its linux_raw backend, which issues
//! `__NR_readlinkat`/`__NR_symlinkat` with `AT_FDCWD` unconditionally. So unlike
//! the no-std version, whose `READLINK`/`SYMLINK` calls reached those numbers on
//! the first machine, this program pins one handler on both - which is what the
//! per-claim diff requires.
//!
//! The sizes are read with `rustix::fs::statat`, not with the no-std suite's
//! `lin::Stat`: that struct is the x86_64 `struct stat`, and the second
//! machine's is a different shape. `st_ino` and `st_nlink` are what the claims
//! need, and rustix's `Stat` is the machine's own - as is `FileType`, which the
//! S_IFLNK claim reads instead of masking the mode word itself.
//!
//! # One divergence, recorded where it would be visible
//!
//! `st_nlink` for a file whose *last* name has been unlinked but whose
//! descriptor is still open reads 1, not Linux's 0. The count lives on the node
//! and the trait's zero means "does not track one" - see `RamFile`'s `links`
//! field - so an unlinked-but-open file is the one case the shared sentinel
//! cannot say. This program does not test it; it is written down here because
//! the claims below are where a reader would look for it.

use mpos_user_tests::Report;
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, linkat, openat, readlinkat_raw, statat, symlink,
    symlinkat, unlinkat,
};
use rustix::io::{Errno, read, write};

/// The names this test owns: the source file, its hard link, the two symlinks,
/// and a name nothing ever makes.
const A: &str = "/tmp/t-link-a";
const B: &str = "/tmp/t-link-b";
const S: &str = "/tmp/t-link-s";
const S2: &str = "/tmp/t-link-s2";
const GONE: &str = "/tmp/t-link-gone";

/// The four bytes the source and every name for it hold, read through `path`.
/// One claim, always made - a name that will not open is the claim failing, not
/// the claim going away.
fn reads_link(r: &mut Report, path: &str, name: &str) {
    let mut buf = [0u8; 8];
    let ok = match openat(CWD, path, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => matches!(read(&fd, &mut buf[..4]), Ok(4)) && buf[..4] == *b"LINK",
        Err(_) => false,
    };
    r.check(name, ok);
}

fn main() {
    println!("t-link");
    let mut r = Report::new();

    for path in [A, B, S, S2] {
        let _ = std::fs::remove_file(path);
    }

    match openat(
        CWD,
        A,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from(0o644),
    ) {
        Ok(fd) => {
            r.check("the source file is created", true);
            r.check("it holds LINK", write(&fd, b"LINK") == Ok(4));
        }
        Err(_) => {
            r.check("the source file is created", false);
            r.check("it holds LINK", false);
        }
    }

    // ---- the hard link -----------------------------------------------------
    r.check(
        "linkat makes a second name for the same inode",
        linkat(CWD, A, CWD, B, AtFlags::empty()) == Ok(()),
    );
    let at = statat(CWD, A, AtFlags::empty());
    let bt = statat(CWD, B, AtFlags::empty());
    r.check("fstatat on the source succeeds", at.is_ok());
    r.check("fstatat on the link succeeds", bt.is_ok());
    match (&at, &bt) {
        (Ok(at), Ok(bt)) => {
            r.check("both names report the same st_ino", bt.st_ino == at.st_ino);
            r.check("both names report st_nlink 2", bt.st_nlink == 2);
            r.check("and so does the source", at.st_nlink == 2);
        }
        _ => {
            r.check("both names report the same st_ino", false);
            r.check("both names report st_nlink 2", false);
            r.check("and so does the source", false);
        }
    }
    reads_link(&mut r, B, "the link reads the same bytes as the source");

    // Dropping one name leaves the other reading what it read.
    r.check(
        "unlinking the second name succeeds",
        unlinkat(CWD, B, AtFlags::empty()) == Ok(()),
    );
    reads_link(&mut r, A, "the source still reads LINK");
    match statat(CWD, A, AtFlags::empty()) {
        Ok(st) => {
            r.check("fstatat on the source still succeeds", true);
            r.check("its st_nlink is back to 1", st.st_nlink == 1);
        }
        Err(_) => {
            r.check("fstatat on the source still succeeds", false);
            r.check("its st_nlink is back to 1", false);
        }
    }

    // The two refusals, in the order the handler makes them.
    r.check(
        "a link from a missing source answers ENOENT",
        linkat(CWD, GONE, CWD, B, AtFlags::empty()) == Err(Errno::NOENT),
    );
    r.check(
        "the second name is made again",
        linkat(CWD, A, CWD, B, AtFlags::empty()) == Ok(()),
    );
    r.check(
        "a link onto a name already taken answers EEXIST",
        linkat(CWD, A, CWD, B, AtFlags::empty()) == Err(Errno::EXIST),
    );

    // ---- the symbolic link -------------------------------------------------
    r.check(
        "symlink stores a target that need not exist",
        symlink("nowhere", S) == Ok(()),
    );
    let mut target = [0u8; 16];
    let n = readlinkat_raw(CWD, S, &mut target[..]);
    r.check("readlink answers the target's length", n == Ok(7));
    r.check(
        "readlink answers exactly those bytes, with no NUL",
        target[..7] == *b"nowhere",
    );
    match statat(CWD, S, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            r.check("fstatat with AT_SYMLINK_NOFOLLOW succeeds", true);
            r.check(
                "the name without following is a link",
                FileType::from_raw_mode(st.st_mode).is_symlink(),
            );
        }
        Err(_) => {
            r.check("fstatat with AT_SYMLINK_NOFOLLOW succeeds", false);
            r.check("the name without following is a link", false);
        }
    }

    r.check(
        "symlinkat makes a link at the dirfd's own spelling",
        symlinkat("elsewhere", CWD, S2) == Ok(()),
    );
    let mut target2 = [0u8; 16];
    let n2 = readlinkat_raw(CWD, S2, &mut target2[..]);
    r.check("readlinkat answers the target's length", n2 == Ok(9));
    r.check(
        "readlinkat answers exactly those bytes",
        target2[..9] == *b"elsewhere",
    );

    r.check(
        "a symlink onto a name already taken answers EEXIST",
        symlink("another", S) == Err(Errno::EXIST),
    );

    for path in [A, B, S, S2] {
        let _ = std::fs::remove_file(path);
    }
    r.finish()
}
