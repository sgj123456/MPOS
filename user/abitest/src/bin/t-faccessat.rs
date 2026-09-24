//! t-faccessat - one test for faccessat, the dirfd form of access that
//! path-probing code uses.
//!
//! Pinned here: an existing file is reachable (answer 0 - one user, every
//! permission bit set), a path that does not exist answers ENOENT, the
//! path resolves relative to a directory descriptor, and a dirfd that is
//! not open answers EBADF.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static BIN: [u8; 5] = *b"/bin\0";
static SH: [u8; 3] = *b"sh\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-faccessat\n");
    let mut failed = false;

    let r = lin::faccessat(lin::AT_FDCWD, &MOTD, lin::R_OK);
    lin::check_eq(&mut failed, b"an existing file is reachable for R_OK", r, 0);

    let r = lin::faccessat(lin::AT_FDCWD, &NO_SUCH, lin::F_OK);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    // The path resolves relative to a directory descriptor.
    let binfd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/bin opens as a dirfd", binfd >= 0);
    if binfd >= 0 {
        let r = lin::faccessat(binfd as i32, &SH, lin::F_OK);
        lin::check_eq(&mut failed, b"faccessat relative to a dirfd succeeds", r, 0);
        let _ = lin::close(binfd as i32);
    }

    // A dirfd that is not open answers EBADF.
    let r = lin::faccessat(77, &MOTD, lin::F_OK);
    lin::check_eq(&mut failed, b"a bad dirfd answers EBADF", r, lin::EBADF);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-faccessat: panicked\n")
}
