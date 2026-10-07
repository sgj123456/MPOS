//! t-lseek - one test for lseek, which the strace uses to walk back through
//! /proc/self/maps (`lseek(3, 0, SEEK_CUR)`, `lseek(3, 3070, SEEK_SET)`).
//!
//! Pinned here on a real file: SEEK_END reports the size, SEEK_SET and
//! SEEK_CUR report and move the absolute position, a full re-read from 0 hands
//! the whole file back, and a bogus whence answers EINVAL. The edges are pinned
//! too: a seek that would land before byte 0 answers EINVAL for all three
//! origins, and a descriptor that does not exist answers EBADF.
//!
//! # The two claims `rustix` cannot spell
//!
//! Most of this program is `rustix::fs::seek`, whose `SeekFrom` names each
//! origin. Two claims need `libc::lseek`, and the first is the rule's clause (c)
//! proper: `SeekFrom` is a plain *enum* - `Start`, `End`, `Current`, `Data`,
//! `Hole` - so a bogus whence like 5 has nowhere to go in it at all, and "an
//! unknown whence answers EINVAL" cannot be asked through the wrapper. (An enum
//! is not a bitflags type: a bitflags type carries bitflags' unnamed flag
//! `const _ = !0` and so *can* hold an unnamed word through `from_bits_retain`,
//! which is why `t-fallocate` needs no `libc` for its unnamed mode bit and this
//! program does need it for its unnamed whence.)
//!
//! The second is a readability choice riding along: `SeekFrom::Start` takes a
//! `u64`, while `lseek`'s offset is a signed `off_t`, so "SEEK_SET to -1" is not
//! spellable in the type. `rustix`'s own backend says it would reach the same
//! kernel branch anyway - it casts with a comment, "Silently cast; we'll get
//! `EINVAL` if the value is negative" - so `SeekFrom::Start(u64::MAX)` would
//! work. `libc::lseek(fd, -1, SEEK_SET)` is what the claim says, and the program
//! is already holding `libc` for the whence above.
//!
//! # What this pins on each machine
//!
//! `lseek` has a real number on both (`nr_aarch64.rs`: `LSEEK = 62`;
//! `nr_x86_64.rs`: `LSEEK = 8`), both reach the same `handle_lseek`, and the
//! per-claim output is the same on both machines.
//!
//! The file is one this test writes itself under `/tmp`, not the no-std
//! version's `/etc/motd` - the second machine's sweep disk carries no motd
//! (`PLAN.md` §15.11), and the claim is about where `lseek` puts the cursor, not
//! about which file the cursor is in. The size the claims are stated against is
//! the one the test *drains* out of the file rather than a constant it also
//! wrote down, so the numbers stay the file's.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd};
use rustix::fs::{CWD, Mode, OFlags, SeekFrom, openat, seek};
use rustix::io::{Errno, read};

/// The file this test owns, in place of the no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-lseek_file";
const CONTENT: &[u8] = b"mpos t-lseek fixture: a line of text to seek around in.\n";

/// The number nothing is open on. Not part of the claim - the descriptor
/// table's empty row is - and 99 is the one `t-read`/`t-write`/`t-openat`/
/// `t-fallocate` probe.
const CLOSED: i32 = 99;

/// A whence no header names. `SEEK_SET`/`CUR`/`END` are 0/1/2 and
/// `SEEK_DATA`/`HOLE` are 3/4, so 5 is the first word the vocabulary does not
/// have.
const NOT_A_WHENCE: i32 = 5;

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn main() {
    println!("t-lseek");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, CONTENT).is_err() {
        r.check("the file to seek opens", false);
        r.finish()
    }
    let Ok(fd) = openat(CWD, FILE, OFlags::RDONLY, Mode::empty()) else {
        r.check("the file to seek opens", false);
        r.finish()
    };
    r.check("the file to seek opens", true);

    // Drain the file to learn its true size. The claims below are then about
    // the file's own length rather than about a number this test also asserted.
    let mut total = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        match read(&fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }

    r.check(
        "SEEK_END reports the file size",
        seek(&fd, SeekFrom::End(0)) == Ok(total as u64),
    );
    r.check(
        "SEEK_SET rewinds to 0",
        seek(&fd, SeekFrom::Start(0)) == Ok(0),
    );
    r.check(
        "a full re-read hands the whole file back",
        matches!(read(&fd, &mut buf), Ok(n) if n == total),
    );
    r.check(
        "SEEK_CUR reports the position after the read",
        seek(&fd, SeekFrom::Current(0)) == Ok(total as u64),
    );

    // The two claims that leave `rustix`: the enum has no arm for a word like
    // 5, and `Start` is unsigned. See the module comment.
    r.check(
        "an unknown whence answers EINVAL",
        unsafe { libc::lseek(fd.as_raw_fd(), 0, NOT_A_WHENCE) } == -1
            && last_errno() == libc::EINVAL,
    );
    r.check(
        "SEEK_SET to -1 answers EINVAL",
        unsafe { libc::lseek(fd.as_raw_fd(), -1, libc::SEEK_SET) } == -1
            && last_errno() == libc::EINVAL,
    );

    // Negative landings: every origin that would end before byte 0. The two
    // refusals above moved nothing, so the cursor is still at the end.
    r.check(
        "SEEK_END(0) parks at the size",
        seek(&fd, SeekFrom::End(0)) == Ok(total as u64),
    );
    r.check(
        "SEEK_CUR(-1) from the end lands on size - 1",
        seek(&fd, SeekFrom::Current(-1)) == Ok(total as u64 - 1),
    );
    r.check(
        "SEEK_CUR far before byte 0 answers EINVAL",
        seek(&fd, SeekFrom::Current(-1_000_000)) == Err(Errno::INVAL),
    );
    r.check(
        "SEEK_END(-size) rewinds to 0",
        seek(&fd, SeekFrom::End(-(total as i64))) == Ok(0),
    );

    drop(fd);
    let closed = unsafe { BorrowedFd::borrow_raw(CLOSED) };
    r.check(
        "a descriptor that does not exist answers EBADF",
        seek(closed, SeekFrom::Start(0)) == Err(Errno::BADF),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
