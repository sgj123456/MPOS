//! t-newfstatat - one test for newfstatat, the path-based stat the strace
//! uses on `/etc/ld.so.cache` before opening it.
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path, stat
//! buffer, flags - regular files and directories reported through the machine's
//! own `struct stat`, and ENOENT for a path that does not exist. The
//! `AT_EMPTY_PATH` form stats the descriptor itself and must agree with `fstat`;
//! without the flag an empty path is just an empty name (ENOENT); paths resolve
//! relative to a directory descriptor, and a dirfd that is not open answers
//! EBADF.
//!
//! # The fixture is this test's own file now
//!
//! The no-std version statted `/etc/motd`, and the second machine's payload has
//! no motd (`NOTES.md` §15.11, measured with `debugfs`), so this test writes its
//! own file under `/tmp`. `/bin` and `/bin/sh` stay: they are the directory and
//! the program both payloads carry - measured, on the second machine's staging
//! tree, as a flat `/bin` of six regular files and a 479176-byte static ELF.
//!
//! # The struct is `rustix`'s
//!
//! The no-std version declared `lin::Stat`, which is the 144-byte x86_64
//! `struct stat` and the wrong size on the other machine. Nothing here says
//! `struct stat` at all: `rustix::fs::statat` answers a `Stat` that is *this*
//! machine's, `FileType::from_raw_mode` reads the mode word for the type, and
//! the `AT_EMPTY_PATH` claim is expressed by comparing rustix's own answer
//! against rustix's `fstat` - two spellings of one description, which is what
//! the claim is about. The *length* of the buffer is `t-stat-buffer`'s subject
//! (§13.10).
//!
//! # The one number this program writes down, and why
//!
//! The bad-dirfd claim needs a descriptor number nothing is open on, and the
//! contract of `BorrowedFd::borrow_raw` is that its argument *is* open - so any
//! such number is written down rather than derived. `77` is the no-std
//! version's, and `t-write`, `t-chdir` and the rest of the suite use the same
//! convention (`999` there) because the kernel's answer is one branch for every
//! number that names nothing. It is not a number the suite's assertions depend
//! on: the claim is EBADF, which any unopened descriptor gives.
//!
//! # The no-std version's bad-dirfd claim was not Linux's, and its path is why
//!
//! It statted `/etc/motd` - an **absolute** path - with a dirfd nothing is open
//! on, and this kernel answered EBADF. Linux does not: the dirfd is ignored
//! entirely when the path is absolute, so `newfstatat(77, "/tmp", ..)` answers
//! `0` with errno `0` there (measured on a current host). The claim below
//! therefore names the kernel's answer with a *relative* path, which is EBADF
//! on both, and the absolute-path behaviour is a divergence of this kernel's -
//! the same thing would happen through `openat`, `unlinkat` and the rest of the
//! family, because one `base_of` resolves the dirfd for all of them - named in
//! `NOTES.md` §15.17 rather than pinned here. Fixing it is not this batch's: it
//! is twenty call sites and every one of them would need its own answer.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fstat, openat, statat};
use rustix::io::Errno;

/// The file this test owns, in place of the no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-newfstatat_file";
/// A directory both payloads carry, and the program inside it that the
/// dirfd-relative claim stats.
const DIR: &str = "/bin";
const SH: &str = "sh";
/// A name nothing ever makes.
const GONE: &str = "/no/such";
/// A descriptor number nothing is open on - see the module comment.
const BAD_DIRFD: i32 = 77;
/// The name the bad-dirfd claim hands the kernel. What it says does not matter
/// - the descriptor is not open - but its *shape* does: it is relative, because
/// an absolute path makes Linux ignore the dirfd altogether. See the claim.
const ANY_NAME: &str = "any";

/// The bytes the fixture holds, so "a regular file with contents" is a claim
/// about a size this test made rather than about whatever an image shipped.
const BODY: &[u8] = b"mpos t-newfstatat fixture\n";

