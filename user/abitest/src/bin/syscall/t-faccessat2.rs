//! t-faccessat2 - the access check with the flags the original could not
//! take.
//!
//! Pinned here, per faccessat2(2): the mode word is validated (a bit
//! outside F_OK/R_OK/W_OK/X_OK is EINVAL), the flag word too (anything
//! beyond AT_EACCESS and AT_SYMLINK_NOFOLLOW is EINVAL), X_OK answers
//! from the mode bits the filesystem keeps - an executable answers 0, a
//! plain non-executable file answers 0 for root (mpos drops Linux's one
//! `CAP_DAC_OVERRIDE` execute exception), a directory traverses - and
//! a name that is not there is still ENOENT, whatever was asked of it.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static OWN: [u8; 17] = *b"/tmp/t_facc2_own\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-faccessat2\n");
    let mut failed = false;

    // The mode word: F_OK against a directory that is always there.
    lin::check_eq(
        &mut failed,
        b"F_OK against /etc answers 0",
        lin::faccessat2(lin::AT_FDCWD, b"/etc\0", lin::F_OK, 0),
        0,
    );
    // Root's X_OK is "any execute bit": a directory has them, and the
    // check that fails is the one a shell's `test -x` is asking.
    lin::check_eq(
        &mut failed,
        b"X_OK against a directory answers 0",
        lin::faccessat2(lin::AT_FDCWD, b"/etc\0", lin::X_OK, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"X_OK against an executable answers 0",
        lin::faccessat2(lin::AT_FDCWD, b"/bin/sh\0", lin::X_OK, 0),
        0,
    );

    // A plain file the kernel stores as a plain file: no execute bit anywhere
    // in the mode. mpos drops Linux's `CAP_DAC_OVERRIDE` execute exception, so
    // root's X_OK is granted even here.
    let fd = lin::openat(lin::AT_FDCWD, &OWN, lin::O_CREAT | lin::O_RDWR, 0o644) as i32;
    lin::check_true(&mut failed, b"the plain file is created", fd >= 0);
    let _ = lin::close(fd);
    lin::check_eq(
        &mut failed,
        b"F_OK against the plain file answers 0",
        lin::faccessat2(lin::AT_FDCWD, &OWN, lin::F_OK, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"R_OK against the plain file answers 0",
        lin::faccessat2(lin::AT_FDCWD, &OWN, lin::R_OK, 0),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"X_OK against a plain file answers 0 for root",
        lin::faccessat2(lin::AT_FDCWD, &OWN, lin::X_OK, 0),
        0,
    );

    // The validations, in the man page's order of refusal.
    lin::check_eq(
        &mut failed,
        b"a mode bit outside the four answers EINVAL",
        lin::faccessat2(lin::AT_FDCWD, b"/etc\0", 8, 0),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"a flag bit outside the two answers EINVAL",
        lin::faccessat2(lin::AT_FDCWD, b"/etc\0", lin::F_OK, 0x8000),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"AT_EACCESS is accepted",
        lin::faccessat2(lin::AT_FDCWD, b"/etc\0", lin::R_OK, lin::AT_EACCESS),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"AT_SYMLINK_NOFOLLOW is accepted",
        lin::faccessat2(
            lin::AT_FDCWD,
            b"/tmp\0",
            lin::F_OK,
            lin::AT_SYMLINK_NOFOLLOW,
        ),
        0,
    );

    // A name that is not there: ENOENT, whatever was asked of it.
    lin::check_eq(
        &mut failed,
        b"a missing name answers ENOENT",
        lin::faccessat2(lin::AT_FDCWD, b"/no/such/thing\0", lin::F_OK, 0),
        lin::ENOENT,
    );

    let _ = lin::unlinkat(lin::AT_FDCWD, &OWN, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-faccessat2: panicked\n")
}
