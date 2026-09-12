//! t_newfstatat - one test for newfstatat, the path-based stat the strace
//! uses on /etc/ld.so.cache before opening it.
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! stat buffer, flags - regular files and directories reported through the
//! 144-byte struct stat, and ENOENT for a path that does not exist. The
//! kernel currently reads the stat buffer as a path length; this test
//! marks that mismatch.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static BIN: [u8; 5] = *b"/bin\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";
static mut ST: core::mem::MaybeUninit<lin::Stat> = core::mem::MaybeUninit::uninit();

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_newfstatat\n");
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

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_newfstatat: panicked\n")
}
