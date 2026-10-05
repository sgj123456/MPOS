//! t-access - one test for access, which the strace shows the dynamic
//! loader using to probe /etc/ld.so.preload (answered -1 ENOENT).
//!
//! Pinned to Linux's root policy: uid 0 (this suite's caller) is checked by
//! `CAP_DAC_OVERRIDE`, so R_OK and W_OK on `/etc/motd` answer 0 because some
//! triad carries those bits, while X_OK answers EACCES because no execute bit
//! exists anywhere in its mode. This kernel used to drop that exception - an
//! outright pass for uid 0 - and mpos now keeps it: a distribution's tree
//! ships real modes, and answering success where Linux answers EACCES sends a
//! caller that tests before it execs down a branch it was never meant to take.
//! A directory is reachable for R_OK, and a missing path answers ENOENT,
//! exactly the loader's expected outcome.

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

    // The override reads the triads: write exists somewhere in the mode, so
    // root may have it.
    let r = lin::access(&MOTD, lin::W_OK);
    lin::check_eq(&mut failed, b"an existing file answers W_OK", r, 0);

    // Linux's `CAP_DAC_OVERRIDE` sentence, kept: execute passes only when some
    // execute bit exists at all, and `/etc/motd` is plain data with none for
    // anyone. Real Linux answers EACCES here, and so does this kernel - the
    // old outright pass answered 0, which is the shape a caller cannot fall
    // back around.
    let r = lin::access(&MOTD, lin::X_OK);
    lin::check_eq(
        &mut failed,
        "an existing non-executable file answers X_OK with EACCES even for root".as_bytes(),
        r,
        lin::EACCES,
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
