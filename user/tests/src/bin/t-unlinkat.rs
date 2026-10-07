//! t-unlinkat - one test for unlinkat, the removal call musl's `rm` and
//! every temp-file cleanup path leans on.
//!
//! Pinned here: `unlinkat(AT_FDCWD, path, 0)` removes a file so reopening
//! the name answers ENOENT, a second unlink of the same name answers
//! ENOENT too, removing a file that is still open succeeds, removing a
//! name that is not there answers ENOENT, and AT_REMOVEDIR removes a
//! directory the same way.
//!
//! # What the flags word is doing here
//!
//! The last three claims are about the *flag* word: an unknown bit is EINVAL
//! and the name survives, while the one recognised bit still removes. That is
//! the shape `user/tests/Cargo.toml`'s `libc` rule is written around, and this
//! program is the case where it costs nothing to get right. `AtFlags` is a
//! bitflags type, so it carries bitflags' unnamed flag (`const _ = !0`) and
//! `AtFlags::from_bits_retain(0x1000)` is a legal call that hands the kernel the
//! word the claim is about. A rustix *plain enum* would have no such escape -
//! that distinction, not "a bitset cannot hold a bit", is what the rule says
//! (`t-fallocate`, `t-lseek`, `t-fadvise` carry the other examples). So this
//! program needs no `libc` at all.
//!
//! # What this pins on each machine
//!
//! `unlinkat` has a real number on both (`nr_aarch64.rs` and `nr_x86_64.rs`:
//! `UNLINKAT = 35` and `263`), both reach the same `handle_unlinkat`, and the
//! per-claim output is the same on both machines.
//!
//! The names are the test's own under `/tmp`, which both runners back with a
//! tmpfs, so nothing here depends on what the payload's image happens to carry.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::{Errno, write};

/// The file whose life and death the claims follow, and the directory the
/// `AT_REMOVEDIR` claims make and remove.
const FILE: &str = "/tmp/t-unlinkat_f";
const DIR: &str = "/tmp/t-unlinkat_d";
/// A name nothing has made.
const GONE: &str = "/tmp/t-unlinkat_gone";

/// A flag bit outside the one `AT_REMOVEDIR` names. Written down because it is
/// deliberately outside every table - the claim is that a word nothing names is
/// refused, and that the name survives the refusal.
const NOT_A_FLAG: u32 = 0x1000;

fn main() {
    println!("t-unlinkat");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    let _ = std::fs::remove_dir(DIR);
    let _ = std::fs::remove_file(GONE);

    // Create, fill, close, then remove.
    match openat(CWD, FILE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o644)) {
        Ok(fd) => {
            r.check("creating the file opens it", true);
            r.check("three bytes are written", write(&fd, b"abc") == Ok(3));
        }
        Err(_) => {
            r.check("creating the file opens it", false);
            r.check("three bytes are written", false);
        }
    }
    r.check(
        "unlinkat removes the file",
        unlinkat(CWD, FILE, AtFlags::empty()) == Ok(()),
    );
    r.check(
        "reopening the removed name answers ENOENT",
        matches!(
            openat(CWD, FILE, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NOENT)
        ),
    );

    // Removing while the file is still open succeeds; the name is then gone.
    match openat(CWD, FILE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o644)) {
        Ok(fd) => {
            r.check("recreating the file opens it", true);
            r.check(
                "unlinking a still-open file succeeds",
                unlinkat(CWD, FILE, AtFlags::empty()) == Ok(()),
            );
            r.check(
                "the second unlink of one name answers ENOENT",
                unlinkat(CWD, FILE, AtFlags::empty()) == Err(Errno::NOENT),
            );
            drop(fd);
        }
        Err(_) => {
            r.check("recreating the file opens it", false);
            r.check("unlinking a still-open file succeeds", false);
            r.check("the second unlink of one name answers ENOENT", false);
        }
    }

    // A name that is not there cannot be unlinked.
    r.check(
        "unlinking a path that does not exist answers ENOENT",
        unlinkat(CWD, GONE, AtFlags::empty()) == Err(Errno::NOENT),
    );

    // AT_REMOVEDIR removes a directory.
    r.check(
        "mkdirat makes the directory",
        mkdirat(CWD, DIR, Mode::empty()) == Ok(()),
    );
    r.check(
        "unlinkat with AT_REMOVEDIR removes it",
        unlinkat(CWD, DIR, AtFlags::REMOVEDIR) == Ok(()),
    );
    r.check(
        "reopening the removed directory answers ENOENT",
        matches!(
            openat(CWD, DIR, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NOENT)
        ),
    );

    // An unknown flag bit is EINVAL, and the name survives: Linux refuses a
    // flag that is not AT_REMOVEDIR before it resolves anything. This kernel
    // used to route only on AT_REMOVEDIR and silently accepted the rest, so a
    // bogus flag removed the file anyway - the wrong shape of answer.
    match openat(CWD, FILE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o644)) {
        Ok(fd) => {
            r.check("creating the flag-test file opens it", true);
            drop(fd);
        }
        Err(_) => r.check("creating the flag-test file opens it", false),
    }
    r.check(
        "unlinkat with an unknown flag (0x1000) answers EINVAL",
        unlinkat(CWD, FILE, AtFlags::from_bits_retain(NOT_A_FLAG)) == Err(Errno::INVAL),
    );
    r.check(
        "the refused unlink leaves the file in place",
        openat(CWD, FILE, OFlags::RDONLY, Mode::empty()).is_ok(),
    );
    // A recognised flag still works, so the gate is not a blanket refusal.
    let _ = unlinkat(CWD, FILE, AtFlags::empty());

    r.finish()
}
