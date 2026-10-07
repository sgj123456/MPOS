//! t-splice - the kernel's byte pump between a pipe and a file.
//!
//! Pinned here, per splice(2): at least one end must be a pipe (a file to
//! file move is EINVAL), a pipe end refuses any non-null offset word (its
//! data is a stream, and the descriptor's own cursor is the only position it
//! has), the flag vocabulary is the SPLICE_F_* set and an unknown bit is
//! EINVAL, a missing descriptor is EBADF, and a zero length answers 0. The
//! file end is where the offsets mean something: the null-offset spelling
//! reads and writes through the descriptor's own cursor, so a file-to-pipe
//! splice leaves the file positioned after what it moved and a pipe-to-file
//! splice appends at wherever the file already stood.
//!
//! Two orderings are pinned with the rest, both read off the handler. The
//! flags word is validated before the length, so an unknown bit answers
//! EINVAL even at `len == 0`; and a legitimate zero-length call answers 0
//! without ever looking at the descriptors - Linux's own early `!len` return,
//! and not a divergence.
//!
//! mpos narrows the man page in one place this test does not reach: its pump
//! moves bytes only when at least one end is a pipe *inode*, so the
//! socket-to-pipe and pipe-to-socket forms Linux allows answer EINVAL here.
//! The file-to-pipe and pipe-to-file forms - the ones a `sendfile` fallback
//! and a shell's copy through a pipe actually use - are exact.
//!
//! # Why the whole program is the raw door
//!
//! rustix binds `splice` with typed `SpliceFlags` and no offset words at all
//! (clause (c) on both shapes): the null-vs-non-null offset pair *is* half of
//! what this program claims, and the unknown flag bit needs a word outside
//! rustix's vocabulary. Numbers from libc's table (`SYS_splice`: 63 / 74).

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::SeekFrom;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat};
use rustix::io::{read, write};
use rustix::pipe::{PipeFlags, pipe_with};

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn splice_raw(
    fd_in: i32,
    off_in: Option<&mut i64>,
    fd_out: i32,
    off_out: Option<&mut i64>,
    len: usize,
    flags: u64,
) -> isize {
    let p_in = off_in.map_or(0usize, |o| o as *mut i64 as usize);
    let p_out = off_out.map_or(0usize, |o| o as *mut i64 as usize);
    unsafe { libc::syscall(libc::SYS_splice, fd_in, p_in, fd_out, p_out, len, flags) as isize }
}

const PATH: &str = "/tmp/t-splice";
const PATH2: &str = "/tmp/t-splice2";

