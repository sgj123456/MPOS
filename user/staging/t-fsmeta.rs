//! t-fsmeta - the file calls a libc makes without asking permission.
//!
//! None of these is glamorous, and every one of them is in the path of an
//! ordinary program: an installer sets a mode after it writes a file, a
//! fetcher truncates its temp file before it fills it, a script asks what
//! a path is before it opens it. What matters is not what each one *does*
//! here but that each one **answers** - and that the answer is the one the
//! man page specifies, because a program branches on the difference:
//!
//! * `fsync` on a file succeeds; on a descriptor with no storage behind it
//!   - a socket, a terminal - it is EINVAL, which is what Linux says for a
//!   file that cannot be synchronized;
//! * `chmod`/`chown` land on a real path or descriptor and succeed, and
//!   name ENOENT when there is nothing there to set;
//! * `mknod` is EPERM, the answer a caller without CAP_MKNOD gets;
//! * `linkat` is EPERM where the filesystem has no hard links, and ENOENT
//!   when the source does not exist - the caller's fallback is to copy;
//! * `truncate` only promises the name resolves; ENOENT is the branch a
//!   caller takes.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const ENOENT: isize = -2;
const EBADF: isize = -9;
const EPERM: isize = -1;
const EINVAL: isize = -22;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-fsmeta\n");
    let mut failed = false;

    // A file to operate on.
    let fd = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/fsmeta",
        lin::O_CREAT | lin::O_RDWR,
        0o644,
    );
    lin::check_true(&mut failed, b"the test file opens", fd >= 0);
    if fd < 0 {
        lin::done(true);
    }
    let fd = fd as i32;
    lin::write(fd, b"hello");

    // --- fsync -----------------------------------------------------------
    let r = lin::fsync(fd);
    lin::check_eq(&mut failed, b"fsync on a file succeeds", r, 0);
    let r = lin::fsync(fd + 100);
    lin::check_eq(&mut failed, b"fsync on a closed descriptor is EBADF", r, EBADF);
    // A socket has nothing to flush to storage: EINVAL is what Linux
    // answers for a descriptor bound to something that cannot be synced.
    let sock = lin::socket(lin::AF_INET as usize, lin::SOCK_STREAM, 0);
    if sock >= 0 {
        let r = lin::fsync(sock as i32);
        lin::check_eq(
            &mut failed,
            b"fsync on a socket is EINVAL",
            r,
            EINVAL,
        );
        let _ = lin::close(sock as i32);
    }

    // --- the mode and owner setters -------------------------------------
    let r = lin::chmod(b"/tmp/fsmeta", 0o755);
    lin::check_eq(&mut failed, b"chmod on a real path succeeds", r, 0);
    let r = lin::chmod(b"/tmp/fsmeta_nothing", 0o755);
    lin::check_eq(
        &mut failed,
        b"chmod on a path that is not there is ENOENT",
        r,
        ENOENT,
    );
    let r = lin::fchmod(fd, 0o600);
    lin::check_eq(&mut failed, b"fchmod on an open descriptor succeeds", r, 0);
    let r = lin::fchmod(fd + 100, 0o600);
    lin::check_eq(&mut failed, b"fchmod on a closed descriptor is EBADF", r, EBADF);

    // --- mknod, linkat ---------------------------------------------------
    let r = lin::mknod(b"/tmp/fsmeta_node", 0o600, 0);
    lin::check_eq(
        &mut failed,
        b"mknod is refused with EPERM, not silently made",
        r,
        EPERM,
    );
    // A hard link: the VFS has none, so the answer has to be the one that
    // sends a caller to its copy-the-file fallback.
    let r = lin::linkat(
        lin::AT_FDCWD,
        b"/tmp/fsmeta",
        lin::AT_FDCWD,
        b"/tmp/fsmeta_link",
        0,
    );
    lin::check_eq(
        &mut failed,
        b"linkat where the filesystem has no links is EPERM",
        r,
        EPERM,
    );
    let r = lin::linkat(
        lin::AT_FDCWD,
        b"/tmp/fsmeta_nothing",
        lin::AT_FDCWD,
        b"/tmp/fsmeta_link",
        0,
    );
    lin::check_eq(
        &mut failed,
        b"linkat of a source that is not there is ENOENT",
        r,
        ENOENT,
    );

    // --- truncate --------------------------------------------------------
    let r = lin::ftruncate(fd, 0);
    lin::check_eq(&mut failed, b"ftruncate on an open file succeeds", r, 0);
    let r = lin::ftruncate(fd + 100, 0);
    lin::check_eq(
        &mut failed,
        b"ftruncate on a closed descriptor is EBADF",
        r,
        EBADF,
    );
    let r = lin::truncate(b"/tmp/fsmeta", 0);
    lin::check_eq(&mut failed, b"truncate by path succeeds", r, 0);
    let r = lin::truncate(b"/tmp/fsmeta_nothing", 0);
    lin::check_eq(
        &mut failed,
        b"truncate of a path that is not there is ENOENT",
        r,
        ENOENT,
    );

    // --- flock, umask ----------------------------------------------------
    let r = lin::flock(fd, 1); // LOCK_SH
    lin::check_eq(&mut failed, b"flock on an open file succeeds", r, 0);
    let r = lin::flock(fd + 100, 1);
    lin::check_eq(&mut failed, b"flock on a closed descriptor is EBADF", r, EBADF);

    // umask returns the mask it replaced, so setting it twice in a row is
    // the test: the second answer is what the first one set.
    let first = lin::umask(0o022);
    lin::check_true(
        &mut failed,
        b"umask answers with the old mask",
        first >= 0,
    );
    let second = lin::umask(0o077);
    lin::check_eq(
        &mut failed,
        b"and the second call reports the mask the first one set",
        second,
        0o022,
    );
    let _ = lin::umask(0o022);

    // --- stat and lstat --------------------------------------------------
    // `struct stat` is 144 bytes on x86_64; the buffer is oversized so a
    // kernel that writes more than expected is still inside it.
    let mut buf = [0u8; 192];
    let st = buf.as_mut_ptr() as *mut lin::Stat;
    let r = lin::stat(b"/tmp/fsmeta", st);
    lin::check_eq(&mut failed, b"stat by path answers", r, 0);
    let mode = unsafe { (*st).st_mode };
    lin::check_true(
        &mut failed,
        b"and names a regular file",
        mode & 0xF000 == 0x8000,
    );
    let r = lin::lstat(b"/tmp/fsmeta", st);
    lin::check_eq(&mut failed, b"lstat by path answers", r, 0);
    let r = lin::stat(b"/tmp/fsmeta_nothing", st);
    lin::check_eq(
        &mut failed,
        b"stat of a path that is not there is ENOENT",
        r,
        ENOENT,
    );

    lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/fsmeta", 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-fsmeta: panicked\n")
}
