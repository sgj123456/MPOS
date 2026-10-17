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
//!
//! And the *owner-shaped* family gets the same two-sided treatment, because it
//! is the same question asked of a different noun: `chmod`, `chown` and the
//! timestamps all turn on whether the caller owns the node, so a stranger's
//! rows are made here after the same drop. Two things those rows pin that a
//! single case could not: the timestamps call asks **two** questions - write
//! permission buys the "both clocks to now" form and not an explicit pair, so
//! one file is refused `EACCES` and the other `EPERM` - and a `chown` naming
//! the ids a node *already holds* is still a `chown` rather than a no-op a
//! stranger may make. Both were measured on a current host
//! (`.workbuddy/probe_capable_scope.sh own`, the owner-shaped table).
//!
//! # The two calls this program does not make through `rustix`
//!
//! `faccessat2` is driven through `libc::syscall` with `libc::SYS_faccessat2`.
//! `rustix` does not bind it: `rustix::fs::accessat` *is* `faccessat` (its
//! linux_raw backend issues `__NR_faccessat` when the flag word is empty) and
//! reaches `faccessat2` only when the caller passes a flag. This program's
//! whole subject is the three-way agreement, so the third spelling has to be
//! the third syscall rather than the first one wearing its name - the `libc`
//! rule's clause (a) (`user/tests/Cargo.toml`).
//!
//! The privilege drop is `rustix::thread::set_thread_uid`, whose own
//! documentation says it is the kernel's per-thread `setuid` and not the POSIX
//! one. That is the call this program wants: it is single-threaded, so the
//! thread's ids *are* the process's, and the wrapper is exactly `__NR_setuid` -
//! the call the no-std suite made through `lin::setuid`.
//!
//! # What this pins on each machine
//!
//! `access(2)` is one of the calls arm64 dropped (`nr_aarch64.rs`: `ACCESS =
//! 64004; // no arm64 number; unreachable on aarch64`), so musl implements it
//! over `faccessat(AT_FDCWD, ...)` - and `rustix::fs::access` routes it the same
//! way on that machine, measured in its linux_raw backend. Both routes reach the
//! same `access_check`, with the same *real* ids, which is why the per-claim
//! output is the same on both machines and why this is a migration rather than a
//! duplication.
//!
//! `chmod` is dropped on arm64 for the same reason, and `rustix::fs::chmod` is
//! written as `fchmodat(AT_FDCWD, ...)` on every machine - which is what makes
//! it answerable here at all. That is the handler the `0000` step below reaches
//! on both machines, so the mode both store is the one this test asked for.
//!
//! The file is one this test writes itself under `/tmp`, and every mode it is
//! asked about is one the calls under test set (`fchmod`, then `chmod`), so the
//! fixture's permissions are the test's rather than the image's.

use mpos_user_tests::Report;
use rustix::fs::{
    Access, AtFlags, CWD, Mode, OFlags, Timespec, Timestamps, access, accessat, chmod, chown,
    fchmod, openat, unlinkat, utimensat,
};
use rustix::io::Errno;
use rustix::process::{Gid, Uid};
use rustix::thread::set_thread_uid;
use std::ffi::{CStr, CString};
use std::io;

/// The file this test owns, and holds in each of the two modes the claims turn
/// on: `0600` while the override is being asked, `0000` afterwards.
const FILE: &str = "/tmp/t-faccessat.f";

/// The two files the *owner-shaped* claims below turn on, both root's: one
/// nobody but its owner may write, and one anybody may. Together they separate
/// the two questions Linux's timestamps call asks - write permission buys the
/// "both clocks to now" form and not the explicit pair - which is why the pair
/// has to be two files rather than one.
const PRIVATE: &str = "/tmp/t-faccessat.p";
const WRITABLE: &str = "/tmp/t-faccessat.w";

/// The file the stranger *does* own, written after the drop so its owner is
/// the id the claim is about.
const MINE: &str = "/tmp/t-faccessat.mine";

/// The id this program drops to for the questions the override does not reach.
/// Any non-zero uid answers the same way here - the file stays root-owned, so
/// the caller is `other` either way.
const OTHER: u32 = 1000;

