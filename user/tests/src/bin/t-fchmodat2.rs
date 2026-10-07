//! t-fchmodat2 - the flagged chmod, and the setuid bits an apk install sets.
//!
//! apk-tools 3.x asks for the mode of every setuid or set-group-id file it
//! unpacks through `fchmodat2(..., AT_SYMLINK_NOFOLLOW)` (`fs_fsys.c` calls
//! it only when `(mode & 07000) != 0`), which is why the warning "failed to
//! preserve <file>: permission" used to land on exactly those files: the call
//! did not exist here, and the old `fchmodat` read its flags off a register
//! no caller sets. Pinned here: the suid/sgid bits store and read back; a
//! non-link under `AT_SYMLINK_NOFOLLOW` chmods like plain `fchmodat` does; a
//! link answers `EOPNOTSUPP` (it has no mode of its own); `AT_EMPTY_PATH`
//! with an empty name is `fchmod` by descriptor; an unknown flag is
//! `EINVAL`; and the plain three-register `fchmodat` works no matter what
//! sits in the register a flags word would have come from.
//!
//! # The whole program is the raw door
//!
//! `fchmodat2` is newer than every wrapper's vocabulary (clause (a): rustix
//! binds it nowhere, libc declares only the number - `SYS_fchmodat2`: 453 /
//! 452), and the *flag word* is the subject. The mode reads back through
//! `rustix::fs::fstat`, whose `Stat` is the machine's own - batch 12's
//! pattern replacing the no-std version's x86_64 `lin::Stat`. On real Linux
//! a symlink chmod under NOFOLLOW answers EPERM (measured here) where mpos
//! answers its documented EOPNOTSUPP; the payload sweep pins the refusal.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, fstat, openat, symlinkat, unlinkat};

const NAME: &str = "/tmp/t-fchmodat2.f";
const LINK: &str = "/tmp/t-fchmodat2.l";
const AT_EMPTY_PATH: u64 = 0x1000;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

/// The number is not in libc's table on every machine (measured: linux-musl
/// aarch64 declares no `SYS_fchmodat2` at all), so it comes from the one
/// table both machines agree with - this kernel's own uapi numbers, which
/// are Linux's (`nr_x86_64.rs` / `nr_aarch64.rs`: FCHMODAT2 = 452 on both).
const SYS_FCHMODAT2: libc::c_long = 452;

fn fchmodat2(dirfd: i32, path: &std::ffi::CStr, mode: u32, flags: u64) -> isize {
    unsafe { libc::syscall(SYS_FCHMODAT2, dirfd, path.as_ptr() as usize, mode, flags) as isize }
}

fn main() {
    println!("t-fchmodat2");
    let mut r = Report::new();

    let _ = unlinkat(CWD, NAME, AtFlags::empty());
    let _ = unlinkat(CWD, LINK, AtFlags::empty());
    let c_name = std::ffi::CString::new(NAME).unwrap();
    let c_link = std::ffi::CString::new(LINK).unwrap();
    let c_empty = std::ffi::CString::new("").unwrap();

    let opened = openat(
        CWD,
        NAME,
        OFlags::CREATE | OFlags::RDWR,
        Mode::from_bits_retain(0o600),
    );
    r.check("the test file is created", opened.is_ok());
    let fd = match opened {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };
    let raw = fd.as_raw_fd();

    // The suid/sgid bits a package install asks for: stored, and read back
    // through fstat.
    let at_fdcwd: i32 = -100;
    r.check(
        "fchmodat2 sets 04755",
        fchmodat2(at_fdcwd, &c_name, 0o4755, 0) == 0,
    );
    let st = fstat(&fd);
    r.check("fstat succeeds", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "the stored mode carries suid and sgid",
            rustix::fs::Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o4755,
        );
    } else {
        r.check("the stored mode carries suid and sgid", false);
    }

    // `AT_SYMLINK_NOFOLLOW` on a file that is not a link: plain chmod.
    r.check(
        "NOFOLLOW on a file is plain chmod",
        fchmodat2(at_fdcwd, &c_name, 0o4755, AT_SYMLINK_NOFOLLOW) == 0,
    );

    // A link has no mode of its own: following is refused, nofollow refuses.
    let made = symlinkat(NAME, CWD, LINK);
    r.check("the symlink is created", made.is_ok());
    let link_refused = fchmodat2(at_fdcwd, &c_link, 0o600, AT_SYMLINK_NOFOLLOW);
    r.check(
        "chmod on a link is EOPNOTSUPP",
        refused(link_refused, libc::EOPNOTSUPP)
            || (mpos_user_tests::host_run() && refused(link_refused, libc::EPERM)),
    );

    // Without the flag the link is followed, and the *target* is chmod'ed.
    r.check(
        "fchmodat2 through a link follows it",
        fchmodat2(at_fdcwd, &c_link, 0o600, 0) == 0,
    );
    let st = fstat(&fd);
    r.check("fstat succeeds", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "the follow reached the file's mode",
            rustix::fs::Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o600,
        );
    } else {
        r.check("the follow reached the file's mode", false);
    }

    // An unknown flag is refused the way Linux refuses it.
    r.check(
        "an unknown flag is EINVAL",
        refused(fchmodat2(at_fdcwd, &c_name, 0o644, 0x200), libc::EINVAL),
    );

    // `AT_EMPTY_PATH` with an empty name is `fchmod` by descriptor. The name
    // is a pointer to an empty string - a null pointer is a fault, not an
    // empty name, here and on Linux.
    r.check(
        "AT_EMPTY_PATH chmods by descriptor",
        fchmodat2(raw, &c_empty, 0o640, AT_EMPTY_PATH) == 0,
    );
    let st = fstat(&fd);
    r.check("fstat succeeds", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "the descriptor's mode is what was asked",
            rustix::fs::Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o640,
        );
    } else {
        r.check("the descriptor's mode is what was asked", false);
    }

    // An empty name *without* the flag names nothing.
    r.check(
        "an empty path is ENOENT",
        refused(fchmodat2(at_fdcwd, &c_empty, 0o644, 0), libc::ENOENT),
    );

    // The old spelling: three registers, and the fourth ignored - the flag
    // word no libc sends through `fchmodat`, and garbage there cannot fail
    // the call (this is the register the old kernel read for its flags).
    // Raw because the claim *is* the ignored fourth word.
    let plain = unsafe {
        libc::syscall(
            libc::SYS_fchmodat,
            at_fdcwd,
            c_name.as_ptr() as usize,
            0o644u32,
            AT_SYMLINK_NOFOLLOW as usize,
        )
    };
    r.check("plain fchmodat succeeds", plain == 0);
    let st = fstat(&fd);
    r.check("fstat succeeds", st.is_ok());
    if let Ok(st) = &st {
        r.check(
            "plain fchmodat stored the mode",
            rustix::fs::Mode::from_raw_mode(st.st_mode).bits() & 0o7777 == 0o644,
        );
    } else {
        r.check("plain fchmodat stored the mode", false);
    }

    let _ = made;
    drop(fd);
    let _ = unlinkat(CWD, NAME, AtFlags::empty());
    let _ = unlinkat(CWD, LINK, AtFlags::empty());
    r.finish()
}
