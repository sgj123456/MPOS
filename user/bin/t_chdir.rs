//! t_chdir - one test for chdir and fchdir, which the shell's `cd` and
//! every process that moves between directories leans on.
//!
//! Pinned here: chdir to an absolute and to a relative path succeeds and
//! getcwd reports where it landed, openat with AT_FDCWD resolves against
//! the new cwd, chdir to a file answers ENOTDIR and to a missing path
//! ENOENT, fchdir moves to the directory its descriptor names (and getcwd
//! follows), a regular-file descriptor answers ENOTDIR, and a bad fd
//! answers EBADF.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static ETC: [u8; 5] = *b"/etc\0";
static MOTD: [u8; 10] = *b"/etc/motd\0";
static MOTD_REL: [u8; 5] = *b"motd\0";
static BIN_REL: [u8; 4] = *b"bin\0";
static DOTDOT: [u8; 3] = *b"..\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static mut BUF: [u8; 256] = [0; 256];
static mut CWD: [u8; 256] = [0; 256];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_chdir\n");
    let mut failed = false;
    let cwd = unsafe { core::slice::from_raw_parts_mut(&raw mut CWD as *mut u8, 256) };

    let r = lin::chdir(&ETC);
    lin::check_eq(&mut failed, b"chdir(/etc) succeeds", r, 0);

    // AT_FDCWD opens resolve against the new cwd.
    let fd = lin::openat(lin::AT_FDCWD, &MOTD_REL, lin::O_RDONLY, 0);
    lin::check_true(
        &mut failed,
        b"openat resolves motd against the new cwd",
        fd >= 0,
    );
    if fd >= 0 {
        let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 256) };
        let n = lin::read(fd as i32, buf);
        lin::check_true(
            &mut failed,
            b"the relatively opened file reads",
            n > 0,
        );
        let _ = lin::close(fd as i32);
    }

    // A relative chdir from / lands in /bin.
    let r = lin::chdir(&DOTDOT);
    lin::check_eq(&mut failed, b"chdir(..) leaves /etc", r, 0);
    let r = lin::chdir(&BIN_REL);
    lin::check_eq(&mut failed, b"chdir(bin) relative succeeds", r, 0);
    let r = lin::getcwd(cwd);
    lin::check_true(&mut failed, b"getcwd returns the buffer", r > 0);
    lin::check_true(
        &mut failed,
        b"the cwd reads /bin",
        cwd[0] == b'/' && cwd[1] == b'b' && cwd[2] == b'i' && cwd[3] == b'n' && cwd[4] == 0,
    );
    let r = lin::chdir(&DOTDOT);
    lin::check_eq(&mut failed, b"chdir(..) leaves /bin", r, 0);

    // A file is not a directory, and a missing path is not anything.
    let r = lin::chdir(&MOTD);
    lin::check_eq(&mut failed, b"chdir to a file answers ENOTDIR", r, lin::ENOTDIR);
    let r = lin::chdir(&NO_SUCH);
    lin::check_eq(
        &mut failed,
        b"chdir to a missing path answers ENOENT",
        r,
        lin::ENOENT,
    );

    // fchdir moves to the directory the descriptor names.
    let dfd = lin::openat(lin::AT_FDCWD, &ETC, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"/etc opens as a directory fd", dfd >= 0);
    if dfd >= 0 {
        let r = lin::fchdir(dfd as i32);
        lin::check_eq(
            &mut failed,
            b"fchdir to the directory fd succeeds",
            r,
            0,
        );
        let r = lin::getcwd(cwd);
        lin::check_true(&mut failed, b"getcwd returns the buffer after fchdir", r > 0);
        lin::check_true(
            &mut failed,
            b"the cwd reads /etc after fchdir",
            cwd[0] == b'/' && cwd[1] == b'e' && cwd[2] == b't' && cwd[3] == b'c' && cwd[4] == 0,
        );
        let _ = lin::close(dfd as i32);
    }

    // A regular-file descriptor is not a directory; a bad fd is not a fd.
    let ffd = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_true(&mut failed, b"motd opens for the ENOTDIR check", ffd >= 0);
    if ffd >= 0 {
        let r = lin::fchdir(ffd as i32);
        lin::check_eq(
            &mut failed,
            b"fchdir on a file fd answers ENOTDIR",
            r,
            lin::ENOTDIR,
        );
        let _ = lin::close(ffd as i32);
    }
    let r = lin::fchdir(77);
    lin::check_eq(&mut failed, b"fchdir on a bad fd answers EBADF", r, lin::EBADF);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_chdir: panicked\n")
}