fn main() {
    println!("t-newfstatat");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, BODY).is_err() {
        r.check("the fixture can be written", false);
        r.finish()
    }

    match statat(CWD, FILE, AtFlags::empty()) {
        Ok(st) => {
            r.check("stat of the fixture succeeds", true);
            r.check(
                "it is a regular file with contents",
                FileType::from_raw_mode(st.st_mode).is_file() && st.st_size > 0,
            );
        }
        Err(_) => {
            r.check("stat of the fixture succeeds", false);
            r.check("it is a regular file with contents", false);
        }
    }

    match statat(CWD, DIR, AtFlags::empty()) {
        Ok(st) => {
            r.check("stat of /bin succeeds", true);
            r.check(
                "it is a directory",
                FileType::from_raw_mode(st.st_mode).is_dir(),
            );
        }
        Err(_) => {
            r.check("stat of /bin succeeds", false);
            r.check("it is a directory", false);
        }
    }

    r.check(
        "a path that does not exist answers ENOENT",
        matches!(statat(CWD, GONE, AtFlags::empty()), Err(Errno::NOENT)),
    );

    // AT_EMPTY_PATH: an empty path names the descriptor itself, and what it
    // reports must be what fstat reports for the same descriptor.
    match openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => {
            r.check("the file to stat by fd opens", true);
            match (fstat(&fd), statat(&fd, "", AtFlags::EMPTY_PATH)) {
                (Ok(by_fd), Ok(by_path)) => {
                    r.check("fstat on the same fd succeeds", true);
                    r.check("an empty path with AT_EMPTY_PATH succeeds", true);
                    r.check(
                        "the AT_EMPTY_PATH size matches fstat's",
                        by_path.st_size == by_fd.st_size,
                    );
                }
                (Ok(_), Err(_)) => {
                    r.check("fstat on the same fd succeeds", true);
                    r.check("an empty path with AT_EMPTY_PATH succeeds", false);
                    r.check("the AT_EMPTY_PATH size matches fstat's", false);
                }
                (Err(_), _) => {
                    r.check("fstat on the same fd succeeds", false);
                    r.check("an empty path with AT_EMPTY_PATH succeeds", false);
                    r.check("the AT_EMPTY_PATH size matches fstat's", false);
                }
            }
            r.check(
                "an empty path without AT_EMPTY_PATH answers ENOENT",
                matches!(statat(&fd, "", AtFlags::empty()), Err(Errno::NOENT)),
            );
        }
        Err(e) => {
            r.check(&format!("the file to stat by fd opens (got {e})"), false);
            r.check("fstat on the same fd succeeds", false);
            r.check("an empty path with AT_EMPTY_PATH succeeds", false);
            r.check("the AT_EMPTY_PATH size matches fstat's", false);
            r.check("an empty path without AT_EMPTY_PATH answers ENOENT", false);
        }
    }

    // A path relative to a directory descriptor.
    match openat(CWD, DIR, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()) {
        Ok(dirfd) => {
            r.check("/bin opens for a dirfd-relative stat", true);
            match statat(&dirfd, SH, AtFlags::empty()) {
                Ok(st) => {
                    r.check("newfstatat relative to a dirfd succeeds", true);
                    r.check(
                        "the dirfd-relative stat says regular file",
                        FileType::from_raw_mode(st.st_mode).is_file(),
                    );
                }
                Err(_) => {
                    r.check("newfstatat relative to a dirfd succeeds", false);
                    r.check("the dirfd-relative stat says regular file", false);
                }
            }
        }
        Err(e) => {
            r.check(
                &format!("/bin opens for a dirfd-relative stat (got {e})"),
                false,
            );
            r.check("newfstatat relative to a dirfd succeeds", false);
            r.check("the dirfd-relative stat says regular file", false);
        }
    }

    // A dirfd that is not open answers EBADF. The path is *relative* on
    // purpose, and that is worth spelling out because the no-std version's was
    // absolute: Linux ignores the dirfd entirely when the path is absolute -
    // measured, `newfstatat(77, "/tmp", ..)` answers 0 with errno 0 - so an
    // absolute path would make this claim about the path and not about the
    // dirfd. This kernel answers EBADF either way, which is a divergence of its
    // own, named in `NOTES.md` §15.17 rather than pinned here.
    // SAFETY: a descriptor number nothing is open on - the kernel answers EBADF,
    // which is the claim.
    let missing = unsafe { BorrowedFd::borrow_raw(BAD_DIRFD) };
    r.check(
        "a bad dirfd answers EBADF",
        matches!(
            statat(missing, ANY_NAME, AtFlags::empty()),
            Err(Errno::BADF)
        ),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
