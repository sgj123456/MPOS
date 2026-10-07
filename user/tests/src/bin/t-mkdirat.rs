//! t-mkdirat - one test for mkdirat, the directory-creation call behind musl's
//! `mkdir`.
//!
//! Pinned here: creating a directory under `AT_FDCWD` succeeds, creating the
//! same name again answers `EEXIST`, the new directory opens and lists (empty,
//! so the first read of it ends immediately), removing it with
//! `unlinkat(AT_REMOVEDIR)` succeeds, and creating under a parent that does not
//! exist answers `ENOENT`.
//!
//! The listing goes through `std::fs::read_dir` rather than `getdents64`
//! directly: the claim is "the directory the kernel just made can be read", and
//! `read_dir` is the same getdents64 walk a real program makes. The scratch name
//! is under `/tmp`, which is what both machines' test payloads carry.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, mkdirat, unlinkat};

/// The directory this test owns.
const DIR: &str = "/tmp/t-mkdirat_d";
/// A parent that is not there. Named rather than created, so the call cannot be
/// answering about something else.
const MISSING: &str = "/no/such/dir";

/// One user, no permission bits to exercise: the mode is a formality here.
const MODE: Mode = Mode::from_bits_truncate(0o755);

fn main() {
    println!("t-mkdirat");
    let mut r = Report::new();

    r.check(
        "mkdirat creates the directory",
        mkdirat(CWD, DIR, MODE).is_ok(),
    );

    r.check(
        "creating the same name again answers EEXIST",
        mkdirat(CWD, DIR, MODE) == Err(rustix::io::Errno::EXIST),
    );

    // A directory opens for reading, and lists without error.
    match std::fs::read_dir(DIR) {
        Ok(mut entries) => r.check(
            "the new directory opens and lists empty",
            entries.next().is_none(),
        ),
        Err(_) => r.check("the new directory opens and lists empty", false),
    }

    // Cleanup: `AT_REMOVEDIR` takes the directory back.
    r.check(
        "unlinkat with AT_REMOVEDIR removes it",
        unlinkat(CWD, DIR, AtFlags::REMOVEDIR).is_ok(),
    );

    r.check(
        "mkdirat under a missing parent answers ENOENT",
        mkdirat(CWD, MISSING, MODE) == Err(rustix::io::Errno::NOENT),
    );

    r.finish()
}
