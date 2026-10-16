//! t-filelock - who a lock belongs to, which is the whole difference between
//! `fcntl` and `flock`.
//!
//! The two families are independent by design (a `flock` never blocks an
//! `fcntl` request and the reverse), but the interesting difference is not that
//! - it is the *owner*. A POSIX record lock belongs to the thread group, so one
//! process locking a range it already holds replaces its own lock instead of
//! deadlocking against itself, and closing **any** descriptor to the file drops
//! the process's record locks (the rule that surprises people). A `flock`
//! belongs to the open file description, so two independent `open()`s of one
//! file inside one process contend with each other, while a `dup`ed descriptor
//! shares the lock its original was given.
//!
//! A kernel that keys both by pid cannot tell those apart, and the difference is
//! visible to an ordinary program: `apk` takes a `flock` on its database, and a
//! second descriptor it opens for the same file must not be handed a lock it
//! already appears to hold - and must not have it dropped under it when an
//! unrelated descriptor closes.
//!
//! All the `flock`/POSIX claims were run against real Linux first, in the
//! shape this program makes them, because a pin's assertions are the first
//! thing that can be wrong: `0, EWOULDBLOCK, 0, 0, 0, 0, 0, 0, 0, 0, F_UNLCK,
//! 0`. The `F_OFD_*` group is the third ownership model - a record lock owned
//! by the description rather than the process - and its verdicts
//! (`0, 0, WOULDBLOCK, 0, (F_WRLCK, -1), WOULDBLOCK, 0, 0, 0`) were measured
//! the same way, with `.workbuddy/probe_ofd.c`.
//!
//! # The description's locks outlive a `dup`'s close
//!
//! A `flock` and an `F_OFD_*` lock are the **open file description's**, so
//! Linux releases them when the *last* descriptor referring to it closes -
//! measured on a current host, `flock(a, LOCK_EX)`, `close(dup_of_a)`, then a
//! fresh `open()` of the file still answers `EWOULDBLOCK`, and an
//! `F_OFD_SETLK` behaves the same (`.workbuddy/probe_ofd.c`, case D). This
//! kernel used to drop them on **any** close of the description, so a process
//! that dups its lock descriptor and closes the dup silently lost its lock -
//! and a second process could take it, which is exactly what `apk`'s database
//! lock exists to prevent. Two claims pin the Linux reading now: each class is
//! held through a description whose `dup` is then closed, and a *fresh*
//! `open()` must still find it in the way (`vfs/locks.rs`'s `drop_on_close` and
//! `drop_with_description` are the two halves of the rule, host-tested).
//!
//! # Which library each door comes from
//!
//! `flock` is `rustix::fs::flock`, and `F_SETLK` is `rustix::fs::fcntl_lock`:
//! both name the call and answer a `Result`, so the rule above keeps them. Every
//! operation this program asks for is a *named* variant of `FlockOperation`
//! (`LockExclusive`, `Unlock`, `NonBlockingLockShared`,
//! `NonBlockingLockExclusive`), so the fact that it is a plain enum with no
//! escape hatch for an out-of-vocabulary word costs nothing here - there is no
//! such claim to make. A program that *did* claim something about an operation
//! word `flock(2)` has no meaning for would be clause (c) and need `libc`;
//! this one is not.
//!
//! Three calls go to `libc`, and each is a different clause:
//!
//! * **`F_DUPFD` is clause (a)** - `rustix` does not bind it. It binds
//!   `fcntl_dupfd_cloexec` (`F_DUPFD_CLOEXEC`) and, for `espidf` alone, a
//!   `fcntl_dupfd`; on Linux there is no spelling of the plain command, which is
//!   the one this program's setup uses and the one a shell's redirection is
//!   built on.
//! * **`F_GETLK` is clause (a)** again - `rustix` binds `F_SETLK`/`F_SETLKW`
//!   through `fcntl_lock` and has no query at all, so the probe that reads back
//!   what is in the way is `libc::fcntl`.
//! * **`close` is clause (b)** - the wrapper binds the call and throws the
//!   result away: `rustix::io::close` takes a raw descriptor and returns
//!   nothing (`try_close` is behind a feature), so "close answers 0" has no
//!   expression through it. This is the same instance `t-dup2` records.
//!
//! The probe's `struct flock` is `libc::flock`, not the hand-rolled 32 bytes the
//! no-std version laid out: the layout is a per-target fact and belongs in a
//! maintained table. It is what `docs/uapi/` §4 pins, so the claim and the
//! record agree.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use rustix::fs::{AtFlags, CWD, FlockOperation, Mode, OFlags, fcntl_lock, flock, openat, unlinkat};
use rustix::io::Errno;

/// The file whose locks the claims follow. Under `/tmp`, which both runners
/// back with a tmpfs - so the name is of a kind the filesystem can hold.
const PATH: &str = "/tmp/t-filelock.bin";

/// The lowest number the `F_DUPFD` duplicate may take: 100, so the duplicate
/// cannot collide with a descriptor this program opened.
const DUP_FLOOR: i32 = 100;

