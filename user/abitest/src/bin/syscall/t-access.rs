//! t-access - one test for access, which the strace shows the dynamic
//! loader using to probe /etc/ld.so.preload (answered -1 ENOENT).
//!
//! Pinned to mpos's root policy: uid 0 (this suite's caller) passes every
//! `access(2)` probe outright, so R_OK, W_OK, F_OK and even X_OK on a file with
//! no execute bit all answer 0. mpos drops Linux's one `CAP_DAC_OVERRIDE`
//! exception (which would refuse root's X_OK on a non-executable file), so a
//! `0644` file is reachable for execute too. A directory is reachable for R_OK,
//! and a missing path answers ENOENT, exactly the loader's expected outcome.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static BIN: [u8; 5] = *b"/bin\0";
static NO_SUCH: [u8; 9] = *b"/no/such\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-access\n");
    let mut failed = false;

    let r = lin::access(&MOTD, lin::R_OK);
    lin::check_eq(&mut failed, b"an existing file is readable", r, 0);

    // One user, no permission bits: every mode probes as reachable.
    let r = lin::access(&MOTD, lin::W_OK);
    lin::check_eq(&mut failed, b"an existing file answers W_OK", r, 0);

    // mpos drops Linux's one `CAP_DAC_OVERRIDE` exception: uid 0 passes every
    // check outright, so X_OK on a `0644` file with no execute bit for anyone is
    // granted to root too (real Linux would answer `EACCES` here). `/etc/motd`
    // is not runnable, yet as the sole root user of this image it is reachable.
    let r = lin::access(&MOTD, lin::X_OK);
    lin::check_eq(
        &mut failed,
        "an existing non-executable file answers X_OK for root".as_bytes(),
        r,
        0,
    );

    let r = lin::access(&MOTD, lin::F_OK);
    lin::check_eq(&mut failed, b"an existing file answers F_OK", r, 0);

    let r = lin::access(&BIN, lin::R_OK);
    lin::check_eq(&mut failed, b"a directory is readable", r, 0);

    let r = lin::access(&NO_SUCH, lin::R_OK);
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
    lin::die(b"t-access: panicked\n")
}
