//! t-newfstatat - one test for newfstatat, the path-based stat the strace
//! uses on /etc/ld.so.cache before opening it.
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! stat buffer, flags - regular files and directories reported through the
//! 144-byte struct stat, and ENOENT for a path that does not exist. The
//! AT_EMPTY_PATH form stats the descriptor itself and must agree with
//! fstat; without the flag an empty path is just an empty name (ENOENT);
//! paths resolve relative to a directory descriptor, and a bad dirfd
//! answers EBADF.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static BIN: [u8; 5] = *b"/bin\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static EMPTY: [u8; 1] = *b"\0";
static SH: [u8; 3] = *b"sh\0";
static mut ST: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-newfstatat\n");
    let mut failed = false;
    let st = &raw mut ST as *mut lin::Stat;

    let r = lin::newfstatat(lin::AT_FDCWD, &MOTD, st, 0);
    lin::check_eq(&mut failed, b"stat of /etc/motd succeeds", r, 0);
    unsafe {
        lin::check_true(
            &mut failed,
            b"it is a regular file with contents",
            (*st).st_mode & lin::S_IFMT == lin::S_IFREG && (*st).st_size > 0,
        );
    }

    let r = lin::newfstatat(lin::AT_FDCWD, &BIN, st, 0);
    lin::check_eq(&mut failed, b"stat of /bin succeeds", r, 0);
    unsafe {
        lin::check_true(
            &mut failed,
            b"it is a directory",
            (*st).st_mode & lin::S_IFMT == lin::S_IFDIR,
        );
    }

    let r = lin::newfstatat(lin::AT_FDCWD, &NO_SUCH, st, 0);
    lin::check_eq(
        &mut failed,
        b"a path that does not exist answers ENOENT",
        r,
        lin::ENOENT,
    );

    // AT_EMPTY_PATH: an empty path names the descriptor itself, and what it
    // reports must be what fstat reports for the same descriptor.
    let fd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"the file to stat by fd opens", fd >= 0);
    if fd >= 0 {
        let mut fst = core::mem::MaybeUninit::<lin::Stat>::uninit();
        let fp = &raw mut fst as *mut lin::Stat;
        let rf = lin::fstat(fd as i32, fp);
        lin::check_eq(&mut failed, b"fstat on the same fd succeeds", rf, 0);
        let r = lin::newfstatat(fd as i32, &EMPTY, st, lin::AT_EMPTY_PATH);
        lin::check_eq(
            &mut failed,
            b"an empty path with AT_EMPTY_PATH succeeds",
            r,
            0,
        );
        unsafe {
            lin::check_true(
                &mut failed,
                b"the AT_EMPTY_PATH size matches fstat's",
                (*st).st_size == (*fp).st_size,
            );
        }
        let r = lin::newfstatat(fd as i32, &EMPTY, st, 0);
        lin::check_eq(
            &mut failed,
            b"an empty path without AT_EMPTY_PATH answers ENOENT",
            r,
            lin::ENOENT,
        );
        let _ = lin::close(fd as i32);
    }

    // A path relative to a directory descriptor.
    let binfd = lin::openat(lin::AT_FDCWD, &BIN, lin::O_RDONLY, 0);
    lin::check_true(
        &mut failed,
        b"/bin opens for a dirfd-relative stat",
        binfd >= 0,
    );
    if binfd >= 0 {
        let r = lin::newfstatat(binfd as i32, &SH, st, 0);
        lin::check_eq(
            &mut failed,
            b"newfstatat relative to a dirfd succeeds",
            r,
            0,
        );
        unsafe {
            lin::check_true(
                &mut failed,
                b"the dirfd-relative stat says regular file",
                (*st).st_mode & lin::S_IFMT == lin::S_IFREG,
            );
        }
        let _ = lin::close(binfd as i32);
    }

    // A dirfd that is not open answers EBADF.
    let r = lin::newfstatat(77, &MOTD, st, 0);
    lin::check_eq(&mut failed, b"a bad dirfd answers EBADF", r, lin::EBADF);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-newfstatat: panicked\n")
}
