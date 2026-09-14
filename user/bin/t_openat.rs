//! t_openat - one test for openat, the strace's file door: the dynamic
//! loader probes /etc/ld.so.cache with it and static binaries open their
//! files through it too (`openat(AT_FDCWD, path, O_RDONLY|O_CLOEXEC)`).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! flags, mode, with no length word; AT_FDCWD relative opens; real-dirfd
//! opens (and EBADF / ENOTDIR for dirfds that are not directories); the
//! lowest-free-fd guarantee, measured rather than assumed to be 3; O_CREAT
//! making a missing
//! file and O_CREAT|O_EXCL answering EEXIST for one that is there; close
//! and its fd reuse; and ENOENT for a path that does not exist.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static BIN: [u8; 5] = *b"/bin\0";
static SH: [u8; 3] = *b"sh\0";
static X: [u8; 2] = *b"x\0";
static NEW: [u8; 18] = *b"/tmp/t_openat_new\0";

const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o400;

static mut BUF: [u8; 64] = [0; 64];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_openat\n");
    let mut failed = false;
    let base = lin::free_fd();

    let fd = lin::openat(
        lin::AT_FDCWD,
        &MOTD,
        lin::O_RDONLY | lin::O_CLOEXEC,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"openat(AT_FDCWD) hands out the lowest free fd",
        fd,
        base,
    );

    let r = lin::close(fd as i32);
    lin::check_eq(&mut failed, b"close of the open fd succeeds", r, 0);

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"the closed fd is reused for the next open",
        fd,
        base,
    );
    let _ = lin::close(fd as i32);

    let r = lin::openat(lin::AT_FDCWD, &NO_SUCH, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    // A real dirfd: /bin names a directory, and "sh" resolves inside it.
    let binfd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"open /bin gives a directory fd", binfd, base);
    let shfd = lin::openat(binfd as i32, &SH, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"openat(binfd, \"sh\") opens", shfd, base + 1);
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 64) };
    let n = lin::read(shfd as i32, buf);
    lin::check_true(&mut failed, b"the sh image reads back bytes", n > 0);
    lin::check_true(
        &mut failed,
        b"the sh image starts with ELF magic",
        n >= 4 && buf[0] == 0x7F && buf[1] == b'E' && buf[2] == b'L' && buf[3] == b'F',
    );
    let _ = lin::close(shfd as i32);
    let _ = lin::close(binfd as i32);

    let r = lin::openat(77, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(&mut failed, b"a dirfd that is not open answers EBADF", r, lin::EBADF);

    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    let r = lin::openat(fd as i32, &X, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"a dirfd that names a regular file answers ENOTDIR",
        r,
        lin::ENOTDIR,
    );
    let _ = lin::close(fd as i32);

    let fd = lin::openat(lin::AT_FDCWD, &NEW, O_CREAT | lin::O_RDWR, 0o644);
    lin::check_eq(&mut failed, b"O_CREAT makes the missing file", fd, base);
    let r = lin::openat(lin::AT_FDCWD, &NEW, O_CREAT | O_EXCL | lin::O_RDWR, 0o644);
    lin::check_eq(
        &mut failed,
        b"O_CREAT|O_EXCL on the made file answers EEXIST",
        r,
        lin::EEXIST,
    );
    let _ = lin::close(fd as i32);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_openat: panicked\n")
}