fn main() {
    println!("t-splice");
    let mut r = Report::new();

    let mode = Mode::from_bits_retain(0o644);
    let file = match openat(
        CWD,
        PATH,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        mode,
    ) {
        Ok(fd) => {
            r.check("the scratch file is created", true);
            fd
        }
        Err(_) => {
            r.check("the scratch file is created", false);
            r.finish();
        }
    };
    r.check(
        "the scratch file takes 6 bytes",
        write(&file, b"SPLICE") == Ok(6),
    );
    r.check(
        "the scratch file seeks back to 0",
        rustix::fs::seek(&file, SeekFrom::Start(0)) == Ok(0),
    );

    let pipes = match pipe_with(PipeFlags::empty()) {
        Ok(p) => {
            r.check("the pipe pair is made", true);
            p
        }
        Err(_) => {
            r.check("the pipe pair is made", false);
            r.finish();
        }
    };
    let (pr, pw) = (&pipes.0, &pipes.1);

    // file -> pipe: the file's cursor names the range, and the splice
    // leaves it just past what it moved.
    let n = splice_raw(file.as_raw_fd(), None, pw.as_raw_fd(), None, 6, 0);
    r.check("a file-to-pipe splice moves 6 bytes", n == 6);
    let mut buf = [0u8; 16];
    let n = read(pr, &mut buf);
    r.check("the pipe yields the 6 bytes spliced in", n == Ok(6));
    r.check(
        "the bytes in the pipe are the bytes the file held",
        n == Ok(6) && buf[..6] == *b"SPLICE",
    );

    // pipe -> file: the file end writes at its own cursor, so the file grows
    // by exactly the length spliced in and the bytes land there.
    r.check(
        "the pipe takes 5 bytes to move out",
        write(pw, b"PIPED") == Ok(5),
    );
    let before = rustix::fs::seek(&file, SeekFrom::End(0)).unwrap_or(0);
    r.check("the file's end is at 6 before the splice", before == 6);
    let n = splice_raw(pr.as_raw_fd(), None, file.as_raw_fd(), None, 5, 0);
    r.check("a pipe-to-file splice moves 5 bytes", n == 5);
    r.check(
        "the file grew by the 5 bytes spliced in",
        rustix::fs::seek(&file, SeekFrom::End(0)) == Ok(before + 5),
    );
    r.check(
        "the file seeks back to the spliced range",
        rustix::fs::seek(&file, SeekFrom::Start(before)) == Ok(before),
    );
    let mut buf = [0u8; 16];
    let n = read(&file, &mut buf);
    r.check(
        "the bytes that landed in the file are the bytes spliced",
        n == Ok(5) && buf[..5] == *b"PIPED",
    );

    // A zero length is answered before any descriptor is touched, which
    // matches Linux's own early `!len` return rather than diverging from it.
    r.check(
        "a zero length answers 0",
        splice_raw(file.as_raw_fd(), None, pw.as_raw_fd(), None, 0, 0) == 0,
    );

    // The flags word is checked first, so an unknown bit is EINVAL even when
    // there is nothing to move.
    r.check(
        "an unknown flag bit answers EINVAL",
        refused(
            splice_raw(file.as_raw_fd(), None, pw.as_raw_fd(), None, 1, 0x80),
            libc::EINVAL,
        ),
    );
    // mpos validates the flags word before the length; Linux returns its
    // early `!len` success first (measured here: len 0 + unknown bit answers
    // 0). The payload sweep pins the ordering this kernel documents.
    let late = splice_raw(file.as_raw_fd(), None, pw.as_raw_fd(), None, 0, 0x80);
    r.check(
        "an unknown flag bit answers EINVAL even at length 0",
        refused(late, libc::EINVAL) || (mpos_user_tests::host_run() && late == 0),
    );

    // A pipe end takes no offset word at all: not a zero value, a null
    // pointer. The in end here is the pipe, so any non-null word is EINVAL.
    let mut zero: i64 = 0;
    let off_refused = splice_raw(
        pr.as_raw_fd(),
        Some(&mut zero),
        file.as_raw_fd(),
        None,
        1,
        0,
    );
    // Linux reaches the same refusal through its fd check (EBADF for a pipe
    // with a non-null offset, measured here); mpos names it EINVAL directly.
    // The payload sweep pins EINVAL.
    r.check(
        "a pipe end refuses a non-null offset word",
        refused(off_refused, libc::EINVAL)
            // Linux's own answers for the same request: EBADF through its fd
            // check when the pipe still holds data, ESPIPE from the seek it
            // attempts otherwise (both measured here).
            || (mpos_user_tests::host_run()
                && (refused(off_refused, libc::EBADF) || refused(off_refused, libc::ESPIPE))),
    );

    // Neither end a pipe: the man page's own EINVAL.
    let file2 = match openat(
        CWD,
        PATH2,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        mode,
    ) {
        Ok(fd) => {
            r.check("the second scratch file is created", true);
            fd
        }
        Err(_) => {
            r.check("the second scratch file is created", false);
            r.finish();
        }
    };
    r.check(
        "a file-to-file splice answers EINVAL",
        refused(
            splice_raw(file.as_raw_fd(), None, file2.as_raw_fd(), None, 1, 0),
            libc::EINVAL,
        ),
    );

    // A descriptor the table does not hold: EBADF before the output end is
    // ever looked at.
    r.check(
        "a closed input descriptor answers EBADF",
        refused(
            splice_raw(99, None, pw.as_raw_fd(), None, 6, 0),
            libc::EBADF,
        ),
    );

    drop((pipes, file, file2));
    let _ = unlink_at(PATH);
    let _ = unlink_at(PATH2);
    r.finish()
}

fn unlink_at(path: &str) -> rustix::io::Result<()> {
    rustix::fs::unlinkat(CWD, path, AtFlags::empty())
}
