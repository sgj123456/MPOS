//! t-fchmodat2 - the flagged chmod, and the setuid bits an apk install sets.
//!
//! apk-tools 3.x asks for the mode of every setuid or set-group-id file it
//! unpacks through `fchmodat2(..., AT_SYMLINK_NOFOLLOW)` (`fs_fsys.c` calls
//! it only when `(mode & 07000) != 0`), which is why the warning
//! "failed to preserve <file>: permission" used to land on exactly those
//! files: the call did not exist here, and the old `fchmodat` read its flags
//! off a register no caller sets. Pinned here: the suid/sgid bits store and
//! read back; a non-link under `AT_SYMLINK_NOFOLLOW` chmods like plain
//! `fchmodat` does; a link answers `EOPNOTSUPP` (it has no mode of its own);
//! `AT_EMPTY_PATH` with an empty name is `fchmod` by descriptor; an unknown
//! flag is `EINVAL`; and the plain three-register `fchmodat` works no matter
//! what sits in the register a flags word would have come from.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const O_CREAT: usize = 0o100;
const O_RDWR: usize = 2;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fchmodat2\n");
    let mut failed = false;

    let name = b"/tmp/t-fchmodat2.f\0";
    let link = b"/tmp/t-fchmodat2.l\0";
    let fd = lin::openat(lin::AT_FDCWD, name, O_CREAT | O_RDWR, 0o600) as i32;
    lin::check_true(&mut failed, b"the test file is created", fd >= 0);
    if fd < 0 {
        lin::done(failed);
    }

    // The suid/sgid bits a package install asks for: stored, and read back
    // through fstat.
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        name.as_ptr() as usize,
        0o4755,
        0,
        0,
    );
    lin::check_eq(&mut failed, b"fchmodat2 sets 04755", r, 0);
    let mut st = lin::Stat {
        st_dev: 0,
        st_ino: 0,
        st_nlink: 0,
        st_mode: 0,
        st_uid: 0,
        st_gid: 0,
        _pad: 0,
        st_rdev: 0,
        st_size: 0,
        st_blksize: 0,
        st_blocks: 0,
        st_atime: 0,
        st_atime_nsec: 0,
        st_mtime: 0,
        st_mtime_nsec: 0,
        st_ctime: 0,
        st_ctime_nsec: 0,
        _unused: [0; 3],
    };
    lin::check_eq(&mut failed, b"fstat succeeds", lin::fstat(fd, &mut st), 0);
    lin::check_eq(
        &mut failed,
        b"the stored mode carries suid and sgid",
        (st.st_mode & 0o7777) as isize,
        0o4755,
    );

    // `AT_SYMLINK_NOFOLLOW` on a file that is not a link: plain chmod.
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        name.as_ptr() as usize,
        0o4755,
        AT_SYMLINK_NOFOLLOW,
        0,
    );
    lin::check_eq(&mut failed, b"NOFOLLOW on a file is plain chmod", r, 0);

    // A link has no mode of its own: following is refused, nofollow refuses.
    let r = lin::raw_sys(
        lin::nr::SYMLINKAT,
        name.as_ptr() as usize,
        lin::AT_FDCWD as usize,
        link.as_ptr() as usize,
    );
    lin::check_eq(&mut failed, b"the symlink is created", r, 0);
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        link.as_ptr() as usize,
        0o600,
        AT_SYMLINK_NOFOLLOW,
        0,
    );
    lin::check_eq(
        &mut failed,
        b"chmod on a link is EOPNOTSUPP",
        r,
        lin::EOPNOTSUPP,
    );

    // Without the flag the link is followed, and the *target* is chmod'ed.
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        link.as_ptr() as usize,
        0o600,
        0,
        0,
    );
    lin::check_eq(&mut failed, b"fchmodat2 through a link follows it", r, 0);
    lin::check_eq(&mut failed, b"fstat succeeds", lin::fstat(fd, &mut st), 0);
    lin::check_eq(
        &mut failed,
        b"the follow reached the file's mode",
        (st.st_mode & 0o7777) as isize,
        0o600,
    );

    // An unknown flag is refused the way Linux refuses it.
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        name.as_ptr() as usize,
        0o644,
        0x200,
        0,
    );
    lin::check_eq(&mut failed, b"an unknown flag is EINVAL", r, lin::EINVAL);

    // `AT_EMPTY_PATH` with an empty name is `fchmod` by descriptor. The name
    // is a pointer to an empty string - a null pointer is a fault, not an
    // empty name, here and on Linux.
    let empty = b"\0";
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        fd as usize,
        empty.as_ptr() as usize,
        0o640,
        AT_EMPTY_PATH,
        0,
    );
    lin::check_eq(&mut failed, b"AT_EMPTY_PATH chmods by descriptor", r, 0);
    lin::check_eq(&mut failed, b"fstat succeeds", lin::fstat(fd, &mut st), 0);
    lin::check_eq(
        &mut failed,
        b"the descriptor's mode is what was asked",
        (st.st_mode & 0o7777) as isize,
        0o640,
    );

    // An empty name *without* the flag names nothing.
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT2,
        lin::AT_FDCWD as usize,
        empty.as_ptr() as usize,
        0o644,
        0,
        0,
    );
    lin::check_eq(&mut failed, b"an empty path is ENOENT", r, lin::ENOENT);

    // The old spelling: three registers, and the fourth ignored - the flag
    // word no libc sends through `fchmodat`, and garbage there cannot fail
    // the call (this is the register the old kernel read for its flags).
    let r = lin::raw_sys5(
        lin::nr::FCHMODAT,
        lin::AT_FDCWD as usize,
        name.as_ptr() as usize,
        0o644,
        AT_SYMLINK_NOFOLLOW,
        0,
    );
    lin::check_eq(&mut failed, b"plain fchmodat succeeds", r, 0);
    lin::check_eq(&mut failed, b"fstat succeeds", lin::fstat(fd, &mut st), 0);
    lin::check_eq(
        &mut failed,
        b"plain fchmodat stored the mode",
        (st.st_mode & 0o7777) as isize,
        0o644,
    );

    let _ = lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, name, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, link, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fchmodat2: panicked\n")
}
