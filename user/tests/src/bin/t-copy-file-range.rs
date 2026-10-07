//! t-copy-file-range - the copy the kernel makes.
//!
//! Pinned here, per copy_file_range(2): both ends must be regular files
//! (a pipe is the man page's EINVAL), a non-zero flags word is EINVAL, a
//! copy into the same file at an overlapping range is EINVAL, the offsets
//! are in-out loff_t words that advance by the copy, and the null-offset
//! spelling is the descriptors' own cursors, which the copy moves.
//!
//! `rustix::fs::copy_file_range` binds the call but takes its offsets as
//! `&mut u64` and its length as a plain count - it cannot spell the *null*
//! offset pair the cursor-mode claim is about, and it hands back a `usize`
//! where the refusal set lives. So the cursor-mode claims ride the raw door
//! (clause (c)); the explicit-offset half rides rustix beside them, because
//! that shape is exactly what the wrapper promises. The numbers come from
//! libc's table (`SYS_copy_file_range`: 326 / 287).

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd};
use rustix::fs::{CWD, OFlags, SeekFrom, copy_file_range, unlinkat};
use rustix::io::{read, write};
use rustix::pipe::pipe;

const SRC: &str = "/tmp/t_cfr_src";
const DST: &str = "/tmp/t_cfr_dst";

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn cfr_raw(
    fd_in: i32,
    off_in: Option<&mut i64>,
    fd_out: i32,
    off_out: Option<&mut i64>,
    len: usize,
    flags: u64,
) -> isize {
    let p_in = off_in.map_or(0usize, |o| o as *mut i64 as usize);
    let p_out = off_out.map_or(0usize, |o| o as *mut i64 as usize);
    unsafe {
        libc::syscall(
            libc::SYS_copy_file_range,
            fd_in,
            p_in,
            fd_out,
            p_out,
            len,
            flags,
        ) as isize
    }
}

fn main() {
    println!("t-copy-file-range");
    let mut r = Report::new();

    let _ = std::fs::remove_file(SRC);
    let _ = std::fs::remove_file(DST);
    let mode = rustix::fs::Mode::from_bits_retain(0o644);
    let src = rustix::fs::open(SRC, OFlags::CREATE | OFlags::RDWR, mode);
    let dst = rustix::fs::open(DST, OFlags::CREATE | OFlags::RDWR, mode);
    let (src, dst) = match (src, dst) {
        (Ok(s), Ok(d)) => {
            r.check("the source is created", true);
            r.check("the destination is created", true);
            (s, d)
        }
        _ => {
            r.check("the source is created", false);
            r.finish();
        }
    };

    r.check(
        "the source takes 11 bytes",
        write(&src, b"hello world") == Ok(11),
    );

    // The explicit-offset spelling through rustix: both words name byte 0,
    // and the copy hands back the full length and moves them to its end.
    let mut off_in: u64 = 0;
    let mut off_out: u64 = 0;
    let n = copy_file_range(&src, Some(&mut off_in), &dst, Some(&mut off_out), 11);
    r.check("the copy takes all 11 bytes", matches!(n, Ok(11)));
    r.check("the source offset advanced", off_in == 11);
    r.check("the destination offset advanced", off_out == 11);

    // What landed is what was copied, read back from byte 0.
    let mut buf = [0u8; 16];
    let back = rustix::fs::seek(&dst, SeekFrom::Start(0)).and_then(|_| read(&dst, &mut buf));
    match back {
        Ok(11) => {
            r.check("the destination reads back 11 bytes", true);
            r.check(
                "the bytes that arrived are the bytes copied",
                &buf[..11] == b"hello world".as_slice(),
            );
        }
        _ => {
            r.check("the destination reads back 11 bytes", false);
            r.check("the bytes that arrived are the bytes copied", false);
        }
    }

    // A non-zero flags word is the man page's own EINVAL.
    let mut oi: i64 = 0;
    let mut oo: i64 = 0;
    r.check(
        "a non-zero flags word answers EINVAL",
        refused(
            cfr_raw(
                src.as_raw_fd(),
                Some(&mut oi),
                dst.as_raw_fd(),
                Some(&mut oo),
                1,
                1,
            ),
            libc::EINVAL,
        ),
    );

    // The same file, overlapping ranges: the one way a copy corrupts what
    // it is copying, and EINVAL is the man page's answer.
    let mut a: i64 = 0;
    let mut b: i64 = 5;
    r.check(
        "an overlapping self-copy answers EINVAL",
        refused(
            cfr_raw(
                src.as_raw_fd(),
                Some(&mut a),
                src.as_raw_fd(),
                Some(&mut b),
                8,
                0,
            ),
            libc::EINVAL,
        ),
    );

    // A descriptor that is not a regular file: the pipe is EINVAL.
    let (pr, pw) = match pipe() {
        Ok(p) => p,
        Err(_) => {
            r.check("the pipe pair is made", false);
            r.finish();
        }
    };
    r.check("the pipe pair is made", true);
    let mut a: i64 = 0;
    let mut b: i64 = 0;
    r.check(
        "a pipe source answers EINVAL",
        refused(
            cfr_raw(
                pr.as_raw_fd(),
                Some(&mut a),
                dst.as_raw_fd(),
                Some(&mut b),
                1,
                0,
            ),
            libc::EINVAL,
        ),
    );
    r.check(
        "a pipe destination answers EINVAL",
        refused(
            cfr_raw(
                src.as_raw_fd(),
                Some(&mut a),
                pw.as_raw_fd(),
                Some(&mut b),
                1,
                0,
            ),
            libc::EINVAL,
        ),
    );

    // The cursor spelling: no offset words, the descriptors' own positions
    // name the ranges, and the copy moves them.
    let _ = rustix::fs::seek(&src, SeekFrom::Start(0));
    let _ = rustix::fs::seek(&dst, SeekFrom::Start(0));
    let n = cfr_raw(
        src.as_fd().as_raw_fd(),
        None,
        dst.as_fd().as_raw_fd(),
        None,
        5,
        0,
    );
    r.check("the cursor-mode copy takes 5 bytes", n == 5);
    r.check(
        "the source cursor advanced",
        rustix::fs::seek(&src, SeekFrom::Current(0)) == Ok(5),
    );
    r.check(
        "the destination cursor advanced",
        rustix::fs::seek(&dst, SeekFrom::Current(0)) == Ok(5),
    );
    r.check(
        "a closed descriptor answers EBADF",
        refused(cfr_raw(99, None, dst.as_raw_fd(), None, 1, 0), libc::EBADF),
    );

    drop((pr, pw));
    drop(src);
    drop(dst);
    let _ = unlinkat(CWD, SRC, rustix::fs::AtFlags::empty());
    let _ = unlinkat(CWD, DST, rustix::fs::AtFlags::empty());
    r.finish()
}
