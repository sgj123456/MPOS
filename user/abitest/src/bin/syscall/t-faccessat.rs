//! t-faccessat - root's access check is the same through every spelling.
//!
//! `apk` preserves a file's ownership by `fchmod`ing it to `0600` and then
//! asking, of the *same* file in the *same* process, whether it may write it -
//! once with `access(2)` and once with `faccessat(2)`. Those two disagreed:
//! `access` said yes and `faccessat` said `EACCES`, which is what surfaced as
//! the "failed to preserve <file>: permission" warnings that left every setuid
//! helper unpacked wrong. They route through one `may_as`, so for uid 0 they
//! must answer alike, and alike with `faccessat2` too - this pins that, on a
//! mode (`0600`, then `0000`) that an unprivileged check would refuse. The
//! alike-answers now include the refusals: Linux's `CAP_DAC_OVERRIDE` grants
//! root execute only where some execute bit exists, and read/write only where
//! some triad carries the bit, so X_OK on `0600` and everything on `0000` are
//! `EACCES` for all three spellings.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const R_OK: usize = 4;
const W_OK: usize = 2;
const X_OK: usize = 1;

const O_CREAT: usize = 0o100;
const O_RDWR: usize = 2;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-faccessat\n");
    let mut failed = false;

    let name = b"/tmp/t-faccessat.f\0";
    // The file apk ends up holding: root:root, mode 0600.
    let fd = lin::openat(lin::AT_FDCWD, name, O_CREAT | O_RDWR, 0o600) as i32;
    lin::check_true(&mut failed, b"the 0600 file is created", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }
    lin::check_eq(
        &mut failed,
        b"fchmod 0600 succeeds",
        lin::fchmod(fd, 0o600),
        0,
    );
    lin::close(fd);

    // Read and write: the triads allow it for root, and every spelling agrees.
    lin::check_eq(
        &mut failed,
        b"access(2) R_OK|W_OK is allowed for root",
        lin::access(name, R_OK | W_OK),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"faccessat(2) W_OK is allowed for root (was EACCES)",
        lin::faccessat(lin::AT_FDCWD, name, W_OK),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"faccessat2 W_OK is allowed for root",
        lin::faccessat2(lin::AT_FDCWD, name, W_OK, 0),
        0,
    );
    // Execute on a file with no execute bit for anyone: Linux's
    // `CAP_DAC_OVERRIDE` does not invent one - the override grants read/write
    // where some triad carries the bit and execute only where some execute bit
    // exists - so all three spellings agree on `EACCES` even for uid 0. The
    // agreement is the point of this program; which way it agrees moved with
    // the policy, and a caller that branches on `access(2)` now gets Linux's
    // branch.
    lin::check_eq(
        &mut failed,
        b"access(2) X_OK on 0600 is EACCES for root",
        lin::access(name, X_OK),
        lin::EACCES,
    );
    lin::check_eq(
        &mut failed,
        b"faccessat(2) X_OK on 0600 matches access(2)",
        lin::faccessat(lin::AT_FDCWD, name, X_OK),
        lin::EACCES,
    );

    // And a mode no one is granted at all: every kind refused, still agreed by
    // all three calls.
    lin::check_eq(
        &mut failed,
        b"chmod 0000 succeeds",
        lin::chmod(name, 0o000),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"access(2) R_OK|W_OK|X_OK on 0000 is EACCES for root",
        lin::access(name, R_OK | W_OK | X_OK),
        lin::EACCES,
    );
    lin::check_eq(
        &mut failed,
        b"faccessat(2) R_OK|W_OK|X_OK on 0000 is EACCES for root",
        lin::faccessat(lin::AT_FDCWD, name, R_OK | W_OK | X_OK),
        lin::EACCES,
    );

    lin::unlinkat(lin::AT_FDCWD, name, 0);
    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-faccessat: panicked\n")
}
