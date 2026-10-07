//! t-faccessat2 - the access check with the flags the original could not take.
//!
//! Pinned here, per faccessat2(2): the mode word is validated (a bit outside
//! F_OK/R_OK/W_OK/X_OK is EINVAL), the flag word too (anything beyond
//! AT_EACCESS and AT_SYMLINK_NOFOLLOW is EINVAL), X_OK answers from the mode
//! bits the filesystem keeps - a directory traverses, an executable answers 0,
//! a plain non-executable file answers EACCES even for root (Linux's
//! `CAP_DAC_OVERRIDE` grants execute only where some execute bit exists) - and
//! a name that is not there is still ENOENT, whatever was asked of it.
//!
//! # Why the whole program is `libc::syscall`
//!
//! This call is *only* `faccessat2`: its subject is the flag word, and every
//! claim below passes a raw one - `0`, an accepted flag, and a word with a bit
//! outside the two. `rustix` does not bind it (`t-faccessat`'s module comment
//! measures that: `rustix::fs::accessat` is `faccessat` and only reaches
//! `faccessat2` when a flag is passed), so the program is driven through
//! `libc::syscall` with the number from `libc::SYS_faccessat2` - the `libc`
//! rule's clause (a) (`user/tests/Cargo.toml`).
//!
//! The flag *numbers* still come from a maintained table, and have to:
//! `libc` 0.2.189 does not define `AT_EACCESS` for linux-musl at all (measured
//! - it is in the crate for `fuchsia`, the BSDs, Hurd and AIX, and absent from
//! every `linux_like` file), so `rustix::fs::AtFlags` supplies it. `libc` has
//! the mode bits (`R_OK`/`W_OK`/`X_OK`/`F_OK`) and `AT_FDCWD`, and those come
//! from there.
//!
//! # What this pins on each machine
//!
//! `faccessat2` was added after the two machines' syscall tables had diverged,
//! so unlike the older calls it carries the *same* number on both
//! (`nr_aarch64.rs` and `nr_x86_64.rs` both say `FACCESSAT2 = 439`), and both
//! reach the same `handle_faccessat2`. The per-claim output is therefore the
//! same on both machines.
//!
//! The plain file is one this test writes itself under `/tmp` - the no-std
//! version read `/etc/motd`, and this program's claim is about the mode the
//! filesystem stores, not about which file carries it. `/etc` and `/bin/sh` are
//! the two paths it does not create, and both machines' payloads carry them.

use mpos_user_tests::Report;
use rustix::fs::AtFlags;
use std::ffi::{CStr, CString};
use std::io;

/// The plain file, written by this test: a create with the ordinary
/// `0666 & !umask` mode, so it carries read and write bits somewhere and no
/// execute bit anywhere - which is what makes the X_OK claim about the mode
/// rather than about the caller.
const OWN: &str = "/tmp/t-faccessat2_own";
/// A directory both machines' test payloads carry.
const DIR: &str = "/etc";
/// The one path this test reads that it does not create: an executable both
/// machines' payloads carry.
const SH: &str = "/bin/sh";
const TMP: &str = "/tmp";
const NO_SUCH: &str = "/no/such/thing";

/// A bit outside `F_OK`/`R_OK`/`W_OK`/`X_OK`. Written down because it is
/// deliberately outside every table - the claim is precisely that a mode bit
/// nothing names is refused.
const NOT_A_MODE_BIT: i32 = 8;
/// A bit outside `AT_EACCESS`/`AT_SYMLINK_NOFOLLOW`, for the same reason.
const NOT_A_FLAG_BIT: i32 = 0x8000;

/// `faccessat2(dirfd, path, mode, flags)` through `libc::syscall`. See the
/// module comment for why the raw call rather than a wrapper.
fn faccessat2(path: &CStr, mode: i32, flags: i32) -> Result<(), rustix::io::Errno> {
    // `syscall` is a libc entry point, so libc's own errno protocol is what
    // applies - read immediately, before anything else can set it.
    let r = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            mode,
            flags,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(rustix::io::Errno::from_raw_os_error(
            io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

fn main() {
    println!("t-faccessat2");
    let mut r = Report::new();

    let dir = CString::new(DIR).expect("the path has no interior NUL");
    let sh = CString::new(SH).expect("the path has no interior NUL");
    let tmp = CString::new(TMP).expect("the path has no interior NUL");
    let no_such = CString::new(NO_SUCH).expect("the path has no interior NUL");

    // The mode word: F_OK against a directory that is always there.
    r.check(
        "F_OK against /etc answers 0",
        faccessat2(&dir, libc::F_OK, 0) == Ok(()),
    );
    // Root's X_OK is "any execute bit": a directory has them, and the check
    // that fails is the one a shell's `test -x` is asking.
    r.check(
        "X_OK against a directory answers 0",
        faccessat2(&dir, libc::X_OK, 0) == Ok(()),
    );
    r.check(
        "X_OK against an executable answers 0",
        faccessat2(&sh, libc::X_OK, 0) == Ok(()),
    );

    // A plain file the kernel stores as a plain file: no execute bit anywhere
    // in the mode. Linux's `CAP_DAC_OVERRIDE` does not invent one, so root's
    // X_OK is refused - the same answer every spelling of the call gives, which
    // is what `t-faccessat` pins across the three.
    let _ = std::fs::remove_file(OWN);
    r.check(
        "the plain file is created",
        std::fs::write(OWN, b"mpos t-faccessat2 fixture\n").is_ok(),
    );
    let own = CString::new(OWN).expect("the path has no interior NUL");
    r.check(
        "F_OK against the plain file answers 0",
        faccessat2(&own, libc::F_OK, 0) == Ok(()),
    );
    r.check(
        "R_OK against the plain file answers 0",
        faccessat2(&own, libc::R_OK, 0) == Ok(()),
    );
    r.check(
        "X_OK against a plain file answers EACCES even for root",
        faccessat2(&own, libc::X_OK, 0) == Err(rustix::io::Errno::ACCESS),
    );

    // The validations, in the man page's order of refusal.
    r.check(
        "a mode bit outside the four answers EINVAL",
        faccessat2(&dir, NOT_A_MODE_BIT, 0) == Err(rustix::io::Errno::INVAL),
    );
    r.check(
        "a flag bit outside the two answers EINVAL",
        faccessat2(&dir, libc::F_OK, NOT_A_FLAG_BIT) == Err(rustix::io::Errno::INVAL),
    );
    r.check(
        "AT_EACCESS is accepted",
        faccessat2(&dir, libc::R_OK, AtFlags::EACCESS.bits() as i32) == Ok(()),
    );
    r.check(
        "AT_SYMLINK_NOFOLLOW is accepted",
        faccessat2(&tmp, libc::F_OK, AtFlags::SYMLINK_NOFOLLOW.bits() as i32) == Ok(()),
    );

    // A name that is not there: ENOENT, whatever was asked of it.
    r.check(
        "a missing name answers ENOENT",
        faccessat2(&no_such, libc::F_OK, 0) == Err(rustix::io::Errno::NOENT),
    );

    let _ = std::fs::remove_file(OWN);
    r.finish()
}
