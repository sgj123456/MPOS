//! t-fstat - one test for fstat, which the strace calls on every file the
//! loader opens (`fstat(3, {st_mode=S_IFREG|0644, st_size=158831, ...})`).
//!
//! Pinned here: the Linux argument shape - fd then buffer, no length, no path -
//! and consistency: `st_size` must equal what reading the file actually yields,
//! `st_mode` must say regular file, directories report `nlink` 2, two stats of
//! one fd agree, a descriptor that is not open answers EBADF, and a buffer the
//! kernel cannot write answers EFAULT.
//!
//! # The fixture is this test's own file now
//!
//! The no-std version statted `/etc/motd`, and the second machine's payload has
//! no motd at all (`PLAN.md` §15.11, measured with `debugfs`), so this test
//! writes its own file under `/tmp` and stats that. The `/bin` claims stay: it
//! is the one directory both payloads carry, and it is a directory with no
//! subdirectory on either - which is what makes its `nlink` 2.
//!
//! # The struct is `rustix`'s, and the *length* is pinned somewhere else
//!
//! The no-std version's first subject was the 144-byte x86_64 `struct stat`: it
//! declared that struct (`lin::Stat`) and handed the kernel a pointer to it,
//! which is a correct buffer length on one machine and a sixteen-byte overflow
//! on the other (§13.10). `rustix::fs::fstat` answers a `Stat` that is *this*
//! machine's own - the same shape `t-link` and `t-fallocate` already read - so
//! the claims here become the **answers**: the size, the file type, the link
//! count, and their stability between two calls.
//!
//! The length itself is not this program's to pin any more, and that is worth
//! saying where it used to be: it is `t-stat-buffer`'s subject (§13.10), which
//! hands the kernel a guarded struct and can see the bytes past it. A wrapper
//! that owns its buffer cannot express "and it wrote exactly this far", which is
//! why that program is the raw call and this one does not need to be.
//!
//! # The one call that is not `rustix`
//!
//! `fstat` with a buffer the kernel cannot write is the rule's clause (c) - the
//! wrapper binds the call and owns the buffer, so the *address* the syscall is
//! handed is outside it - and so is the C library's `fstat`, measured: musl's
//! `fstat` faults in userspace on a buffer the kernel would refuse (NULL and
//! `0x1` alike, SIGSEGV), where the raw call answers EFAULT for both and 0 for a
//! real buffer. So this one claim is `libc::syscall(libc::SYS_fstat, ..)` - the
//! raw door with the number in a maintained per-machine table (`5` here, `80`
//! on the second machine) - which is what that door is for. `fstat_raw` carries
//! the measurements.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::fs::{CWD, FileType, Mode, OFlags, fstat, openat};
use rustix::io::{Errno, read};

/// The file this test owns, in place of the no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-fstat_file";
/// A directory both payloads carry, with no subdirectory in either.
const DIR: &str = "/bin";
/// A descriptor number nothing is open on. `BorrowedFd::borrow_raw` refuses a
/// negative raw fd by contract, and the kernel answers the same EBADF branch.
const BAD_FD: i32 = 77;

/// The bytes the fixture holds. `st_size` is compared against what a read
/// actually yielded rather than against this constant, so a stat that reported
/// the buffer's length instead of the file's would still fail.
const BODY: &[u8] = b"mpos t-fstat fixture, read back before the stat\n";

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `fstat(fd, buf)` as the **raw** call, for the one claim `rustix` cannot
/// express *and* the C library's own `fstat` cannot either.
///
/// That second half is a measurement rather than a preference. `rustix`'s
/// `fstat` owns its buffer, so the caller's address is outside it - and musl's
/// `fstat`, handed a buffer the kernel would refuse, faults in userspace
/// instead of answering: NULL and `0x1` both killed the process with SIGSEGV
/// (`status 139`), run in child processes so that one crash could not hide the
/// next, while the raw call answered EFAULT for both and 0 for the caller's own
/// buffer. `size_of::<libc::stat>()` is 144, so the type the crate declares is
/// the right one - the wrapper is simply not a passthrough for this argument.
/// The number still comes from a maintained per-machine table (`libc::SYS_fstat`:
/// 5 here, 80 on the second machine).
fn fstat_raw(fd: i32, buf: *mut libc::stat) -> Result<(), Errno> {
    let r = unsafe { libc::syscall(libc::SYS_fstat, fd as libc::c_long, buf as libc::c_long) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(())
    }
}

