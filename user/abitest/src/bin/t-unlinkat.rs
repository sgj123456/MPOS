//! t-unlinkat - one test for unlinkat, the removal call musl's `rm` and
//! every temp-file cleanup path leans on.
//!
//! Pinned here: `unlinkat(AT_FDCWD, path, 0)` removes a file so reopening
//! the name answers ENOENT, a second unlink of the same name answers
//! ENOENT too, removing a file that is still open succeeds, removing a
//! name that is not there answers ENOENT, and AT_REMOVEDIR removes a
//! directory the same way.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// O_CREAT: make the named file when it is not there already.
const O_CREAT: usize = 0o100;

static FILE: [u8; 18] = *b"/tmp/t-unlinkat_f\0";
static DIR: [u8; 18] = *b"/tmp/t-unlinkat_d\0";
static GONE: [u8; 21] = *b"/tmp/t-unlinkat_gone\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-unlinkat\n");
    let mut failed = false;

    // Create, fill, close, then remove.
    let fd = lin::openat(lin::AT_FDCWD, &FILE, O_CREAT | lin::O_RDWR, 0o644);
    lin::check_true(&mut failed, b"creating the file opens it", fd >= 0);
    if fd >= 0 {
        let n = lin::write(fd as i32, b"abc");
        lin::check_eq(&mut failed, b"three bytes are written", n, 3);
        let _ = lin::close(fd as i32);
    }
    let r = lin::unlinkat(lin::AT_FDCWD, &FILE, 0);
    lin::check_eq(&mut failed, b"unlinkat removes the file", r, 0);
    let r = lin::openat(lin::AT_FDCWD, &FILE, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"reopening the removed name answers ENOENT",
        r,
        lin::ENOENT,
    );

    // Removing while the file is still open succeeds; the name is then gone.
    let fd = lin::openat(lin::AT_FDCWD, &FILE, O_CREAT | lin::O_RDWR, 0o644);
    lin::check_true(&mut failed, b"recreating the file opens it", fd >= 0);
    if fd >= 0 {
        let r = lin::unlinkat(lin::AT_FDCWD, &FILE, 0);
        lin::check_eq(
            &mut failed,
            b"unlinking a still-open file succeeds",
            r,
            0,
        );
        let r = lin::unlinkat(lin::AT_FDCWD, &FILE, 0);
        lin::check_eq(
            &mut failed,
            b"the second unlink of one name answers ENOENT",
            r,
            lin::ENOENT,
        );
        let _ = lin::close(fd as i32);
    }

    // A name that is not there cannot be unlinked.
    let r = lin::unlinkat(lin::AT_FDCWD, &GONE, 0);
    lin::check_eq(
        &mut failed,
        b"unlinking a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    // AT_REMOVEDIR removes a directory.
    let r = lin::mkdirat(lin::AT_FDCWD, &DIR, 0);
    lin::check_eq(&mut failed, b"mkdirat makes the directory", r, 0);
    let r = lin::unlinkat(lin::AT_FDCWD, &DIR, lin::AT_REMOVEDIR);
    lin::check_eq(
        &mut failed,
        b"unlinkat with AT_REMOVEDIR removes it",
        r,
        0,
    );
    let r = lin::openat(lin::AT_FDCWD, &DIR, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"reopening the removed directory answers ENOENT",
        r,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-unlinkat: panicked\n")
}
