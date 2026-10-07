//! t-dup - one test for dup, the fd-table copy musl's startup and the shell's
//! redirections lean on.
//!
//! Pinned here: dup hands back the lowest free descriptor, and the duplicate
//! names the *same* open file description - one cursor moved through one fd is
//! visible through the other, which is what makes `cat a b > out` append instead
//! of overwrite. A descriptor that does not exist answers `EBADF`.
//!
//! "Lowest free" is measured, not assumed to be 3. A process inherits whatever
//! descriptors its parent had: under this kernel's own shell that is 0-2 and
//! nothing else, but a real shell like Alpine's ash holds one of its own, and the
//! kernel answering 4 when 3 is taken is correct. What this pins is that `dup`
//! and `open` agree on the same next number.
//!
//! The fixture is one this test writes under `/tmp` rather than the no-std
//! version's `/etc/motd`: the claim is about descriptor semantics, not about
//! which file they are exercised on, and the second machine's payload carries
//! `/tmp` and no motd (`PLAN.md` §15.11).

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd, IntoRawFd};
use rustix::fs::{CWD, Mode, OFlags, SeekFrom, openat, seek};
use rustix::io::{Errno, dup};

/// The file this test owns. Longer than three bytes so a seek to 2 leaves
/// something behind it and `SEEK_END` is not zero.
const FILE: &str = "/tmp/t-dup_file";
const CONTENT: &[u8] = b"mpos t-dup fixture: at least a few bytes\n";

fn main() {
    println!("t-dup");
    let mut r = Report::new();

    // The standard descriptors are open by the runner's own contract.
    let stdout = unsafe { BorrowedFd::borrow_raw(1) };

    // What the lowest free descriptor is, discovered rather than assumed: dup
    // and open must both answer it.
    let Ok(probe) = dup(stdout) else {
        r.check("dup(stdout) hands out a free fd", false);
        r.finish()
    };
    let base = probe.as_raw_fd();
    drop(probe);
    r.check("dup(stdout) hands out a free fd", base >= 3);

    if std::fs::write(FILE, CONTENT).is_err() {
        r.check("the file opens on the fd dup had", false);
        r.finish()
    }

    let Ok(a) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("the file opens on the fd dup had", false);
        r.finish()
    };
    r.check("the file opens on the fd dup had", a.as_raw_fd() == base);

    let Ok(b) = dup(&a) else {
        r.check("dup lands on the next free fd", false);
        r.finish()
    };
    r.check("dup lands on the next free fd", b.as_raw_fd() == base + 1);

    r.check(
        "lseek through the original reports 2",
        seek(&a, SeekFrom::Start(2)) == Ok(2),
    );
    r.check(
        "the duplicate shares the file offset",
        seek(&b, SeekFrom::Current(0)) == Ok(2),
    );

    let Ok(end) = seek(&b, SeekFrom::End(0)) else {
        r.check("SEEK_END through the duplicate reports the size", false);
        r.finish()
    };
    r.check("SEEK_END through the duplicate reports the size", end > 0);
    r.check(
        "the original sees the duplicate's seek",
        seek(&a, SeekFrom::Current(0)) == Ok(end),
    );

    // `rustix::io::close` takes a raw fd and returns nothing, so the "close
    // answers 0" claim is `libc::close`'s - the same clause `t-dup2` needs for
    // `dup2`, and for the same reason: the result the ABI promises has to be
    // observable for a test to pin it (`user/tests/Cargo.toml`). The ownership
    // is taken back out of the `OwnedFd` first, so nothing closes the number a
    // second time.
    let (ra, rb) = (a.into_raw_fd(), b.into_raw_fd());
    r.check(
        "closing the original succeeds",
        unsafe { libc::close(ra) } == 0,
    );
    r.check(
        "closing the duplicate succeeds",
        unsafe { libc::close(rb) } == 0,
    );

    let missing = unsafe { BorrowedFd::borrow_raw(99) };
    // `matches!` rather than `==`: `dup` answers an `OwnedFd`, which is not
    // `PartialEq`, so the comparison is on the error arm.
    r.check(
        "a descriptor that does not exist answers EBADF",
        matches!(dup(missing), Err(Errno::BADF)),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
