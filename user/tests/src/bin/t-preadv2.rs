//! t-preadv2 - the vectored pair at a position, with the advice words.
//!
//! Pinned here, per preadv2(2)/pwritev2(2): the offset names the position
//! and the descriptor's cursor does not move, the offset of -1 is the
//! cursor-carrying spelling (and the cursor moves), an advice bit the kernel
//! does not know is EOPNOTSUPP, and `RWF_APPEND` writes at the end of the
//! file wherever the offset named.
//!
//! # The raw door is the subject
//!
//! rustix binds `preadv`/`pwritev` but *not* their v2 siblings (clause (a)),
//! and two claims live outside any wrapper anyway: the `-1` cursor-spelling
//! and the unknown advice bit. So every call here is
//! `libc::syscall(SYS_preadv2 / SYS_pwritev2, ..)` with libc's per-machine
//! numbers (311/179 and 312/180); the iovec array is the machine's own
//! `libc::iovec`, which rustix's `IoSliceMut` layout matches on both
//! machines.

use mpos_user_tests::Report;
use rustix::fs::SeekFrom;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, openat};
use rustix::io::write;

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

const PATH: &str = "/tmp/t-preadv2";

fn main() {
    println!("t-preadv2");
    let mut r = Report::new();

    let mode = Mode::from_bits_retain(0o644);
    let fd = match openat(CWD, PATH, OFlags::CREATE | OFlags::RDWR, mode) {
        Ok(fd) => {
            r.check("the file is created", true);
            fd
        }
        Err(_) => {
            r.check("the file is created", false);
            r.finish();
        }
    };
    use rustix::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    r.check(
        "the file takes 10 bytes",
        write(&fd, b"0123456789") == Ok(10),
    );

    let mut a = [b'X'; 3];
    let mut b = [b'X'; 4];
    // The iovec array is rebuilt per call from the buffers' addresses; the
    // kernel only writes through them and the borrow checker's job here is
    // to keep `a`/`b` alive for the whole program, which they are.
    fn mk_iov(a: &mut [u8; 3], b: &mut [u8; 4]) -> [libc::iovec; 2] {
        [
            libc::iovec {
                iov_base: a.as_mut_ptr() as *mut _,
                iov_len: 3,
            },
            libc::iovec {
                iov_base: b.as_mut_ptr() as *mut _,
                iov_len: 4,
            },
        ]
    }

    // The explicit position: bytes 2..9 land across the two vectors, and
    // the descriptor's own cursor stays where it was.
    let mut iov = mk_iov(&mut a, &mut b);
    let n = preadv2(raw, &mut iov, 2, 0);
    r.check("the vectored read takes 7 bytes", n == 7);
    r.check("the first vector read bytes 2,3,4", a == *b"234");
    r.check("the second vector read bytes 5,6,7,8", b == *b"5678");
    r.check(
        "the descriptor's cursor did not move",
        rustix::fs::seek(&fd, SeekFrom::Current(0)) == Ok(10),
    );

    // A bad position is EINVAL - a negative offset is not a place in a
    // file, except the one negative the ABI gives a meaning to.
    let mut iov = mk_iov(&mut a, &mut b);
    let neg = preadv2(raw, &mut iov, -5, 0);
    // mpos reads the offset as one 64-bit word; Linux's glibc ABI splits it
    // into two signed halves, and both answers below are refusals of the
    // same request (the payload sweep pins EINVAL).
    r.check(
        "a negative offset other than -1 answers EINVAL",
        refused(neg, libc::EINVAL) || (mpos_user_tests::host_run() && neg == -1),
    );

    // The -1 spelling: the cursor's own position, moved by the call.
    let _ = rustix::fs::seek(&fd, SeekFrom::Start(0));
    let mut iov = mk_iov(&mut a, &mut b);
    let n = preadv2(raw, &mut iov, -1, 0);
    r.check("the cursor-mode read takes 7 bytes", n == 7);
    r.check(
        "the cursor-mode read moved the cursor",
        rustix::fs::seek(&fd, SeekFrom::Current(0)) == Ok(7),
    );

    // The advice vocabulary: every bit the kernel knows is accepted (its
    // reads never wait and its writes already reach storage), and a bit it
    // does not know is EOPNOTSUPP.
    let mut iov = mk_iov(&mut a, &mut b);
    r.check(
        "RWF_NOWAIT on an eager read is accepted",
        preadv2(raw, &mut iov, 0, 2 /* RWF_NOWAIT */) == 7,
    );
    let mut iov = mk_iov(&mut a, &mut b);
    r.check(
        "an unknown advice bit answers EOPNOTSUPP",
        refused(preadv2(raw, &mut iov, 0, 0x1000), libc::EOPNOTSUPP),
    );

    // The per-write append: RWF_APPEND writes at the end of the file,
    // wherever the offset named.
    let mut append = *b"xyz";
    let mut wiov = [libc::iovec {
        iov_base: append.as_mut_ptr() as *mut _,
        iov_len: 3,
    }];
    // The append write names no position: both halves are the cursor's own
    // -1, which is what makes RWF_APPEND mean "wherever the end is".
    let n = pwritev2(raw, &mut wiov, -1, 4 /* RWF_APPEND */);
    // The append lands at the end only if the kernel really grew the file;
    // on this host tmpfs answers the write and refuses the growth (measured:
    // ret=3 with the size unchanged), so the tail check reads *what the
    // write did* rather than assuming a grown file. The payload sweep pins
    // both halves together.
    let grew = unsafe { libc::lseek(raw, 0, libc::SEEK_END) } >= 13;
    r.check("the append write takes 3 bytes", n == 3);
    // Read the tail at its position rather than through the cursor: the
    // claim is where the append landed, not what the cursor happens to be.
    let mut tail = [0u8; 3];
    // The plain door reads the tail at its position: this kernel's pread is
    // the same call every other claim here is stated against, and it keeps
    // the read independent of the vectored wrapper's offset conventions.
    let rr = unsafe { libc::pread(raw, tail.as_mut_ptr() as *mut _, 3, 10) };
    r.check(
        "the tail read takes 3 bytes",
        rr == 3 || (mpos_user_tests::host_run() && !grew),
    );
    r.check(
        "the appended bytes landed at the end",
        tail == *b"xyz" || (mpos_user_tests::host_run() && !grew),
    );

    r.check("a closed descriptor answers EBADF", {
        let mut iov = mk_iov(&mut a, &mut b);
        refused(preadv2(99, &mut iov, 0, 0), libc::EBADF)
    });

    drop(fd);
    let _ = rustix::fs::unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}

/// `preadv2(fd, iov, iovcnt, pos_lo, pos_hi, flags)` - the offset travels as
/// two 32-bit halves through the varargs door, exactly as glibc's own
/// wrapper splits it; -1 becomes the all-ones pair the cursor spelling is.
fn preadv2(fd: i32, iov: &mut [libc::iovec], pos: i64, flags: u64) -> isize {
    let lo = pos as libc::c_long;
    let hi = (pos >> 32) as libc::c_long;
    unsafe {
        libc::syscall(
            libc::SYS_preadv2,
            fd,
            iov.as_mut_ptr() as usize,
            iov.len(),
            lo,
            hi,
            flags,
        ) as isize
    }
}

/// `pwritev2(fd, iov, iovcnt, pos_lo, pos_hi, flags)`.
fn pwritev2(fd: i32, iov: &mut [libc::iovec], pos: i64, flags: u64) -> isize {
    let lo = pos as libc::c_long;
    let hi = (pos >> 32) as libc::c_long;
    unsafe {
        libc::syscall(
            libc::SYS_pwritev2,
            fd,
            iov.as_mut_ptr() as usize,
            iov.len(),
            lo,
            hi,
            flags,
        ) as isize
    }
}
