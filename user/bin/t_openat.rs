//! t_openat - one test for openat, the strace's file door: the dynamic
//! loader probes /etc/ld.so.cache with it and static binaries open their
//! files through it too (`openat(AT_FDCWD, path, O_RDONLY|O_CLOEXEC)`).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! flags, mode, with no length word; AT_FDCWD relative opens; the
//! lowest-free-fd guarantee (0-2 taken, so 3); close and its fd reuse; and
//! ENOENT for a path that does not exist. What the kernel currently reads
//! as a mpos-style (path, len) pair will fail this test - that mismatch is
//! the kernel work this test marks.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_openat\n");
    let mut failed = false;

    let fd = lin::openat(
        lin::AT_FDCWD,
        &MOTD,
        lin::O_RDONLY | lin::O_CLOEXEC,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"openat(AT_FDCWD) hands out the lowest free fd (3)",
        fd,
        3,
    );

    let r = lin::close(fd as i32);
    lin::check_eq(&mut failed, b"close of the open fd succeeds", r, 0);

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"the closed fd is reused for the next open",
        fd,
        3,
    );
    let _ = lin::close(fd as i32);

    let r = lin::openat(lin::AT_FDCWD, &NO_SUCH, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_openat: panicked\n")
}
