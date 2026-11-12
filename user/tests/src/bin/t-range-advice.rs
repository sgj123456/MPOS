//! `readahead(2)` and `sync_file_range(2)` — the two "here is a byte range, do
//! something sensible with it" doors.
//!
//! Both were answered by a shape rather than by a rule. `readahead` was "the
//! descriptor exists, therefore 0", so a write-only descriptor, a directory, a
//! pipe and a character device all got the 0 Linux reserves for a file it will
//! really read. `sync_file_range` had no dispatch arm at all, so it answered
//! `ENOSYS` — a call Linux has served since 2.6.17, and one whose whole point is
//! that the caller does not have to wait for the data.
//!
//! The host's answers (Linux 7.2.14, measured in `build/scratch/advice/`):
//!
//! | call | answer |
//! |---|---|
//! | `readahead(regular, 0, 4096)` | 0 |
//! | `readahead(write-only regular fd)` | `EBADF` |
//! | `readahead(dir)` / `(pipe)` / `(/dev/null)` / `(socket)` | `EINVAL` |
//! | `readahead(regular, -1, 4096)` | `EINVAL` |
//! | `readahead(regular, 0, 2^63)` | `EINVAL` |
//! | `readahead(regular, 0, i64::MAX)` / `(0, 131073)` | 0 |
//! | `readahead(999, -1, 4096)` | `EBADF` |
//! | `sync_file_range(regular, 0, 4096, 0\|1\|2\|4\|7)` | 0 |
//! | `sync_file_range(regular, 0, 4096, 8)` | `EINVAL` |
//! | `sync_file_range(pipe, 0, 4096, 8)` | `EINVAL` |
//! | `sync_file_range(pipe)` / `(/dev/null)` / `(socket)` | `ESPIPE` |
//! | `sync_file_range(dir)` | 0 |
//! | `sync_file_range(999, 0, 4096, 8)` | `EBADF` |
//! | `sync_file_range(regular, -1, 4096, 0)` | `EINVAL` |
//! | `sync_file_range(regular, 2^62, 2^62, 0)` | `EINVAL` (the sum wraps) |
//! | `sync_file_range(regular, 0, i64::MAX, 0)` | 0 |
//!
//! Two of those readings are the interesting ones. The descriptor is judged
//! before the range in `readahead` (`999, -1` is `EBADF`, not `EINVAL`), and the
//! **flag word** is judged before the descriptor's kind in `sync_file_range`
//! (`pipe, flags=8` is `EINVAL` while `pipe, flags=0` is `ESPIPE`). A door that
//! asks in the other order answers the same errno for one caller and the wrong
//! one for every caller who has two things wrong at once.
//!
//! Nothing here asserts that the data was actually pulled in or flushed: both
//! calls are advice, and this kernel writes through and reads eagerly, so the
//! success *is* the admission. What is pinned is which question each door asks
//! first.

use mpos_user_tests::Report;
use std::os::unix::io::{AsRawFd, RawFd};

const OK: i32 = 0;
const EBADF: i32 = 9;
const EINVAL: i32 = 22;
const ESPIPE: i32 = 29;

/// A descriptor number no process here can hold.
const BAD: i32 = 999;

/// `2^63` as an unsigned count, which is the negative `i64` Linux reads.
const COUNT_AS_NEGATIVE: usize = 1 << 63;

fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

fn readahead(fd: RawFd, offset: i64, count: usize) -> i32 {
    match raw(libc::SYS_readahead, fd as usize, offset as usize, count, 0) {
        Ok(_) => OK,
        Err(e) => e,
    }
}

fn sync_file_range(fd: RawFd, offset: i64, nbytes: i64, flags: usize) -> i32 {
    let nr = libc::SYS_sync_file_range;
    match raw(nr, fd as usize, offset as usize, nbytes as usize, flags) {
        Ok(_) => OK,
        Err(e) => e,
    }
}

fn want(r: &mut Report, label: &str, got: i32, expect: i32) {
    r.check(
        &format!("{label} (got {got}, want {expect})"),
        got == expect,
    );
}