/// The errno of the call just made, read immediately - before anything else can
/// set it.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `fcntl(fd, F_DUPFD, floor)` - the duplicate-at-or-above command, through the
/// named wrapper. See the module comment for why it is not `rustix`'s.
fn dupfd(fd: i32, floor: i32) -> Result<i32, Errno> {
    let r = unsafe { libc::fcntl(fd, libc::F_DUPFD, floor as libc::c_ulong) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(r)
    }
}

/// `F_GETLK` as the write lock over the whole file: what `l_type` the kernel
/// puts back - the kind of the lock in the way, or `F_UNLCK` when nothing is.
fn getlk(fd: i32) -> Result<libc::c_short, Errno> {
    let mut probe = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    let r = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut probe as *mut libc::flock) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(probe.l_type)
    }
}

/// `F_OFD_SETLK` (the whole file, `l_type` as asked): `0`, or the errno. The
/// `F_OFD_*` family has no `rustix` binding, so this is clause (a) again - the
/// same raw `libc::fcntl` the `F_GETLK` probe uses, with the command named.
fn ofd_setlk(fd: i32, l_type: libc::c_short) -> Result<(), Errno> {
    let mut want = libc::flock {
        l_type,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    let r = unsafe { libc::fcntl(fd, libc::F_OFD_SETLK, &mut want as *mut libc::flock) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(())
    }
}

/// `F_OFD_GETLK` as the same whole-file write probe, answering the pair the
/// claim is about: the conflicting kind, and the `l_pid` the kernel puts back.
fn ofd_getlk(fd: i32) -> Result<(libc::c_short, i32), Errno> {
    let mut probe = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    let r = unsafe { libc::fcntl(fd, libc::F_OFD_GETLK, &mut probe as *mut libc::flock) };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok((probe.l_type, probe.l_pid))
    }
}

