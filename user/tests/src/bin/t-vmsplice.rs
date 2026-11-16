//! t-vmsplice - `vmsplice(2)`: move bytes between user memory and a pipe.
//!
//! The last of item 310's four census calls (after `execveat`, `ioprio_*` and
//! `tee`), and the only one whose contract is about *memory*. It was not
//! implemented at all: the number was never declared, so the dispatch answered
//! `ENOSYS`.
//!
//! Measured on Linux 7.3.4 (`build/scratch/opathmmap/{vms1,vms2,vms3}.c`) before
//! any claim:
//!
//! | what | Linux |
//! |---|---|
//! | a pipe's **write** end | the iovecs' bytes land in the pipe |
//! | a pipe's **read** end | it **drains into** the iovecs |
//! | several iovecs | each contributes, in order |
//! | a regular file | `EBADF` on either side |
//! | a missing descriptor | `EBADF` |
//! | `nr_segs == 0` | `0` |
//! | more than `IOV_MAX` (1024) | `EINVAL` |
//! | an unknown flag bit | `EINVAL` |
//! | `SPLICE_F_GIFT` | **served**, no extra requirement |
//! | a zero-length iovec | skipped (an array of them answers 0) |
//! | an unreadable base | `EFAULT` |
//!
//! **The drain row is the one worth stating**: the man page describes `vmsplice`
//! as grafting *pages into* a pipe, and a read end measured first on the host
//! returns 5 and fills the buffer - so the direction is the pipe end's, and this
//! implementation does the same.
//!
//! `SPLICE_F_GIFT` is the flag whose *meaning* this kernel cannot honour: Linux
//! grafts the caller's pages so a later `splice` moves them without a copy, and
//! `GIFT` promises the caller will never write them again. This kernel's pipes
//! hold bytes, so both shapes copy - `GIFT` is accepted and ignored, which is
//! what the claim below pins (accepted, not refused).
//!
//! Numbers from libc's table (`rustix` binds neither `vmsplice` nor `splice`).

use mpos_user_tests::Report;

const SPLICE_F_GIFT: u32 = 0x08;

fn vmsplice(fd: i32, iov: &[libc::iovec], flags: u32) -> isize {
    unsafe { libc::syscall(libc::SYS_vmsplice, fd, iov.as_ptr(), iov.len(), flags) as isize }
}
fn errno() -> Option<i32> {
    std::io::Error::last_os_error().raw_os_error()
}
fn refused(rc: isize, want: i32) -> bool {
    rc == -1 && errno() == Some(want)
}
fn pipe() -> Option<(i32, i32)> {
    let mut fds = [0i32; 2];
    (unsafe { libc::pipe(fds.as_mut_ptr()) } == 0).then_some((fds[0], fds[1]))
}
fn iov(buf: &mut [u8], len: usize) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: len,
    }
}

