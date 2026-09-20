//! t-renameat2 - one test for renameat2, the call musl's `rename` and
//! `mv` land in.
//!
//! Pinned here: a plain rename (flags 0) moves the content to the new name
//! and the old name answers ENOENT afterwards, RENAME_NOREPLACE refuses
//! with EEXIST when the target name is taken and leaves the source alone,
//! a plain rename over an existing target replaces it, a flags word with
//! unknown bits answers EINVAL, and a missing source answers ENOENT.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static A: [u8; 19] = *b"/tmp/t-renameat2_a\0";
static B: [u8; 19] = *b"/tmp/t-renameat2_b\0";
static GONE: [u8; 22] = *b"/tmp/t-renameat2_gone\0";
static mut BUF: [u8; 16] = [0; 16];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-renameat2\n");
    let mut failed = false;

    // Create a, write two bytes, close, then rename it onto b.
    let fd = lin::openat(lin::AT_FDCWD, &A, 0o100 | lin::O_RDWR, 0o644);
    lin::check_true(&mut failed, b"creating a opens it", fd >= 0);
    if fd >= 0 {
        let n = lin::write(fd as i32, b"R1");
        lin::check_eq(&mut failed, b"two bytes are written", n, 2);
        let _ = lin::close(fd as i32);
    }
    let r = lin::renameat2(lin::AT_FDCWD, &A, lin::AT_FDCWD, &B, 0);
    lin::check_eq(&mut failed, b"plain rename a to b succeeds", r, 0);

    // The content lives under b now; a is gone.
    let fd = lin::openat(lin::AT_FDCWD, &B, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the renamed file opens under b", fd >= 0);
    if fd >= 0 {
        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
        let n = lin::read(fd as i32, buf);
        lin::check_eq(&mut failed, b"b holds the two bytes written", n, 2);
        lin::check_true(
            &mut failed,
            b"the bytes are the ones a carried",
            buf[0] == b'R' && buf[1] == b'1',
        );
        let _ = lin::close(fd as i32);
    }
    let r = lin::openat(lin::AT_FDCWD, &A, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"reopening the old name answers ENOENT",
        r,
        lin::ENOENT,
    );

    // RENAME_NOREPLACE refuses to take a name that is already taken.
    let fd = lin::openat(lin::AT_FDCWD, &A, 0o100 | lin::O_RDWR, 0o644);
    lin::check_true(&mut failed, b"recreating a opens it", fd >= 0);
    if fd >= 0 {
        let _ = lin::close(fd as i32);
    }
    let r = lin::renameat2(
        lin::AT_FDCWD,
        &A,
        lin::AT_FDCWD,
        &B,
        lin::RENAME_NOREPLACE,
    );
    lin::check_eq(
        &mut failed,
        b"NOREPLACE over an existing target answers EEXIST",
        r,
        lin::EEXIST,
    );
    let fd = lin::openat(lin::AT_FDCWD, &A, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"NOREPLACE left the source in place", fd >= 0);
    if fd >= 0 {
        let _ = lin::close(fd as i32);
    }

    // Without NOREPLACE the rename replaces the target.
    let r = lin::renameat2(lin::AT_FDCWD, &A, lin::AT_FDCWD, &B, 0);
    lin::check_eq(
        &mut failed,
        b"a plain rename over an existing target succeeds",
        r,
        0,
    );

    // A flags word with unknown bits is refused outright.
    let r = lin::renameat2(lin::AT_FDCWD, &A, lin::AT_FDCWD, &B, 0x8);
    lin::check_eq(
        &mut failed,
        b"unknown flags answer EINVAL",
        r,
        lin::EINVAL,
    );

    // A source that is not there cannot be renamed.
    let r = lin::renameat2(lin::AT_FDCWD, &GONE, lin::AT_FDCWD, &B, 0);
    lin::check_eq(
        &mut failed,
        b"renaming a missing source answers ENOENT",
        r,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-renameat2: panicked\n")
}
