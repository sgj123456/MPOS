//! t-newfstatat - one test for newfstatat, the path-based stat the strace
//! uses on `/etc/ld.so.cache` before opening it.
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path, stat
//! buffer, flags - regular files and directories reported through the machine's
//! own `struct stat`, and ENOENT for a path that does not exist. The
//! `AT_EMPTY_PATH` form stats the descriptor itself and must agree with `fstat`;
//! without the flag an empty path is just an empty name (ENOENT); paths resolve
//! relative to a directory descriptor, and a dirfd that is not open answers
//! EBADF. An **absolute** path consults the descriptor not at all - neither
//! whether it is open nor what kind it is - and a name pointer the kernel cannot
//! read answers EFAULT before either question.
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
//! on: the *relative* claim is EBADF, which any unopened descriptor gives, and
//! the absolute one is served, which any descriptor at all gives.
//!
//! # Which of the two questions is asked first, and why the path's shape decides
//!
//! The no-std version statted `/etc/motd` - an **absolute** path - with a dirfd
//! nothing is open on, and this kernel answered EBADF. That was a divergence
//! wearing a pin: Linux does not look at the dirfd at all when the path is
//! absolute, so `newfstatat(1000, "/tmp", buf, 0)` with 1000 not open answers `0`
//! with errno `0` there, and the descriptor's *kind* is not a question either -
//! measured on a current host, the same call through a dirfd open on a regular
//! file answers `0`. Both are pinned below, and the relative claim that answers
//! EBADF is kept beside them, because the pair is the rule: the descriptor stops
//! being a question, not the path.
//!
//! The order the two questions are asked in is the other half. The kernel reads
//! the name before it looks at the descriptor, as Linux does - the copy happens
//! in `getname()` and `path_init()` reads the name's first character before
//! `fdget_raw()` is reached - so a name the kernel cannot read at all answers
//! EFAULT even when the dirfd is bad too. That claim needs `libc::syscall`:
//! `rustix::fs::statat` takes a path, and "a path that is a bad address" is not
//! something a `&CStr` can spell (`user/tests/Cargo.toml`'s shape clause). The
//! machinery is the raw call alone and the number comes from `libc`'s own table,
//! which is the machine's.
//!
//! The family's other members carry the same rule through their own doors; this
//! program pins it where it was found, and `t-openat` pins the same two answers
//! for the family's busiest member.

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
/// an absolute path stops the dirfd being a question at all. See the claims.
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

    // The pair that is the whole rule. An absolute path does not reach the
    // descriptor - not its state and not its kind - so both of these are
    // served, and it is the descriptor that stops being a question rather than
    // the path: the relative claim below still answers EBADF.
    //
    // SAFETY: a descriptor number nothing is open on. Two of the three claims
    // below are *served* through it, which is what they pin.
    let missing = unsafe { BorrowedFd::borrow_raw(BAD_DIRFD) };
    r.check(
        "an absolute path with a bad dirfd is served",
        matches!(
            statat(missing, DIR, AtFlags::empty()),
            Ok(st) if FileType::from_raw_mode(st.st_mode).is_dir()
        ),
    );
    match openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) {
        Ok(file) => r.check(
            "an absolute path is served through a regular-file dirfd too",
            matches!(
                statat(&file, DIR, AtFlags::empty()),
                Ok(st) if FileType::from_raw_mode(st.st_mode).is_dir()
            ),
        ),
        Err(e) => r.check(
            &format!("the fixture reopens for the kind claim (got {e})"),
            false,
        ),
    }

    // A *relative* name still goes through the descriptor, which is what the
    // no-std version's claim was about before its absolute path hid it.
    r.check(
        "a bad dirfd answers EBADF for a relative name",
        matches!(
            statat(missing, ANY_NAME, AtFlags::empty()),
            Err(Errno::BADF)
        ),
    );

    // And the name is read before the descriptor is: a pointer the kernel
    // cannot read is EFAULT even though the same dirfd is bad. Raw, because no
    // `&CStr` may name a bad address (see the module comment). The buffer is a
    // word array rather than `struct stat` - nothing writes it, the call fails
    // first - and 256 bytes hold either machine's layout.
    let mut raw = [0u64; 32];
    let fault = unsafe {
        libc::syscall(
            libc::SYS_newfstatat,
            BAD_DIRFD as libc::c_long,
            1usize as libc::c_long,
            raw.as_mut_ptr() as libc::c_long,
            0,
        )
    };
    r.check(
        "an unreadable name pointer answers EFAULT, not the dirfd's EBADF",
        fault == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EFAULT),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