/// `utimensat(2)` with a **null** `times` pointer: the "both clocks to now"
/// shape, which Linux spells `ATTR_TOUCH` and answers with write permission as
/// well as ownership.
///
/// `rustix::fs::utimensat` takes a `&Timestamps` and therefore cannot send a
/// null pointer at all, so the shape this program's claim turns on is one the
/// safe wrapper cannot express - the `libc` rule's clause (c)
/// (`user/tests/Cargo.toml`). The explicit pair beside it goes through `rustix`
/// as everything else here does.
fn utimensat_now(path: &CStr) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated C string, and a null `times` is
    // the shape being asked about rather than a pointer to dereference.
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(
            io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

/// A pair of times this test does not vary: the claims about it are about
/// whether the *pair* may be named at all, not about the values.
fn epoch() -> Timestamps {
    let t = Timespec {
        tv_sec: 1_600_000_000,
        tv_nsec: 0,
    };
    Timestamps {
        last_access: t,
        last_modification: t,
    }
}

/// `faccessat2(dirfd, path, mode, flags)`, with the dirfd and the flag word this
/// program does not vary: every call below is `AT_FDCWD` and no flags, and the
/// flagged vocabulary is `t-faccessat2`'s subject. See the module comment for
/// why the call is made through `libc::syscall`.
fn faccessat2(path: &CStr, mode: i32) -> Result<(), Errno> {
    // `syscall` is a libc entry point, so libc's own errno protocol is what
    // applies - read immediately, before anything else can set it.
    let r = unsafe { libc::syscall(libc::SYS_faccessat2, libc::AT_FDCWD, path.as_ptr(), mode, 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(
            io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

fn main() {
    println!("t-faccessat");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    let cpath = CString::new(FILE).expect("the path has no interior NUL");
    let Ok(fd) = openat(CWD, FILE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o600)) else {
        r.check("the 0600 file is created", false);
        r.finish()
    };
    r.check("the 0600 file is created", true);
    r.check(
        "fchmod 0600 succeeds",
        fchmod(&fd, Mode::from(0o600)).is_ok(),
    );
    drop(fd);

    // Read and write as root: the owner triad carries both bits, so the
    // override agrees with the plain answer, and every spelling says the same.
    r.check(
        "access(2) R_OK|W_OK is allowed for root",
        access(FILE, Access::READ_OK | Access::WRITE_OK) == Ok(()),
    );
    r.check(
        "faccessat(2) W_OK is allowed for root (was EACCES)",
        accessat(CWD, FILE, Access::WRITE_OK, AtFlags::empty()) == Ok(()),
    );
    r.check(
        "faccessat2 W_OK is allowed for root",
        faccessat2(&cpath, libc::W_OK).is_ok(),
    );
    // Execute on a file with no execute bit for anyone: Linux's
    // `CAP_DAC_OVERRIDE` does not invent one, so all three spellings agree on
    // `EACCES` even for uid 0. A caller that branches on `access(X_OK)` before
    // it execs now gets Linux's branch rather than a success that fails later.
    r.check(
        "access(2) X_OK on 0600 is EACCES for root",
        access(FILE, Access::EXEC_OK) == Err(Errno::ACCESS),
    );
    r.check(
        "faccessat(2) X_OK on 0600 matches access(2)",
        accessat(CWD, FILE, Access::EXEC_OK, AtFlags::empty()) == Err(Errno::ACCESS),
    );

    // A mode no one is granted at all. What root gets there is **two**
    // capabilities and not an exception: measured on a current host
    // (`.workbuddy/probe_capable_scope.sh dac`), `CAP_DAC_OVERRIDE` grants a
    // read and a write on a file whatever its bits are and
    // `CAP_DAC_READ_SEARCH` grants a read on its own - so `R_OK` alone and
    // `R_OK|W_OK` are `Ok` for root where the compound mask including `X_OK` is
    // refused, because execute needs an execute bit *somewhere*.
    r.check(
        "chmod 0000 succeeds",
        chmod(FILE, Mode::from(0o000)).is_ok(),
    );
    //
    // The two rows that need the capabilities *held* are root's, so a host run
    // - an ordinary user, which never has them - names them and does not make
    // them (`t-caps`'s rule for the same reason).
    if unsafe { libc::geteuid() } == 0 {
        r.check(
            "access(2) R_OK on 0000 is allowed for root (CAP_DAC_READ_SEARCH)",
            access(FILE, Access::READ_OK) == Ok(()),
        );
        r.check(
            "access(2) R_OK|W_OK on 0000 is allowed for root (CAP_DAC_OVERRIDE)",
            access(FILE, Access::READ_OK | Access::WRITE_OK) == Ok(()),
        );
    } else {
        println!(
            "note: not root, so the two rows where the 0000 file is *readable* are named and not made"
        );
    }
    let all = Access::READ_OK | Access::WRITE_OK | Access::EXEC_OK;
    // `EACCES` here is the *execute* clause's answer and not "0000 is refused
    // outright" - the three spellings still agree, which is the claim.
    r.check(
        "access(2) R_OK|W_OK|X_OK on 0000 is EACCES for root",
        access(FILE, all) == Err(Errno::ACCESS),
    );
    r.check(
        "faccessat(2) R_OK|W_OK|X_OK on 0000 is EACCES for root",
        accessat(CWD, FILE, all, AtFlags::empty()) == Err(Errno::ACCESS),
    );
    r.check(
        "faccessat2 R_OK|W_OK|X_OK on 0000 is EACCES for root",
        faccessat2(&cpath, libc::R_OK | libc::W_OK | libc::X_OK) == Err(Errno::ACCESS),
    );

    // The two fixtures the owner-shaped claims below need, made while this
    // process is still root: the drop that follows is one-way (`set_thread_uid`
    // moves the effective id and nothing here moves it back), so every owner
    // and mode those claims turn on has to be in place first. The modes are set
    // explicitly rather than left to `open`'s word, because the suite's umask is
    // not this test's.
    for path in [PRIVATE, WRITABLE] {
        let _ = std::fs::remove_file(path);
    }
    let mut made = true;
    for (path, mode) in [(PRIVATE, 0o600), (WRITABLE, 0o666)] {
        match openat(CWD, path, OFlags::CREATE | OFlags::RDWR, Mode::from(mode)) {
            Ok(fd) => made &= fchmod(&fd, Mode::from(mode)).is_ok(),
            Err(_) => made = false,
        }
    }
    r.check("root's 0600 and 0666 fixtures are made", made);

    // The other side of the same file: a caller the override does not cover.
    // Dropping below also proves the refusal is about the ids and not the node
    // - the very calls that answered 0 above now answer EACCES.
    r.check(
        "setuid to an ordinary user succeeds",
        set_thread_uid(Uid::from_raw(OTHER)).is_ok(),
    );
    r.check(
        "access(2) R_OK on root's 0000 file is EACCES for a stranger",
        access(FILE, Access::READ_OK) == Err(Errno::ACCESS),
    );
    r.check(
        "faccessat(2) matches access(2) for a stranger",
        accessat(CWD, FILE, Access::READ_OK, AtFlags::empty()) == Err(Errno::ACCESS),
    );
    // `F_OK` is not a permission question at all: the name is there, so it
    // answers 0 whoever asks. Refusing it would make the check a lookup
    // failure rather than a refusal.
    r.check(
        "F_OK for a stranger answers 0 - existence is not a permission",
        access(FILE, Access::EXISTS) == Ok(()),
    );

    // The owner-shaped family, on the same side of the drop: `chmod`, `chown`
    // and the timestamps all ask whether the caller owns the node, and the
    // answer is not always the shape the triads give. Measured on a current
    // host (`.workbuddy/probe_capable_scope.sh own`):
    //
    // ```text
    //                                       another's 0600  another's 0666
    // utimensat(NULL), both clocks to now        EACCES             0
    // utimensat(explicit times)                  EPERM           EPERM
    // chown/chgrp naming the ids it already has  EPERM
    // ```
    //
    // So the *refusal* itself differs between the two timestamps forms -
    // `EACCES` for the one write permission buys, `EPERM` for the one it does
    // not - and naming the ids a node already holds is a `chown` like any other
    // rather than a no-op a stranger may make.
    r.check(
        "chmod on root's 0600 file is EPERM for a stranger",
        chmod(PRIVATE, Mode::from(0o644)) == Err(Errno::PERM),
    );
    r.check(
        "chown naming the uid the file already has is EPERM for a stranger",
        chown(PRIVATE, Some(Uid::from_raw(0)), None) == Err(Errno::PERM),
    );
    r.check(
        "chgrp naming the gid the file already has is EPERM too",
        chown(PRIVATE, None, Some(Gid::from_raw(0))) == Err(Errno::PERM),
    );
    let cprivate = CString::new(PRIVATE).expect("the path has no interior NUL");
    let cwritable = CString::new(WRITABLE).expect("the path has no interior NUL");
    r.check(
        "utimensat(NULL) on root's 0600 file is EACCES for a stranger",
        utimensat_now(&cprivate) == Err(Errno::ACCESS),
    );
    r.check(
        "utimensat(NULL) on a world-writable file is served - write permission",
        utimensat_now(&cwritable) == Ok(()),
    );
    r.check(
        "utimensat(explicit times) on that same writable file is EPERM",
        utimensat(CWD, WRITABLE, &epoch(), AtFlags::empty()) == Err(Errno::PERM),
    );
    r.check(
        "and explicit times on the 0600 file are EPERM rather than EACCES",
        utimensat(CWD, PRIVATE, &epoch(), AtFlags::empty()) == Err(Errno::PERM),
    );
    // The control for the id arm, on a file this caller *does* own - written
    // after the drop, so it is uid 1000's own, and naming its own uid is the
    // one `chown` an owner may make without a capability.
    let _ = std::fs::remove_file(MINE);
    let mine_made = openat(CWD, MINE, OFlags::CREATE | OFlags::RDWR, Mode::from(0o600)).is_ok();
    r.check("a file of this stranger's own is made", mine_made);
    r.check(
        "chown naming the owner's own uid is served",
        chown(MINE, Some(Uid::from_raw(OTHER)), None) == Ok(()),
    );

    // Best effort, and expected to fail: the file is root's and the caller is
    // not, so the unlink is refused. The claim above is the last one that needs
    // the file, so the residue costs the run nothing - and both sweeps build
    // their `/tmp` from scratch (a tmpfs the runner mounts), so the next run
    // starts with an empty one either way. The stranger's own file *is*
    // removable, which is the same rule read from the other side.
    let _ = unlinkat(CWD, FILE, AtFlags::empty());
    let _ = unlinkat(CWD, PRIVATE, AtFlags::empty());
    let _ = unlinkat(CWD, WRITABLE, AtFlags::empty());
    let _ = unlinkat(CWD, MINE, AtFlags::empty());
    r.finish()
}
