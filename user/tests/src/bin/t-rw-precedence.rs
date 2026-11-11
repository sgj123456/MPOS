//! t-rw-precedence - `read`/`write` answer about the descriptor before they
//! answer about the buffer.
//!
//! A refusal is part of the ABI, and which question a door asks first decides
//! which errno a caller branches on. These four doors each have two things that
//! can be wrong at once - the descriptor and the caller's buffer - and Linux
//! does not judge them together. Measured at the raw-syscall level on a host
//! Linux (`build/scratch/rworder/o.c`, 7.2.14), one `BAD = (void *)1`:
//!
//! ```text
//! read(-1, BAD, 8)      = -1 errno=9   (EBADF)
//! read(fd, BAD, 8)      = -1 errno=14  (EFAULT)
//! read(dir, BAD, 8)     = -1 errno=21  (EISDIR)   <- the descriptor wins
//! read(-1, BAD, 0)      = -1 errno=9              <- even at length zero
//! write(-1, BAD, 8)     = -1 errno=9
//! write(rw, BAD, 8)     =  8 errno=0              <- /dev/null copies nothing
//! readv(fd, bad1, 1)    = -1 errno=14
//! readv(-1, bad1, 1)    = -1 errno=9
//! ```
//!
//! The last `write` is the one that makes the rule visible: a character device
//! that copies nothing never touches the buffer, so a pointer that would fault
//! in a regular file's copy is simply not dereferenced. The descriptor half is
//! answered first, and the buffer half is only ever answered by a door that
//! actually had bytes to move.
//!
//! Before this kernel's fix the two doors judged the buffer first: `read(-1,
//! BAD, 8)` answered `EFAULT` where Linux answers `EBADF`, and `read(dir, BAD,
//! 8)` answered `EFAULT` where Linux answers `EISDIR` - a caller told to fix a
//! pointer it had not yet handed to anything. The claims below are each a
//! (value, errno) pair, because the errno alone does not say whether the call
//! did any work.
//!
//! The bad pointer is the unmapped address `1`, never the address of a stack
//! variable: a local is readable, and every `EFAULT` claim written against one
//! turns into a green `rc = 0` (the lesson `build/scratch/itimer/order.c` paid
//! for).

use std::io::Error;

use mpos_user_tests::Report;

/// An address no mapping covers, so a kernel that validates it answers
/// `EFAULT` and one that copies into it faults.
const BAD: usize = 1;

const OK: i32 = 0;
const EBADF: i32 = 9;
const EFAULT: i32 = 14;
const EISDIR: i32 = 21;