fn main() {
    println!("t-filelock");
    let mut r = Report::new();

    let _ = std::fs::remove_file(PATH);

    let first = match openat(CWD, PATH, OFlags::RDWR | OFlags::CREATE, Mode::from(0o644)) {
        Ok(fd) => fd.into_raw_fd(),
        Err(e) => {
            r.check(&format!("the file is created (got {e})"), false);
            r.finish()
        }
    };
    r.check("the file is created", true);

    // A second, *independent* description of the same file - the case a
    // per-process lock table cannot see - and a `dup` of the first, which is
    // the *same* description.
    let second = match openat(CWD, PATH, OFlags::RDWR, Mode::empty()) {
        Ok(fd) => fd.into_raw_fd(),
        Err(_) => -1,
    };
    let dup = dupfd(first, DUP_FLOOR).unwrap_or(-1);
    r.check(
        "the second open() and the dup both exist",
        second >= 0 && dup >= 0,
    );
    if second < 0 || dup < 0 {
        let _ = std::fs::remove_file(PATH);
        r.finish()
    }

    // `first` and `second` stay owned so their descriptors cannot be closed by
    // accident; `dup` is the number the `F_DUPFD` answered, because a claim
    // below closes it by hand.
    // SAFETY: two descriptors this program just opened, neither closed since.
    let (first, second) = unsafe { (OwnedFd::from_raw_fd(first), OwnedFd::from_raw_fd(second)) };
    // SAFETY: `dup` is open from the `F_DUPFD` above until the `close` claim
    // below, which is the last use of this borrow.
    let dup = unsafe { BorrowedFd::borrow_raw(dup) };

    // --- flock: owned by the description ---
    r.check(
        "the first description takes LOCK_EX",
        flock(&first, FlockOperation::LockExclusive) == Ok(()),
    );
    r.check(
        "a second open() contends with it, same process and all",
        flock(&second, FlockOperation::NonBlockingLockExclusive) == Err(Errno::WOULDBLOCK),
    );
    r.check(
        "a dup shares the description's lock instead",
        flock(dup, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );
    r.check(
        "the holder releases",
        flock(&first, FlockOperation::Unlock) == Ok(()),
    );
    r.check(
        "and the contending description now takes it",
        flock(&second, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );

    // --- fcntl: owned by the process ---
    r.check(
        "a record lock is taken through the first descriptor",
        fcntl_lock(&first, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );
    r.check(
        "another descriptor of the same process replaces it rather than fighting it",
        fcntl_lock(&second, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );
    // With no flock held anywhere, a record lock alone must not refuse a
    // `flock`: the families never see each other.
    r.check(
        "the second description releases its flock",
        flock(&second, FlockOperation::Unlock) == Ok(()),
    );
    r.check(
        "a held record lock does not refuse a flock",
        flock(dup, FlockOperation::NonBlockingLockShared) == Ok(()),
    );
    // Closing a description drops the process's *record* locks wherever they
    // were taken (POSIX's rule) - but the description's *flock* is the
    // description's, and the description is still open, so it stays. The two
    // look like opposites and are the pair to check together.
    r.check(
        "closing that unrelated descriptor...",
        unsafe { libc::close(dup.as_raw_fd()) } == 0,
    );
    r.check(
        "...drops the record lock taken through a different fd",
        getlk(first.as_raw_fd()) == Ok(libc::F_UNLCK as libc::c_short),
    );
    // A *fresh* description, not the `dup`: the claim is about what a program
    // arriving afterwards finds, which is the whole point of a lock. Measured
    // on a current host - the description's shared `flock` is still in the way.
    // The probe's own descriptor is dropped immediately afterwards, so a kernel
    // that wrongly *handed* it the lock cannot leave it holding one and make
    // the next claim fail for this one's reason.
    let fresh = openat(CWD, PATH, OFlags::RDWR, Mode::empty());
    r.check(
        "...but the description's flock survives the dup's close: a fresh open() is refused",
        matches!(
            &fresh,
            Ok(fd) if flock(fd, FlockOperation::NonBlockingLockExclusive) == Err(Errno::WOULDBLOCK)
        ),
    );
    drop(fresh);
    r.check(
        "...and the first description converts its own shared lock to exclusive",
        flock(&first, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );

    // --- F_OFD_*: the third ownership model ---
    // A record lock in the same conflict domain as POSIX's, but owned by the
    // open file description - so two `open()`s contend while a `dup` shares,
    // exactly as `flock` behaves, and no process can be named as the holder.
    // Every verdict here was measured on the host first (`.workbuddy/probe_ofd.c`).
    r.check(
        "the flock holder releases before the OFD pair",
        flock(&first, FlockOperation::Unlock) == Ok(()),
    );
    let ofd_dup = match dupfd(first.as_raw_fd(), DUP_FLOOR) {
        Ok(fd) => fd,
        Err(_) => {
            let _ = unlinkat(CWD, PATH, AtFlags::empty());
            r.finish()
        }
    };
    // SAFETY: a descriptor just returned by `F_DUPFD`, closed by hand below.
    let ofd_dup = unsafe { BorrowedFd::borrow_raw(ofd_dup) };

    r.check(
        "an OFD write lock is taken through the first description",
        ofd_setlk(first.as_raw_fd(), libc::F_WRLCK as libc::c_short) == Ok(()),
    );
    r.check(
        "a second open() of the same process contends with it, description against process",
        ofd_setlk(second.as_raw_fd(), libc::F_WRLCK as libc::c_short) == Err(Errno::WOULDBLOCK),
    );
    r.check(
        "a dup shares the description's OFD lock",
        ofd_setlk(ofd_dup.as_raw_fd(), libc::F_WRLCK as libc::c_short) == Ok(()),
    );
    r.check(
        "F_OFD_GETLK names the holder as -1, since no process can name it",
        ofd_getlk(second.as_raw_fd()) == Ok((libc::F_WRLCK as libc::c_short, -1)),
    );
    r.check(
        "and a POSIX lock is refused by it, the same record domain",
        fcntl_lock(&second, FlockOperation::NonBlockingLockExclusive) == Err(Errno::WOULDBLOCK),
    );
    // The OFD lock is the description's too, so a `dup`'s close does not
    // release it - the same rule as the `flock` above, measured the same way
    // (`.workbuddy/probe_ofd.c`, case D).
    let ofd_survivor = match dupfd(first.as_raw_fd(), DUP_FLOOR) {
        Ok(fd) => fd,
        Err(_) => {
            let _ = unlinkat(CWD, PATH, AtFlags::empty());
            r.finish()
        }
    };
    // SAFETY: a descriptor just returned by `F_DUPFD`, closed by hand below.
    let ofd_survivor = unsafe { BorrowedFd::borrow_raw(ofd_survivor) };
    r.check(
        "a second dup of the description closes clean",
        unsafe { libc::close(ofd_survivor.as_raw_fd()) } == 0,
    );
    let fresh2 = openat(CWD, PATH, OFlags::RDWR, Mode::empty());
    r.check(
        "the description's OFD lock survives it: a fresh open() is refused",
        matches!(
            &fresh2,
            Ok(fd) if ofd_setlk(fd.as_raw_fd(), libc::F_WRLCK as libc::c_short)
                == Err(Errno::WOULDBLOCK)
        ),
    );
    // The same discipline as the `flock` probe above: drop the probe's
    // descriptor so a kernel that wrongly granted it the lock releases it here.
    drop(fresh2);
    r.check(
        "an unlock through the shared description releases it",
        ofd_setlk(ofd_dup.as_raw_fd(), libc::F_UNLCK as libc::c_short) == Ok(()),
    );
    r.check(
        "so the process can then take its POSIX record lock",
        fcntl_lock(&second, FlockOperation::NonBlockingLockExclusive) == Ok(()),
    );
    // SAFETY: the descriptor from the `F_DUPFD` above, open since.
    r.check(
        "the OFD dup closes clean",
        unsafe { libc::close(ofd_dup.as_raw_fd()) } == 0,
    );

    let _ = flock(&first, FlockOperation::Unlock);
    drop(first);
    drop(second);
    let _ = unlinkat(CWD, PATH, AtFlags::empty());
    r.finish()
}