fn main() {
    let mut r = Report::new();

    let path = format!("/tmp/mpos-range-advice-{}", std::process::id());
    let _ = std::fs::remove_file(&path);
    if std::fs::write(&path, b"0123456789abcdef").is_err() {
        r.check("a file to advise about is created", false);
        r.finish();
    }
    let regular = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            r.check("the file opens for reading", false);
            r.finish();
        }
    };
    let write_only = match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(f) => f,
        Err(_) => {
            r.check("the file opens for writing", false);
            r.finish();
        }
    };
    let dir = match std::fs::File::open("/tmp") {
        Ok(f) => f,
        Err(_) => {
            r.check("a directory opens", false);
            r.finish();
        }
    };
    let chardev = match std::fs::File::open("/dev/null") {
        Ok(f) => f,
        Err(_) => {
            r.check("a character device opens", false);
            r.finish();
        }
    };
    let mut pipe = [0 as RawFd; 2];
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        r.check("a pipe is made", false);
        r.finish();
    }

    let ok = readahead(regular.as_raw_fd(), 0, 4096);
    want(&mut r, "a promise about a regular file is served", ok, OK);

    let wo = readahead(write_only.as_raw_fd(), 0, 4096);
    want(
        &mut r,
        "a descriptor this call cannot read from is EBADF",
        wo,
        EBADF,
    );

    let d = readahead(dir.as_raw_fd(), 0, 4096);
    want(&mut r, "a directory has no page cache to fill", d, EINVAL);

    let p = readahead(pipe[0], 0, 4096);
    want(&mut r, "a pipe has no page cache either", p, EINVAL);

    let c = readahead(chardev.as_raw_fd(), 0, 4096);
    want(&mut r, "and neither has a character device", c, EINVAL);

    let negative = readahead(regular.as_raw_fd(), -1, 4096);
    want(&mut r, "a negative offset is refused", negative, EINVAL);

    let negative_zero = readahead(regular.as_raw_fd(), -1, 0);
    want(
        &mut r,
        "and it is refused even with nothing to read",
        negative_zero,
        EINVAL,
    );

    let signed_count = readahead(regular.as_raw_fd(), 0, COUNT_AS_NEGATIVE);
    want(
        &mut r,
        "a count whose top bit is set is a negative count",
        signed_count,
        EINVAL,
    );

    let huge = readahead(regular.as_raw_fd(), 0, 131_073);
    want(
        &mut r,
        "a count past MAX_READAHEAD is still a promise, not a refusal",
        huge,
        OK,
    );

    let missing = readahead(BAD, -1, 4096);
    want(
        &mut r,
        "the descriptor is asked before the range",
        missing,
        EBADF,
    );

    // ---- sync_file_range ----
    let plain = sync_file_range(regular.as_raw_fd(), 0, 4096, 0);
    want(&mut r, "a flush of a regular range is served", plain, OK);

    let all_bits = sync_file_range(regular.as_raw_fd(), 0, 4096, 7);
    want(
        &mut r,
        "all three flag bits together are served",
        all_bits,
        OK,
    );

    let bad_flag = sync_file_range(regular.as_raw_fd(), 0, 4096, 8);
    want(
        &mut r,
        "a flag bit outside the vocabulary is EINVAL",
        bad_flag,
        EINVAL,
    );

    let pipe_flags = sync_file_range(pipe[0], 0, 4096, 8);
    want(
        &mut r,
        "the flag word is judged before the node's kind",
        pipe_flags,
        EINVAL,
    );

    let pipe_plain = sync_file_range(pipe[0], 0, 4096, 0);
    want(
        &mut r,
        "a pipe has nothing to flush, so the same pipe is ESPIPE",
        pipe_plain,
        ESPIPE,
    );

    let char_plain = sync_file_range(chardev.as_raw_fd(), 0, 4096, 0);
    want(
        &mut r,
        "a character device has nothing to flush either",
        char_plain,
        ESPIPE,
    );

    let dir_plain = sync_file_range(dir.as_raw_fd(), 0, 4096, 0);
    want(
        &mut r,
        "a directory does have something to flush",
        dir_plain,
        OK,
    );

    let missing_flags = sync_file_range(BAD, 0, 4096, 8);
    want(
        &mut r,
        "the descriptor is asked before the flag word",
        missing_flags,
        EBADF,
    );

    let negative_offset = sync_file_range(regular.as_raw_fd(), -1, 4096, 0);
    want(
        &mut r,
        "a negative offset is refused by this call too",
        negative_offset,
        EINVAL,
    );

    let negative_length = sync_file_range(regular.as_raw_fd(), 0, -1, 0);
    want(
        &mut r,
        "and so is a negative length",
        negative_length,
        EINVAL,
    );

    let wrapping = sync_file_range(regular.as_raw_fd(), 1 << 62, 1 << 62, 0);
    want(
        &mut r,
        "a range whose sum wraps an i64 is refused",
        wrapping,
        EINVAL,
    );

    let huge_ok = sync_file_range(regular.as_raw_fd(), 0, i64::MAX, 0);
    want(
        &mut r,
        "a huge range that does not wrap is still advice",
        huge_ok,
        OK,
    );

    let _ = std::fs::remove_file(&path);
    r.finish();
}
