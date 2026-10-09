//! t-access - one test for access, which the strace shows the dynamic
//! loader using to probe /etc/ld.so.preload (answered -1 ENOENT).
//!
//! Pinned to Linux's root policy: uid 0 (this suite's caller) is checked by
//! `CAP_DAC_OVERRIDE`, so R_OK and W_OK on the file this test owns answer 0
//! because some triad carries those bits, while X_OK answers EACCES because no
//! execute bit exists anywhere in its mode. This kernel used to drop that
//! exception - an outright pass for uid 0 - and mpos now keeps it: a
//! distribution's tree ships real modes, and answering success where Linux
//! answers EACCES sends a caller that tests before it execs down a branch it was
//! never meant to take. A directory is reachable for R_OK, and a missing path
//! answers ENOENT, exactly the loader's expected outcome.
//!
//! # The fixture, and what it means for the modes
//!
//! The file is one this test writes itself under `/tmp`, not the no-std version's
//! `/etc/motd`: the claim is about the policy `access` answers *with*, not about
//! which file's mode it reads, and the second machine's payload carries `/tmp`
//! and no motd at all (`NOTES.md` §15.11). `/bin` is a directory on both machines.
//!
//! Writing the file is what makes the mode part of the test rather than of the
//! image: the old file is removed first so the create always produces `0666 &
//! !umask`, which is the shape the claims need - read and write bits somewhere in
//! the triads, and no execute bit anywhere.
//!
//! # What this pins on each machine
//!
//! `access(2)` is one of the calls arm64 dropped (`nr_aarch64.rs`: `ACCESS =
//! 64004; // no arm64 number; unreachable on aarch64`), so musl implements it over
//! `faccessat(AT_FDCWD, …)`. Both routes reach the same `access_check` in the
//! kernel, with the same *real* ids - which is why the per-claim output is the
//! same on both machines, and why this is a migration rather than a duplication.

use mpos_user_tests::Report;
use rustix::fs::{Access, access};
use rustix::io::Errno;

/// The file this test owns: created as `0666 & !umask` after removing any
/// leftover, so it carries read and write bits for its owner and no execute bit
/// anywhere - which is what makes the X_OK claim below about the mode rather than
/// about the caller.
const FILE: &str = "/tmp/t-access_file";
/// A directory both machines' test payloads carry.
const DIR: &str = "/bin";
const NO_SUCH: &str = "/no/such";

fn main() {
    println!("t-access");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    if std::fs::write(FILE, b"mpos t-access fixture\n").is_err() {
        r.check("an existing file is readable", false);
        r.finish()
    }

    r.check(
        "an existing file is readable",
        access(FILE, Access::READ_OK) == Ok(()),
    );

    // The override reads the triads: write exists somewhere in the mode, so
    // root may have it.
    r.check(
        "an existing file answers W_OK",
        access(FILE, Access::WRITE_OK) == Ok(()),
    );

    // Linux's `CAP_DAC_OVERRIDE` sentence, kept: execute passes only when some
    // execute bit exists at all, and this file is plain data with none for
    // anyone. Real Linux answers EACCES here, and so does this kernel - the old
    // outright pass answered 0, which is the shape a caller cannot fall back
    // around.
    r.check(
        "an existing non-executable file answers X_OK with EACCES even for root",
        access(FILE, Access::EXEC_OK) == Err(Errno::ACCESS),
    );

    r.check(
        "an existing file answers F_OK",
        access(FILE, Access::EXISTS) == Ok(()),
    );

    r.check(
        "a directory is readable",
        access(DIR, Access::READ_OK) == Ok(()),
    );

    r.check(
        "a path that does not exist answers ENOENT",
        access(NO_SUCH, Access::READ_OK) == Err(Errno::NOENT),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
