//! t-chdir - one test for chdir and fchdir, which the shell's `cd` and every
//! process that moves between directories leans on.
//!
//! Pinned here: chdir to an absolute and to a relative path succeeds and getcwd
//! reports where it landed, openat with AT_FDCWD resolves against the new cwd,
//! chdir to a file answers ENOTDIR and to a missing path ENOENT, fchdir moves to
//! the directory its descriptor names (and getcwd follows), a regular-file
//! descriptor answers ENOTDIR, and a bad fd answers EBADF.
//!
//! # The fixture, and why it moved
//!
//! The no-std version chdir'd to `/etc`, opened `motd` relatively, and used
//! `/etc/motd` for the ENOTDIR pair. The second machine's payload carries no motd
//! (`PLAN.md` §15.11), so this version owns its file under `/tmp` - the claim is
//! about where `chdir` puts the process, not about which file is read through the
//! new cwd - and it chdirs to `/tmp`, whose `..` is `/` and which both payloads
//! carry, before the relative step into `/bin`.
//!
//! # Why this program calls `libc::getcwd`
//!
//! For the reason `t-getcwd` records at length: `rustix::process::getcwd` takes a
//! `Vec` and returns a `CString`, so "the answer is the buffer address" is outside
//! it - the third clause of `user/tests/Cargo.toml`'s `libc` rule. `chdir` and
//! `fchdir` do come from `rustix`, which binds those two as the ABI promises.

use mpos_user_tests::Report;
use rustix::fd::BorrowedFd;
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{Errno, read};
use rustix::process::{chdir, fchdir};

/// The directory this test moves into, and whose `..` is `/`.
const DIR: &str = "/tmp";
/// The file this test owns: opened by the relative name through the cwd it just
/// chdir'd into, and the regular-file subject of the ENOTDIR pair.
const FILE: &str = "/tmp/t-chdir_file";
const FILE_REL: &str = "t-chdir_file";
const NO_SUCH: &str = "/no/such";

/// One `getcwd` into `buf`, answering the raw result - see `t-getcwd`.
fn getcwd(buf: &mut [u8]) -> *mut libc::c_char {
    // SAFETY: `buf` is a live, writable slice and its length is the size that goes
    // with it.
    unsafe { libc::getcwd(buf.as_mut_ptr().cast(), buf.len()) }
}

fn main() {
    println!("t-chdir");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, b"mpos t-chdir fixture\n").is_err() {
        r.check("chdir(/tmp) succeeds", false);
        r.finish()
    }

    r.check("chdir(/tmp) succeeds", chdir(DIR).is_ok());

    // AT_FDCWD opens resolve against the new cwd.
    let Ok(fd) = openat(CWD, FILE_REL, OFlags::RDONLY, Mode::empty()) else {
        r.check("openat resolves the file against the new cwd", false);
        r.finish()
    };
    r.check("openat resolves the file against the new cwd", true);
    let mut buf = [0u8; 256];
    r.check(
        "the relatively opened file reads",
        read(&fd, &mut buf).is_ok(),
    );
    drop(fd);

    // A relative chdir from / lands in /bin.
    r.check("chdir(..) leaves /tmp", chdir("..").is_ok());
    r.check("chdir(bin) relative succeeds", chdir("bin").is_ok());
    let mut cwd = [0u8; 256];
    let answer = getcwd(&mut cwd);
    r.check(
        "getcwd returns the buffer",
        answer == cwd.as_mut_ptr().cast(),
    );
    r.check(
        "the cwd reads /bin",
        cwd[0] == b'/' && cwd[1] == b'b' && cwd[2] == b'i' && cwd[3] == b'n' && cwd[4] == 0,
    );
    r.check("chdir(..) leaves /bin", chdir("..").is_ok());

    // A file is not a directory, and a missing path is not anything.
    r.check(
        "chdir to a file answers ENOTDIR",
        chdir(FILE) == Err(Errno::NOTDIR),
    );
    r.check(
        "chdir to a missing path answers ENOENT",
        chdir(NO_SUCH) == Err(Errno::NOENT),
    );

    // fchdir moves to the directory the descriptor names.
    let Ok(dfd) = openat(CWD, DIR, OFlags::RDONLY, Mode::empty()) else {
        r.check("the directory opens as a directory fd", false);
        r.finish()
    };
    r.check("the directory opens as a directory fd", true);
    r.check("fchdir to the directory fd succeeds", fchdir(&dfd).is_ok());
    let answer = getcwd(&mut cwd);
    r.check(
        "getcwd returns the buffer after fchdir",
        answer == cwd.as_mut_ptr().cast(),
    );
    r.check(
        "the cwd reads /tmp after fchdir",
        cwd[0] == b'/' && cwd[1] == b't' && cwd[2] == b'm' && cwd[3] == b'p' && cwd[4] == 0,
    );
    drop(dfd);

    // A regular-file descriptor is not a directory; a bad fd is not a fd.
    let Ok(ffd) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("the file opens for the ENOTDIR check", false);
        r.finish()
    };
    r.check("the file opens for the ENOTDIR check", true);
    r.check(
        "fchdir on a file fd answers ENOTDIR",
        matches!(fchdir(&ffd), Err(Errno::NOTDIR)),
    );
    drop(ffd);

    // A number nothing is open on. `BorrowedFd::borrow_raw` refuses a negative raw
    // fd by contract, so 999 is the number `t-read`/`t-write`/`t-openat` use; the
    // kernel's answer comes from the same branch - the descriptor table has no
    // such row.
    let missing = unsafe { BorrowedFd::borrow_raw(999) };
    r.check(
        "fchdir on a bad fd answers EBADF",
        matches!(fchdir(missing), Err(Errno::BADF)),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
