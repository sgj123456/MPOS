//! t-tee - `tee(2)`: duplicate a pipe's bytes into another pipe **without
//! consuming the source**.
//!
//! It was not implemented at all: the number was never declared, so the dispatch
//! answered `ENOSYS`. That is honest for an unknown number and wrong for the pair
//! to `splice` - a program that wants one stream both read locally and forwarded
//! reaches for this rather than reading and writing.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{tee1,tee2,tee3}.c`) before
//! any claim:
//!
//! | what | Linux |
//! |---|---|
//! | `tee(pipe, pipe, n)` | `n` bytes copied; the **source is unchanged** |
//! | a non-pipe source, readable | `EINVAL` |
//! | a non-pipe source, not readable | `EBADF` |
//! | a non-pipe destination, writable | `EINVAL` |
//! | a non-pipe destination, not writable | `EBADF` |
//! | a missing descriptor | `EBADF` |
//! | an unknown flag bit | `EINVAL`, **even at `len == 0`** |
//! | `len == 0` with good flags | `0` |
//! | `SPLICE_F_MOVE` / `SPLICE_F_NONBLOCK` | served |
//!
//! **The order is the finding**: both mode questions come before both pipe
//! questions. A draft that asked the source's mode, then the flags, then the pipe
//! questions, then the destination's mode answered `EINVAL` for
//! `tee(pipe, O_RDONLY file)` - a descriptor it should have refused as unopenable
//! for writing. The mode is the descriptor's own state; the pipe question is
//! about the node.
//!
//! Numbers from libc's table (`rustix` binds neither `tee` nor `splice`).

use mpos_user_tests::Report;

const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;

fn tee(in_fd: i32, out_fd: i32, len: usize, flags: u32) -> isize {
    unsafe { libc::syscall(libc::SYS_tee, in_fd, out_fd, len, flags) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}

/// A pipe pair, or `None` if the system refuses to make one.
fn pipe() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    (unsafe { libc::pipe(fds.as_mut_ptr()) } == 0).then_some((fds[0], fds[1]))
}

fn main() {
    println!("t-tee");
    let mut r = Report::new();

    let Some((src_r, src_w)) = pipe() else {
        r.check("a source pipe is made", false);
        r.finish();
    };
    let Some((dst_r, dst_w)) = pipe() else {
        r.check("a destination pipe is made", false);
        r.finish();
    };
    r.check("two pipe pairs are made", true);

    // === the copy, and what it does not do =================================
    // SAFETY: every descriptor is one this test just made.
    let wrote = unsafe { libc::write(src_w, b"HELLO".as_ptr().cast::<libc::c_void>(), 5) };
    r.check(&format!("the source takes bytes ({wrote})"), wrote == 5);
    let copied = tee(src_r, dst_w, 5, 0);
    r.check(&format!("tee copies the bytes ({copied})"), copied == 5);
    // **The claim that separates `tee` from `read`+`write`**: the source still
    // has everything it had.
    let mut buf = [0u8; 8];
    // SAFETY: `src_r` is this test's pipe; `buf` is 8 bytes of its own stack.
    let from_src = unsafe { libc::read(src_r, buf.as_mut_ptr().cast::<libc::c_void>(), 5) };
    r.check(
        &format!("and the source is NOT consumed ({from_src} bytes)"),
        from_src == 5,
    );
    r.check(
        &format!("the source's bytes are intact ({:?})", &buf[..5]),
        &buf[..from_src.max(0) as usize] == b"HELLO",
    );
    // SAFETY: as above, on the destination.
    let from_dst = unsafe { libc::read(dst_r, buf.as_mut_ptr().cast::<libc::c_void>(), 5) };
    r.check(
        &format!("and the destination got a copy ({from_dst} bytes)"),
        from_dst == 5,
    );
    r.check(
        &format!("whose bytes match ({:?})", &buf[..5]),
        &buf[..from_dst.max(0) as usize] == b"HELLO",
    );

    // === the order: mode before node kind ==================================
    //
    // A writable non-pipe on either side is `EINVAL`; an **unopenable** one is
    // `EBADF`, and that answer comes first. The four combinations are the claim.
    let file = std::ffi::CString::new("/tmp/t-tee.bin").unwrap();
    // SAFETY: a path this test owns.
    let rdwr = unsafe {
        libc::open(
            file.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    // SAFETY: same path, read-only.
    let rdonly = unsafe { libc::open(file.as_ptr(), libc::O_RDONLY) };
    // SAFETY: same path, write-only.
    let wronly = unsafe { libc::open(file.as_ptr(), libc::O_WRONLY) };
    r.check(
        &format!("the three file descriptors are opened ({rdwr}, {rdonly}, {wronly})"),
        rdwr >= 0 && rdonly >= 0 && wronly >= 0,
    );
    r.check(
        "tee(RDWR file, pipe) is EINVAL - readable, not a pipe",
        refused(tee(rdwr, dst_w, 5, 0), libc::EINVAL),
    );
    r.check(
        "tee(WRONLY file, pipe) is EBADF - not readable at all",
        refused(tee(wronly, dst_w, 5, 0), libc::EBADF),
    );
    r.check(
        "tee(pipe, RDWR file) is EINVAL - writable, not a pipe",
        refused(tee(src_r, rdwr, 5, 0), libc::EINVAL),
    );
    r.check(
        "tee(pipe, RDONLY file) is EBADF - not writable at all",
        refused(tee(src_r, rdonly, 5, 0), libc::EBADF),
    );
    r.check(
        "a missing descriptor is EBADF",
        refused(tee(-1, dst_w, 5, 0), libc::EBADF),
    );

    // === flags and length ==================================================
    // The flag word is judged **before** the length: an unknown bit is `EINVAL`
    // even when the call would transfer nothing.
    r.check(
        "an unknown flag bit is EINVAL",
        refused(tee(src_r, dst_w, 5, 0x99), libc::EINVAL),
    );
    r.check(
        "and it is EINVAL even at len 0",
        refused(tee(src_r, dst_w, 0, 0x99), libc::EINVAL),
    );
    r.check(
        "a legitimate zero length answers 0",
        tee(src_r, dst_w, 0, 0) == 0,
    );
    // The two served flags, with a source that has something to give.
    for (label, flags) in [
        ("SPLICE_F_MOVE", SPLICE_F_MOVE),
        ("SPLICE_F_NONBLOCK", SPLICE_F_NONBLOCK),
    ] {
        // SAFETY: `src_w` is this test's pipe.
        let w = unsafe { libc::write(src_w, b"abcde".as_ptr().cast::<libc::c_void>(), 5) };
        let rc = tee(src_r, dst_w, 5, flags);
        r.check(
            &format!("{label} is served ({rc}, wrote {w})"),
            w == 5 && rc == 5,
        );
    }

    for fd in [src_r, src_w, dst_r, dst_w, rdwr, rdonly, wronly] {
        if fd >= 0 {
            // SAFETY: each is a descriptor this test made and has not closed.
            unsafe { libc::close(fd) };
        }
    }
    let _ = std::fs::remove_file("/tmp/t-tee.bin");
    r.finish()
}
