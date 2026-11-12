//! `fadvise64(2)` — the hint whose vocabulary, range and order were all wrong at
//! once.
//!
//! A hint sounds like a call with nothing to refuse, and the door was written
//! that way: it accepted any descriptor that was open, refused a pipe, then
//! compared the advice word. Three shapes came out of that single line of
//! reasoning, each of them a caller's wrong answer:
//!
//! * the **order** was backwards - `(pipe, advice=6)` answered `EINVAL` where
//!   Linux answers `ESPIPE`, because the kind is judged before the advice;
//! * the **range** was not judged at all - `(-1, 4096)` and `(0, -1)` were
//!   answered 0 where Linux answers `EINVAL`;
//! * the **word width** was wrong - the advice was compared as the whole `usize`
//!   the register carries, so a caller passing `0x1_0000_0003` was refused,
//!   while Linux reads the low 32 bits and sees `POSIX_FADV_NORMAL`.
//!
//! Measured on Linux 7.2.14 (`build/scratch/advice/{f,g,h,i,j}.c`):
//!
//! | call | answer |
//! |---|---|
//! | `fadvise(regular, 0, 4096, 0..=5)` | 0 |
//! | `fadvise(regular, 0, 4096, 6 \| 7 \| -1)` | `EINVAL` |
//! | `fadvise(regular, 0, 4096, 0x1_0000_0003)` | 0 - the word is an `int` |
//! | `fadvise(regular, 0, 4096, 0x1_0000_0006)` | `EINVAL` |
//! | `fadvise(regular, -1, 0 \| 4096, 3)` | `EINVAL` |
//! | `fadvise(regular, 0, -1, 3)` | `EINVAL` |
//! | `fadvise(regular, -4096, 4096, 3)` | `EINVAL` |
//! | `fadvise(regular, 2^62, 2^62, 3)` | 0 - a wrapping sum is fine |
//! | `fadvise(regular, 0, i64::MAX, 3)` | 0 |
//! | `fadvise(pipe, 0, 4096, 9)` | `ESPIPE` - the kind outranks the advice |
//! | `fadvise(pipe, -1, 4096, 3)` | `ESPIPE` - and the range |
//! | `fadvise(socketpair \| socket \| /dev/null \| dir, 0, 4096, 3)` | 0 |
//! | `fadvise(999, 0, 4096, 9)` | `EBADF` - the descriptor outranks everything |
//!
//! The row worth reading twice is the third. Four neighbouring doors in this
//! kernel - `readahead`, `sync_file_range`, `fadvise64` and `madvise` - each take
//! a "advice" word, and three of them read it as a 32-bit `int` while the fourth
//! reads a flag word. Nothing about the call's shape tells them apart; only the
//! measurement does.

use mpos_user_tests::Report;
use std::os::unix::io::{AsRawFd, RawFd};

const OK: i32 = 0;
const EBADF: i32 = 9;
const EINVAL: i32 = 22;
const ESPIPE: i32 = 29;

const BAD: i32 = 999;
/// `2^62`, the offset whose sum with itself wraps an `i64`.
const HIGH: i64 = 1 << 62;

const POSIX_FADV_NORMAL: usize = 0;
const POSIX_FADV_NOREUSE: usize = 5;

fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> Result<isize, i32> {
    let ret = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if ret == -1 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret as isize)
    }
}

fn fadvise(fd: RawFd, offset: i64, len: i64, advice: usize) -> i32 {
    match raw(
        libc::SYS_fadvise64,
        fd as usize,
        offset as usize,
        len as usize,
        advice,
    ) {
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

    let path = format!("/tmp/mpos-fadvise-{}", std::process::id());
    let _ = std::fs::remove_file(&path);
    if std::fs::write(&path, b"0123456789abcdef").is_err() {
        r.check("a file to advise about is created", false);
        r.finish();
    }
    let regular = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            r.check("the file opens", false);
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
    let mut pair = [0 as RawFd; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
        r.check("a socketpair is made", false);
        r.finish();
    }

    let normal = fadvise(regular.as_raw_fd(), 0, 4096, POSIX_FADV_NORMAL);
    want(&mut r, "the first advice is served", normal, OK);
    let noreuse = fadvise(regular.as_raw_fd(), 0, 4096, POSIX_FADV_NOREUSE);
    want(&mut r, "and so is the sixth", noreuse, OK);

    let past = fadvise(regular.as_raw_fd(), 0, 4096, 6);
    want(&mut r, "an advice past the set is refused", past, EINVAL);
    let negative = fadvise(regular.as_raw_fd(), 0, 4096, 0xffff_ffff);
    want(
        &mut r,
        "a sign-extended negative advice too",
        negative,
        EINVAL,
    );

    let wide_ok = fadvise(
        regular.as_raw_fd(),
        0,
        4096,
        0x1_0000_0000 | POSIX_FADV_NORMAL,
    );
    want(
        &mut r,
        "the advice is the low 32 bits of the register",
        wide_ok,
        OK,
    );
    let wide_bad = fadvise(regular.as_raw_fd(), 0, 4096, 0x1_0000_0006);
    want(&mut r, "and so is the refusal it earns", wide_bad, EINVAL);

    let negative_offset = fadvise(regular.as_raw_fd(), -1, 4096, POSIX_FADV_NORMAL);
    want(
        &mut r,
        "a negative offset is refused",
        negative_offset,
        EINVAL,
    );
    let negative_len = fadvise(regular.as_raw_fd(), 0, -1, POSIX_FADV_NORMAL);
    want(&mut r, "so is a negative length", negative_len, EINVAL);
    let backwards = fadvise(regular.as_raw_fd(), -4096, 4096, POSIX_FADV_NORMAL);
    want(
        &mut r,
        "and so is a range that ends before it starts",
        backwards,
        EINVAL,
    );

    let wrapping = fadvise(regular.as_raw_fd(), HIGH, HIGH, POSIX_FADV_NORMAL);
    want(
        &mut r,
        "a range whose sum wraps is still a hint",
        wrapping,
        OK,
    );
    let enormous = fadvise(regular.as_raw_fd(), 0, i64::MAX, POSIX_FADV_NORMAL);
    want(&mut r, "and so is an enormous one", enormous, OK);

    let pipe_plain = fadvise(pipe[0], 0, 4096, POSIX_FADV_NORMAL);
    want(
        &mut r,
        "a pipe has no range to advise about",
        pipe_plain,
        ESPIPE,
    );

    let pipe_bad_advice = fadvise(pipe[0], 0, 4096, 9);
    want(
        &mut r,
        "the kind is judged before the advice",
        pipe_bad_advice,
        ESPIPE,
    );
    let pipe_bad_range = fadvise(pipe[0], -1, 4096, POSIX_FADV_NORMAL);
    want(&mut r, "and before the range", pipe_bad_range, ESPIPE);

    let pair_plain = fadvise(pair[0], 0, 4096, POSIX_FADV_NORMAL);
    want(
        &mut r,
        "a socketpair is not a pipe, so it is served",
        pair_plain,
        OK,
    );
    let char_plain = fadvise(chardev.as_raw_fd(), 0, 4096, POSIX_FADV_NORMAL);
    want(&mut r, "and neither is a character device", char_plain, OK);
    let dir_plain = fadvise(dir.as_raw_fd(), 0, 4096, POSIX_FADV_NORMAL);
    want(&mut r, "a directory is served too", dir_plain, OK);

    let missing = fadvise(BAD, 0, 4096, 9);
    want(
        &mut r,
        "the descriptor is judged before anything else",
        missing,
        EBADF,
    );

    let _ = std::fs::remove_file(&path);
    r.finish();
}
