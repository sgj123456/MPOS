//! t-link - a second name for one inode, and a name that stands for another.
//!
//! Pinned here, per link(2)/linkat(2): a hard link is a second directory
//! entry for the *same* inode - both names report one `st_ino`, `st_nlink`
//! rises to 2 for both, a read through either answers the same bytes, and
//! unlinking one leaves the other reading exactly what it read before while
//! `st_nlink` falls back to 1. A link to a name that does not exist is
//! ENOENT, and a link onto a name already taken is EEXIST. A missing source
//! is refused before anything is created, which is why ENOENT and not EPERM
//! is the answer - and before the new descriptor is looked at, which is this
//! call's own order: the pair of claims below tells "asked second" from "never
//! asked", where `renameat` reads both names and then resolves.
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
use rustix::fd::BorrowedFd;
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
/// A descriptor number nothing is open on, for the two claims about *when* a
/// `linkat` asks its new descriptor. `t-write`, `t-chdir` and the rest of the
/// suite write a number down the same way, because `BorrowedFd::borrow_raw`
/// refuses a negative raw fd by contract and one branch answers every number
/// that names nothing.
const BAD_DIRFD: i32 = 77;

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

    // And that refusal comes *before* the new descriptor is looked at, which is
    // this call's order rather than the family's: measured on a current host,
    // `linkat(dirfd, <missing>, <unopened fd>, "b")` is ENOENT - the old name's
    // walk is asked first - while the same shape through `renameat` is the bad
    // descriptor's EBADF, because that call reads both names and then resolves.
    // The second claim is what makes the first mean "asked second" rather than
    // "never asked at all": with a source that *does* exist, the same unopened
    // descriptor is reached.
    //
    // The new name is *relative* on purpose, and that is the rule this pair
    // stands next to: `B` above is absolute, and an absolute new name would stop
    // the new dirfd being a question at all - the claim would then be about the
    // path' shape instead of about the descriptor.
    let unopened = unsafe { BorrowedFd::borrow_raw(BAD_DIRFD) };
    const NEW_NAME: &str = "t-link-new-name";
    r.check(
        "a missing source is ENOENT before the new dirfd is asked",
        linkat(CWD, GONE, unopened, NEW_NAME, AtFlags::empty()) == Err(Errno::NOENT),
    );
    r.check(
        "a source that exists reaches the unopened new dirfd, which answers EBADF",
        linkat(CWD, A, unopened, NEW_NAME, AtFlags::empty()) == Err(Errno::BADF),
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

    // === `linkat(fd, "", …, AT_EMPTY_PATH)`: a name for a descriptor ========
    //
    // The one door that gives a file with **no name of its own** one, which is
    // what `apk` does with the repository index it wrote through `O_TMPFILE`
    // and what `shm_open`-style callers do. Measured on Linux 7.3.4
    // (`build/scratch/opathmmap/{p,q2,gaps}.c`):
    //
    //   * served for an **`O_PATH`** descriptor and for an ordinary one;
    //   * a **non-empty** name beside the flag is `ENOTDIR` (20) - the flag says
    //     the name is empty, so a caller that gave one is describing a walk its
    //     source is not;
    //   * without the flag an empty name is `ENOENT`;
    //   * `AT_SYMLINK_FOLLOW` may be combined with it;
    //   * an unknown flag bit is `EINVAL`, and a bad descriptor is `EBADF`.
    //
    // Raw, because `rustix::fs::linkat` takes `&CStr` and an `AtFlags` whose
    // vocabulary has no `AT_EMPTY_PATH` - the empty-name shape *is* the claim
    // (clause (c) again).
    //
    // The `CAP_DAC_READ_SEARCH` requirement the man page states is **not**
    // observed on a file the caller owns, so the claims below hold as root; a
    // pin that dropped privilege would be measuring the man page's sentence
    // rather than the kernel's behaviour.
    let empty_link = "/tmp/t-link-empty";
    let _ = std::fs::remove_file(empty_link);
    let target_file = "/tmp/t-link-at-empty-src";
    let _ = std::fs::write(target_file, b"second name");
    let e = |rc: i64| -> Option<i32> {
        (rc == -1)
            .then(|| std::io::Error::last_os_error().raw_os_error())
            .flatten()
    };
    match (
        rustix::fs::open(target_file, OFlags::RDWR, Mode::empty()),
        rustix::fs::open(target_file, OFlags::PATH, Mode::empty()),
    ) {
        (Ok(ordinary), Ok(path_only)) => {
            let ord = std::os::fd::AsRawFd::as_raw_fd(&ordinary);
            let op = std::os::fd::AsRawFd::as_raw_fd(&path_only);
            // SAFETY: every descriptor is live and the two names are
            // NUL-terminated strings this test owns.
            let made = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    ord,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            r.check(
                &format!("linkat(ordinary fd, empty name, AT_EMPTY_PATH) is served, got {made}"),
                made == 0,
            );
            r.check(
                "and the new name is the same inode",
                std::fs::metadata(empty_link)
                    .ok()
                    .zip(std::fs::metadata(target_file).ok())
                    .is_some_and(|(a, b)| {
                        use std::os::unix::fs::MetadataExt;
                        a.ino() == b.ino() && a.nlink() == 2
                    }),
            );
            let _ = std::fs::remove_file(empty_link);
            // The `O_PATH` spelling: the descriptor that exists to *name* a file
            // is the one this door is most used with.
            // SAFETY: as above.
            let via_path = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    op,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            r.check(
                &format!("linkat(O_PATH fd, empty name, AT_EMPTY_PATH) is served, got {via_path}"),
                via_path == 0,
            );
            let _ = std::fs::remove_file(empty_link);
            // A name beside the flag is ENOTDIR: the flag says there is none.
            // SAFETY: as above.
            let nonempty = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    ord,
                    c"x".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            r.check(
                &format!(
                    "a non-empty name beside AT_EMPTY_PATH is ENOTDIR, got {}",
                    e(nonempty)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| nonempty.to_string())
                ),
                e(nonempty) == Some(libc::ENOTDIR),
            );
            // Without the flag the empty name names nothing.
            // SAFETY: as above.
            let unflagged = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    ord,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    0,
                )
            };
            r.check(
                &format!(
                    "an empty name without AT_EMPTY_PATH is ENOENT, got {}",
                    e(unflagged)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| unflagged.to_string())
                ),
                e(unflagged) == Some(libc::ENOENT),
            );
            // An unknown flag bit is EINVAL, and it is judged before the names.
            // SAFETY: as above.
            let badflag = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    ord,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    0x4000_0000i64,
                )
            };
            r.check(
                &format!(
                    "an unknown linkat flag bit is EINVAL, got {}",
                    e(badflag)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| badflag.to_string())
                ),
                e(badflag) == Some(libc::EINVAL),
            );
            // A descriptor the table does not hold is EBADF, and it outranks
            // the empty name.
            // SAFETY: as above; -1 names no descriptor.
            let badfd = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    -1i32,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            r.check(
                &format!(
                    "linkat(-1, empty name, AT_EMPTY_PATH) is EBADF, got {}",
                    e(badfd)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| badfd.to_string())
                ),
                e(badfd) == Some(libc::EBADF),
            );
            // The pair that makes the above about the flag rather than about
            // "this door refuses empty names": `AT_SYMLINK_FOLLOW` may ride
            // along with it.
            // SAFETY: as above.
            let with_follow = unsafe {
                libc::syscall(
                    libc::SYS_linkat,
                    ord,
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    c"/tmp/t-link-empty".as_ptr(),
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_FOLLOW,
                )
            };
            r.check(
                &format!("AT_EMPTY_PATH|AT_SYMLINK_FOLLOW is served, got {with_follow}"),
                with_follow == 0,
            );
        }
        _ => r.check(
            "the two descriptors for the AT_EMPTY_PATH claims are opened",
            false,
        ),
    }
    let _ = std::fs::remove_file(empty_link);
    let _ = std::fs::remove_file(target_file);

    // === a dirfd-relative link: both names in one directory =================
    //
    // Every claim above passes `CWD` with an **absolute** name, so the dirfd
    // spelling was never asked - and it was broken: measured on the guest,
    // `linkat(dirfd, "src", dirfd, "hard")` answered **`EXDEV`** while the
    // absolute-path spelling of the same link was served. The cause was two
    // paths composed from two different bases: the source from its dirfd, the
    // destination from the **cwd** (`absolute()`), so `parent_path == "."`
    // resolved to `/` and the comparison saw two filesystems. Item 298's bug
    // one door over.
    //
    // Both spellings are claimed - the ordinary descriptor and `O_PATH` -
    // because the fix had to compose the destination from *its* dirfd for
    // either to work, and the `O_PATH` one is the flag's own largest use.
    let dirdir = "/tmp/t-link-dirfd";
    let _ = std::fs::remove_dir_all(dirdir);
    if std::fs::create_dir_all(dirdir).is_ok()
        && std::fs::write(format!("{dirdir}/src"), b"x").is_ok()
    {
        for (label, path_flags) in [
            ("ordinary", OFlags::RDONLY | OFlags::DIRECTORY),
            ("O_PATH", OFlags::PATH),
        ] {
            if let Ok(dfd) = rustix::fs::open(dirdir, path_flags, Mode::empty()) {
                let raw = std::os::fd::AsRawFd::as_raw_fd(&dfd);
                let name = format!("hard-{}", label.to_lowercase().replace("_", "-"));
                let cname = std::ffi::CString::new(name.as_str()).unwrap();
                // SAFETY: `raw` is a live directory descriptor and both names
                // are NUL-terminated strings this test owns.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_linkat,
                        raw,
                        c"src".as_ptr(),
                        raw,
                        cname.as_ptr(),
                        0,
                    )
                };
                r.check(
                    &format!("linkat({label} dirfd, src, same dirfd, {name}) is served, got {rc}"),
                    rc == 0,
                );
                r.check(
                    &format!("and the {label}-dirfd link is the same inode"),
                    std::fs::metadata(format!("{dirdir}/{name}"))
                        .ok()
                        .zip(std::fs::metadata(format!("{dirdir}/src")).ok())
                        .is_some_and(|(a, b)| {
                            use std::os::unix::fs::MetadataExt;
                            a.ino() == b.ino()
                        }),
                );
            } else {
                r.check(
                    &format!("a {label} directory descriptor for the linkat claims is opened"),
                    false,
                );
            }
        }
    } else {
        r.check("the dirfd fixture directory is made", false);
    }
    let _ = std::fs::remove_dir_all(dirdir);

    for path in [A, B, S, S2] {
        let _ = std::fs::remove_file(path);
    }
    r.finish()
}