fn main() {
    println!("t-fstat");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, BODY).is_err() {
        r.check("the file to stat can be written", false);
        r.finish()
    }

    match openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => {
            r.check("the file to stat opens", true);
            // Read it whole first: `st_size` has to agree with reality, and the
            // total is what it is compared against.
            let mut total = 0usize;
            let mut buf = [0u8; 512];
            for _ in 0..8 {
                match read(&fd, &mut buf) {
                    Ok(n) if n > 0 => total += n,
                    _ => break,
                }
            }

            match fstat(&fd) {
                Ok(st) => {
                    r.check("fstat(fd) succeeds", true);
                    r.check("st_size matches the bytes read", st.st_size == total as i64);
                    r.check(
                        "st_mode says regular file",
                        FileType::from_raw_mode(st.st_mode).is_file(),
                    );

                    // A second stat of the same descriptor must agree with the
                    // first.
                    match fstat(&fd) {
                        Ok(again) => {
                            r.check("the second fstat succeeds", true);
                            r.check(
                                "st_size is stable between two stats",
                                again.st_size == st.st_size,
                            );
                            r.check(
                                "st_ino is stable between two stats",
                                again.st_ino == st.st_ino,
                            );
                        }
                        Err(_) => {
                            r.check("the second fstat succeeds", false);
                            r.check("st_size is stable between two stats", false);
                            r.check("st_ino is stable between two stats", false);
                        }
                    }
                }
                Err(_) => {
                    r.check("fstat(fd) succeeds", false);
                    r.check("st_size matches the bytes read", false);
                    r.check("st_mode says regular file", false);
                    r.check("the second fstat succeeds", false);
                    r.check("st_size is stable between two stats", false);
                    r.check("st_ino is stable between two stats", false);
                }
            }

            // A buffer the kernel cannot write answers EFAULT, not corruption.
            r.check(
                "fstat with a null buffer answers EFAULT",
                fstat_raw(fd.as_raw_fd(), core::ptr::null_mut()) == Err(Errno::FAULT),
            );
        }
        Err(e) => {
            r.check(&format!("the file to stat opens (got {e})"), false);
            r.check("fstat(fd) succeeds", false);
            r.check("st_size matches the bytes read", false);
            r.check("st_mode says regular file", false);
            r.check("the second fstat succeeds", false);
            r.check("st_size is stable between two stats", false);
            r.check("st_ino is stable between two stats", false);
            r.check("fstat with a null buffer answers EFAULT", false);
        }
    }

    // A descriptor that is not open answers EBADF. The number is positive
    // because `BorrowedFd::borrow_raw` refuses a negative raw fd by contract.
    // SAFETY: a descriptor number nothing is open on - the kernel answers EBADF,
    // which is the claim.
    let missing = unsafe { BorrowedFd::borrow_raw(BAD_FD) };
    r.check(
        "fstat on a bad fd answers EBADF",
        matches!(fstat(missing), Err(Errno::BADF)),
    );

    // A directory: S_IFDIR in st_mode, nlink 2 like the man page's example.
    match openat(CWD, DIR, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => {
            r.check("/bin opens", true);
            match fstat(&fd) {
                Ok(st) => {
                    r.check("fstat(/bin) succeeds", true);
                    r.check(
                        "st_mode says directory",
                        FileType::from_raw_mode(st.st_mode).is_dir(),
                    );
                    r.check("a directory reports st_nlink 2", st.st_nlink == 2);
                }
                Err(_) => {
                    r.check("fstat(/bin) succeeds", false);
                    r.check("st_mode says directory", false);
                    r.check("a directory reports st_nlink 2", false);
                }
            }
        }
        Err(e) => {
            r.check(&format!("/bin opens (got {e})"), false);
            r.check("fstat(/bin) succeeds", false);
            r.check("st_mode says directory", false);
            r.check("a directory reports st_nlink 2", false);
        }
    }

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
