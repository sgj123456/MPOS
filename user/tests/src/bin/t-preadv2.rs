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
//! numbers (327/286 and 328/287 - the same values this kernel's own
//! `src/syscall/linux/nr_x86_64.rs` and `nr_aarch64.rs` pin); the iovec array
//! is the machine's own `libc::iovec`, which rustix's `IoSliceMut` layout
//! matches on both machines.
//!
//! # The position's shape is per-machine too
//!
//! The number is not the only thing that differs: x86_64 splits the 64-bit
//! position into two 32-bit halves and puts the advice word sixth
//! (`preadv2(fd, iov, iovcnt, pos_l, pos_h, flags)`, glibc's own spelling),
//! while arm64 passes it whole with the advice word fifth - the asm-generic
//! shape musl emits there. [`preadv2`]/[`pwritev2`] below are written once per
//! machine for that reason: handing the pair to arm64 puts the position's
//! high half where the kernel reads flags, which turns `-1` into a huge
//! positive position, hides the real flags, and made seven claims here fail
//! on a kernel that was reading the shape it should.

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

/// The advice vocabulary, as `linux/fs.h` numbers it - the same bits
/// `src/syscall/linux/fs/file.rs` masks `flags` against, so a claim that names
/// one of them really sends that one. (The first draft of this file sent
/// `2 /* RWF_NOWAIT */` and `4 /* RWF_APPEND */`, which are `RWF_DSYNC` and
/// `RWF_SYNC`: both are bits the kernel *knows*, so it accepted them without a
/// word and the append claim then read the cursor-moving path instead of the
/// append - a claim that passed for the wrong reason while the two claims
/// about where the bytes landed failed.)
const RWF_NOWAIT: u64 = 0x8;
const RWF_APPEND: u64 = 0x10;

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
        preadv2(raw, &mut iov, 0, RWF_NOWAIT) == 7,
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
    let n = pwritev2(raw, &mut wiov, -1, RWF_APPEND);
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

/// `preadv2`, spelled the way *this* machine's ABI defines it.
///
/// x86_64 keeps its 32-bit `off_t` heritage: the position travels as two
/// 32-bit halves through the varargs door, exactly as glibc's own wrapper
/// splits it, and the advice word is the *sixth* argument. -1 becomes the
/// all-ones pair the cursor spelling is.
#[cfg(target_arch = "x86_64")]
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

/// `preadv2(fd, iov, iovcnt, pos, flags)` - the asm-generic shape arm64 uses,
/// and the one musl emits there: one 64-bit position, the advice word fifth.
/// Sending the x86_64 pair instead puts the position's high half where the
/// kernel reads flags (so a negative position reads as a huge positive one
/// and `-1` stops being the cursor spelling) and hides the real advice word
/// in a slot nothing looks at.
#[cfg(not(target_arch = "x86_64"))]
fn preadv2(fd: i32, iov: &mut [libc::iovec], pos: i64, flags: u64) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_preadv2,
            fd,
            iov.as_mut_ptr() as usize,
            iov.len(),
            pos as libc::c_long,
            flags,
        ) as isize
    }
}

/// `pwritev2` - the write's half of [`preadv2`], with the same per-machine
/// argument shape.
#[cfg(target_arch = "x86_64")]
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

#[cfg(not(target_arch = "x86_64"))]
fn pwritev2(fd: i32, iov: &mut [libc::iovec], pos: i64, flags: u64) -> isize {
    unsafe {
        libc::syscall(
            libc::SYS_pwritev2,
            fd,
            iov.as_mut_ptr() as usize,
            iov.len(),
            pos as libc::c_long,
            flags,
        ) as isize
    }
}
