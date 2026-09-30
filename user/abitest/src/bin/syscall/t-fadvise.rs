//! t-fadvise - `posix_fadvise`'s advice vocabulary is closed, not a blanket
//! accept.
//!
//! `posix_fadvise` is a hint, so a *recognised* advice answered `0` is honest
//! even on a kernel that prefetches nothing. The failure this pins is the other
//! half: the call used to answer `0` for **any** advice on **any** open
//! descriptor - a number no one gave it, and a pipe, which has no byte range to
//! advise about. That is the §4.2 success-for-a-thing-not-done: the caller was
//! told the kernel had accepted a request it was never asked to keep.
//!
//! Measured on a current host (glibc `posix_fadvise` on x86_64, whose raw
//! syscall answers `0` or `-errno` directly):
//!
//! * advice `NORMAL(0)`, `DONTNEED(4)`, `NOREUSE(5)` - the recognised set
//!   `0..=5` - answers `0`;
//! * advice `6` and a past-the-set number like `99` answer `EINVAL`;
//! * a descriptor that is not open answers `EBADF`;
//! * a pipe answers `ESPIPE`, not the `0` a page-cacheable file would get.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fadvise\n");
    let mut failed = false;

    // A memfd is a regular file, so a recognised advice is accepted as the
    // hint it is.
    let fd = lin::memfd_create(b"t-fadvise\0", 0) as i32;
    lin::check_true(
        &mut failed,
        b"memfd_create hands back a descriptor",
        fd >= 0,
    );

    lin::check_eq(
        &mut failed,
        b"POSIX_FADV_NORMAL (0) on a regular file answers 0",
        lin::fadvise64(fd, 0, 0, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"POSIX_FADV_DONTNEED (4) on a regular file answers 0",
        lin::fadvise64(fd, 0, 0, 4),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"POSIX_FADV_NOREUSE (5) - the last recognised advice - answers 0",
        lin::fadvise64(fd, 0, 0, 5),
        0,
    );

    // The vocabulary is closed: the first number past the recognised set, and a
    // number far past it, are EINVAL. Both were a silent 0 before this shipped.
    lin::check_eq(
        &mut failed,
        b"advice 6 (past NOREUSE) answers EINVAL, not a silent 0",
        lin::fadvise64(fd, 0, 0, 6),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"advice 99 answers EINVAL",
        lin::fadvise64(fd, 0, 0, 99),
        lin::EINVAL,
    );

    // A descriptor that is not open is EBADF.
    lin::check_eq(
        &mut failed,
        b"an unopened descriptor is EBADF even with a valid advice",
        lin::fadvise64(0x3FFF, 0, 0, 0),
        lin::EBADF,
    );

    // A pipe has no byte range to advise about, so even a recognised advice is
    // ESPIPE rather than the 0 a regular file gets. This was a silent 0 before.
    let mut fds = [-1i32, -1i32];
    lin::check_eq(&mut failed, b"pipe2 succeeds", lin::pipe2(&mut fds, 0), 0);
    if fds[0] >= 0 {
        lin::check_eq(
            &mut failed,
            b"a pipe is ESPIPE, not the 0 a page-cacheable file gets",
            lin::fadvise64(fds[0], 0, 0, 0),
            lin::ESPIPE,
        );
    }

    let _ = lin::close(fd);
    if fds[0] >= 0 {
        let _ = lin::close(fds[0]);
        let _ = lin::close(fds[1]);
    }
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fadvise: panicked\n")
}
