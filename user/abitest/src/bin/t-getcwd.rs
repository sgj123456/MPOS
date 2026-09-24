//! t-getcwd - one test for getcwd, which musl calls on every `pwd` and
//! every relative path that needs its absolute form.
//!
//! Pinned here: the answer is the buffer address itself and the buffer
//! holds the cwd NUL-terminated ("/" at process start), chdir updates
//! what getcwd reports ("/etc", then a relative ".." back to "/"), a
//! buffer smaller than two bytes answers EINVAL, and a buffer too small
//! for the path plus its NUL answers ERANGE.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static ETC: [u8; 5] = *b"/etc\0";
static DOTDOT: [u8; 3] = *b"..\0";
static mut CWD: [u8; 256] = [0; 256];
static mut TINY: [u8; 1] = [0];
static mut TWO: [u8; 2] = [0; 2];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-getcwd\n");
    let mut failed = false;

    let cwd = unsafe { core::slice::from_raw_parts_mut(&raw mut CWD as *mut u8, 256) };
    let r = lin::getcwd(cwd);
    lin::check_true(&mut failed, b"getcwd returns the buffer address", r > 0);
    lin::check_true(
        &mut failed,
        b"the initial cwd reads /",
        cwd[0] == b'/' && cwd[1] == 0,
    );

    let r = lin::chdir(&ETC);
    lin::check_eq(&mut failed, b"chdir(/etc) succeeds", r, 0);
    let r = lin::getcwd(cwd);
    lin::check_true(&mut failed, b"getcwd returns the buffer again", r > 0);
    lin::check_true(
        &mut failed,
        b"the cwd now reads /etc",
        cwd[0] == b'/' && cwd[1] == b'e' && cwd[2] == b't' && cwd[3] == b'c' && cwd[4] == 0,
    );

    let r = lin::chdir(&DOTDOT);
    lin::check_eq(&mut failed, b"chdir(..) succeeds", r, 0);
    let r = lin::getcwd(cwd);
    lin::check_true(
        &mut failed,
        b"getcwd returns the buffer a third time",
        r > 0,
    );
    lin::check_true(
        &mut failed,
        b"the cwd reads / again",
        cwd[0] == b'/' && cwd[1] == 0,
    );

    // A buffer smaller than two bytes cannot even hold "/\0": EINVAL.
    let tiny = unsafe { core::slice::from_raw_parts_mut(&raw mut TINY as *mut u8, 1) };
    let r = lin::getcwd(tiny);
    lin::check_eq(
        &mut failed,
        b"a one-byte buffer answers EINVAL",
        r,
        lin::EINVAL,
    );

    // A buffer too small for the path plus its NUL answers ERANGE.
    let r = lin::chdir(&ETC);
    lin::check_eq(&mut failed, b"chdir(/etc) succeeds again", r, 0);
    let two = unsafe { core::slice::from_raw_parts_mut(&raw mut TWO as *mut u8, 2) };
    let r = lin::getcwd(two);
    lin::check_eq(
        &mut failed,
        b"a two-byte buffer for /etc answers ERANGE",
        r,
        lin::ERANGE,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-getcwd: panicked\n")
}
