//! t-faccessat - every spelling of the access check answers alike, for root
//! and for a caller that is not.
//!
//! `apk` preserves a file's ownership by `fchmod`ing it to `0600` and then
//! asking, of the *same* file in the *same* process, whether it may write it -
//! once with `access(2)` and once with `faccessat(2)`. Those two disagreed:
//! `access` said yes and `faccessat` said `EACCES`, which is what surfaced as
//! the "failed to preserve <file>: permission" warnings that left every setuid
//! helper unpacked wrong. They route through one `may_as`, so they must answer
//! alike, and alike with `faccessat2` too - that agreement is the point, and
//! this program pins it from **both** sides of the override.
//!
//! The side the override does not reach is the triads themselves, so the
//! refusals are asked as an ordinary user (the suite starts as root and drops
//! to uid 1000): on root's `0600` file a stranger gets `EACCES` for read and
//! write, and on a `0000` file it gets `EACCES` whatever it asks. That half
//! used to be unobservable here - the kernel passed uid 0 outright, so there
//! was no answer an unprivileged call could differ from.
//!
//! Then, back as root, Linux's `CAP_DAC_OVERRIDE` sentence: read/write pass
//! where *some* triad carries the bit, execute only where some execute bit
//! exists at all. X_OK on `0600` is therefore `EACCES` even for the superuser,
//! which is the fine print this kernel used to drop.

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

/// The id this program drops to to ask the questions the override does not
/// reach. Any non-zero uid answers the same way here - the file stays
/// root-owned, so the caller is `other` either way.
const OTHER: isize = 1000;

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

    // Read and write as root: the owner triad carries both bits, so the
    // override agrees with the plain answer, and every spelling says the same.
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
    // `CAP_DAC_OVERRIDE` does not invent one, so all three spellings agree on
    // `EACCES` even for uid 0. A caller that branches on `access(X_OK)` before
    // it execs now gets Linux's branch rather than a success that fails later.
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

    // And a mode no one is granted at all: refused outright, still agreed by
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
    lin::check_eq(
        &mut failed,
        b"faccessat2 R_OK|W_OK|X_OK on 0000 is EACCES for root",
        lin::faccessat2(lin::AT_FDCWD, name, R_OK | W_OK | X_OK, 0),
        lin::EACCES,
    );

    // The other side of the same file: a caller the override does not cover.
    // Dropping below also proves the refusal is about the ids and not the node
    // - the very calls that answered 0 above now answer EACCES.
    lin::check_eq(
        &mut failed,
        b"setuid to an ordinary user succeeds",
        lin::setuid(OTHER),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"access(2) R_OK on root's 0000 file is EACCES for a stranger",
        lin::access(name, R_OK),
        lin::EACCES,
    );
    lin::check_eq(
        &mut failed,
        b"faccessat(2) matches access(2) for a stranger",
        lin::faccessat(lin::AT_FDCWD, name, R_OK),
        lin::EACCES,
    );
    // `F_OK` is not a permission question at all: the name is there, so it
    // answers 0 whoever asks. Refusing it would make the check a lookup
    // failure rather than a refusal.
    lin::check_eq(
        &mut failed,
        b"F_OK for a stranger answers 0 - existence is not a permission",
        lin::access(name, 0),
        0,
    );

    lin::unlinkat(lin::AT_FDCWD, name, 0);
    lin::done(failed);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-faccessat: panicked\n")
}