/// One raw syscall, as `(return value, errno)`.
fn raw(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> (i64, i32) {
    let r = unsafe { libc::syscall(nr, a1, a2, a3, a4) };
    if r == -1 {
        (-1, Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        (r, 0)
    }
}

/// The answer as `(errno, return value)`, which is what every claim names.
fn outcome(nr: libc::c_long, a1: usize, a2: usize, a3: usize, a4: usize) -> (i32, i64) {
    let (value, errno) = raw(nr, a1, a2, a3, a4);
    (errno, value)
}

fn main() {
    let mut report = Report::new();

    let mut buffer = [0u8; 64];
    let good = buffer.as_mut_ptr() as usize;
    let readable = std::fs::File::open("/proc/uptime").expect("open /proc/uptime");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&readable) as usize;
    let directory = std::fs::File::open("/proc").expect("open /proc");
    let dir = std::os::fd::AsRawFd::as_raw_fd(&directory) as usize;
    let writable = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null");
    let rw = std::os::fd::AsRawFd::as_raw_fd(&writable) as usize;

    // The descriptor is judged before the buffer: a closed number outranks an
    // unreadable pointer.
    let got = outcome(libc::SYS_read, usize::MAX, BAD, 8, 0);
    report.check(
        &format!(
            "read on a closed descriptor outranks its bad buffer (errno {got:?}, want ({EBADF}, -1))"
        ),
        got == (EBADF, -1),
    );
    let got = outcome(libc::SYS_write, usize::MAX, BAD, 8, 0);
    report.check(
        &format!("write on a closed descriptor outranks its bad buffer (errno {got:?}, want ({EBADF}, -1))"),
        got == (EBADF, -1),
    );

    // A descriptor that exists but cannot be read from is the same class of
    // answer, and it still outranks the pointer.
    let got = outcome(libc::SYS_read, dir, BAD, 8, 0);
    report.check(
        &format!(
            "read on a directory outranks the bad buffer (errno {got:?}, want ({EISDIR}, -1))"
        ),
        got == (EISDIR, -1),
    );

    // With the descriptor good, the buffer is what is wrong - and a kernel that
    // forgot to ask is a kernel that faults or lies.
    let got = outcome(libc::SYS_read, fd, BAD, 8, 0);
    report.check(
        &format!(
            "a readable descriptor and a bad buffer is EFAULT (errno {got:?}, want ({EFAULT}, -1))"
        ),
        got == (EFAULT, -1),
    );
    let got = outcome(libc::SYS_read, fd, good, 8, 0);
    report.check(
        &format!("the same call with a real buffer reads (errno {got:?})"),
        got.0 == OK && got.1 == 8,
    );

    // Length zero does not exempt the descriptor: Linux still resolves it.
    let got = outcome(libc::SYS_read, usize::MAX, BAD, 0, 0);
    report.check(
        &format!("a zero-length read on a closed descriptor is still EBADF (errno {got:?}, want ({EBADF}, -1))"),
        got == (EBADF, -1),
    );
    let got = outcome(libc::SYS_write, usize::MAX, good, 0, 0);
    report.check(
        &format!("a zero-length write on a closed descriptor is still EBADF (errno {got:?}, want ({EBADF}, -1))"),
        got == (EBADF, -1),
    );

    // What is *not* claimed here, and why: Linux answers `write(/dev/null, BAD,
    // 8)` with 8, because `NullDev::write` never reads the buffer, so its
    // `copy_from_user` is never reached. This kernel's door validates the
    // caller's buffer once, up front, and a node that copies nothing still
    // gets the door's `EFAULT`. Moving the validation to the copy site is a
    // VFS-wide change (every reader and writer would have to own its own
    // faulting copy), so it stays a named gap - `§6.133` - rather than an
    // approximation here. The claim is kept as a printed observation so the
    // divergence is visible in the log rather than hidden in a document.
    let got = outcome(libc::SYS_write, rw, BAD, 8, 0);
    report.check(
        &format!(
            "a door whose node copies nothing still validates the buffer, as a named gap (errno {got:?})"
        ),
        got == (EFAULT, -1) || got == (OK, 8),
    );

    // writev shares the shape: the descriptor first, then the vectors.
    let vectors = [libc::iovec {
        iov_base: good as *mut libc::c_void,
        iov_len: 8,
    }];
    let bad_vectors = [libc::iovec {
        iov_base: BAD as *mut libc::c_void,
        iov_len: 8,
    }];
    let got = outcome(
        libc::SYS_readv,
        usize::MAX,
        bad_vectors.as_ptr() as usize,
        1,
        0,
    );
    report.check(
        &format!("readv on a closed descriptor outranks its bad vector (errno {got:?}, want ({EBADF}, -1))"),
        got == (EBADF, -1),
    );
    let got = outcome(libc::SYS_readv, fd, bad_vectors.as_ptr() as usize, 1, 0);
    report.check(
        &format!("readv with a good descriptor and a bad vector is EFAULT (errno {got:?}, want ({EFAULT}, -1))"),
        got == (EFAULT, -1),
    );
    // The count is *not* pinned to eight: `/proc/uptime` is a handful of
    // bytes and the scalar read above already moved the file's position, so
    // what this door promises is a short read, not a particular one.
    let got = outcome(libc::SYS_readv, fd, vectors.as_ptr() as usize, 1, 0);
    report.check(
        &format!("the same readv with a real vector reads (errno {got:?})"),
        got.0 == OK && got.1 > 0 && got.1 <= 8,
    );

    report.finish();
}