fn main() {
    println!("t-vmsplice");
    let mut r = Report::new();

    let Some((pr, pw)) = pipe() else {
        r.check("a pipe pair is made", false);
        r.finish();
    };
    r.check("a pipe pair is made", true);

    // === memory into the pipe ==============================================
    let mut src = *b"HELLO\0\0\0";
    let in_iov = [iov(&mut src, 5)];
    let rc = vmsplice(pw, &in_iov, 0);
    r.check(
        &format!("vmsplice(write end) stores the bytes ({rc})"),
        rc == 5,
    );
    let mut got = [0u8; 8];
    // SAFETY: a pipe this test made and a buffer of its own.
    let n = unsafe { libc::read(pr, got.as_mut_ptr().cast::<libc::c_void>(), 5) };
    r.check(&format!("and the pipe hands them back ({n})"), n == 5);
    r.check(
        &format!("with the bytes intact ({:?})", &got[..5]),
        &got[..5] == b"HELLO",
    );

    // === the pipe into memory ==============================================
    //
    // The row the man page does not prepare you for: a **read** end drains into
    // the iovecs, and the measurement is the authority.
    // SAFETY: as above.
    let w = unsafe { libc::write(pw, b"ABCDE".as_ptr().cast::<libc::c_void>(), 5) };
    r.check(&format!("the pipe takes a second payload ({w})"), w == 5);
    let mut sink = [0u8; 8];
    let out_iov = [iov(&mut sink, 5)];
    let rc = vmsplice(pr, &out_iov, 0);
    r.check(
        &format!("vmsplice(read end) drains into the buffer ({rc})"),
        rc == 5,
    );
    r.check(
        &format!("and the buffer holds it ({:?})", &sink[..5]),
        &sink[..5] == b"ABCDE",
    );

    // === several iovecs, in order ==========================================
    let mut multi = *b"ABCDEF\0\0";
    let base = multi.as_mut_ptr();
    // SAFETY: three slices of one 8-byte array, all inside it.
    let iovs = unsafe {
        [
            libc::iovec {
                iov_base: base.cast(),
                iov_len: 2,
            },
            libc::iovec {
                iov_base: base.add(2).cast(),
                iov_len: 2,
            },
            libc::iovec {
                iov_base: base.add(4).cast(),
                iov_len: 2,
            },
        ]
    };
    let rc = vmsplice(pw, &iovs, 0);
    r.check(&format!("three 2-byte iovecs transfer 6 ({rc})"), rc == 6);
    let mut o2 = [0u8; 8];
    // SAFETY: as above.
    let n2 = unsafe { libc::read(pr, o2.as_mut_ptr().cast::<libc::c_void>(), 6) };
    r.check(
        &format!("and the order is the array's ({:?})", &o2[..6]),
        n2 == 6 && &o2[..6] == b"ABCDEF",
    );

    // === the refusals ======================================================
    let mut one = *b"XYZ\0\0";
    let one_iov = [iov(&mut one, 3)];
    r.check("a regular file is EBADF", {
        let f = std::ffi::CString::new("/tmp/t-vmsplice.out").unwrap();
        // SAFETY: a path this test owns.
        let fd = unsafe {
            libc::open(
                f.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            )
        };
        let rc = if fd >= 0 {
            vmsplice(fd, &one_iov, 0)
        } else {
            -2
        };
        if fd >= 0 {
            // SAFETY: the descriptor opened just above.
            unsafe { libc::close(fd) };
        }
        refused(rc, libc::EBADF)
    });
    r.check(
        "a missing descriptor is EBADF",
        refused(vmsplice(-1, &one_iov, 0), libc::EBADF),
    );
    r.check("nr_segs == 0 answers 0", vmsplice(pw, &[], 0) == 0);
    // `IOV_MAX` is 1024, and one past it is `EINVAL` - the count is validated
    // before any iovec is looked at.
    let many = [iov(&mut one, 1); 1025];
    r.check(
        "more than IOV_MAX iovecs is EINVAL",
        refused(vmsplice(pw, &many, 0), libc::EINVAL),
    );
    r.check(
        "an unknown flag bit is EINVAL",
        refused(vmsplice(pw, &one_iov, 0x99), libc::EINVAL),
    );
    // `SPLICE_F_GIFT` is **accepted**, and has no requirement this kernel can
    // check: Linux wants whole pages it can graft, and a copied transfer needs
    // nothing at all.
    r.check(
        &format!(
            "SPLICE_F_GIFT is accepted, not refused ({})",
            vmsplice(pw, &one_iov, SPLICE_F_GIFT)
        ),
        vmsplice(pw, &one_iov, SPLICE_F_GIFT) >= 0,
    );
    // A zero-length iovec contributes nothing and is not an error.
    let mut zero = *b"AA\0\0\0\0";
    let zero_iov = [iov(&mut zero, 0)];
    r.check(
        "a zero-length iovec answers 0",
        vmsplice(pw, &zero_iov, 0) == 0,
    );
    // An **unreadable** base is `EFAULT`, and it is asked before the transfer:
    // nothing would have been shared, but the page still cannot be read.
    let bad = [libc::iovec {
        iov_base: 0x10 as *mut libc::c_void,
        iov_len: 4,
    }];
    r.check(
        "an unreadable iovec base is EFAULT",
        refused(vmsplice(pw, &bad, 0), libc::EFAULT),
    );

    for fd in [pr, pw] {
        // SAFETY: the two ends this test made.
        unsafe { libc::close(fd) };
    }
    let _ = std::fs::remove_file("/tmp/t-vmsplice.out");
    r.finish()
}
