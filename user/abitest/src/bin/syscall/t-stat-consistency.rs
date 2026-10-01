//! t-stat-consistency - `stat(path)` and `fstat(fd)` must name the same object.
//!
//! The shape pinned here is the one a lock-file protocol depends on: open a
//! path with `O_CREAT|O_TRUNC|O_RDWR`, then compare what `fstat` says about the
//! descriptor with what `stat` says about the path. A caller uses the pair to
//! close a race - it locks the file, then confirms the name still points at the
//! inode it locked, and retries if it does not. If the two answers disagree
//! about a file nobody has touched, that retry never converges: it is a tight
//! loop, not a wait.
//!
//! That is what happened here. `wayland-backend`'s pure-Rust server - the
//! backend `niri` uses, and the reason it links no `libwayland-server` -
//! acquires `<XDG_RUNTIME_DIR>/wayland-N.lock` exactly this way, and the loop
//! spun forever on an unchanged file, in state R with zero voluntary context
//! switches. The counterfactual is that loop: with the two answers agreeing
//! this program passes and the compositor gets past its socket setup.
//!
//! Two paths, chosen for what each says about the filesystem: `/tmp` is the
//! ramfs the boot mounts itself, and `/run/user/0` is where the lock file
//! actually lives (made by the image's own `/etc/local.d`). A path that cannot
//! be opened is reported rather than passed over in silence.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// `O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC`: the flags the callers use.
const CREATE_TRUNC: usize = 0o2 | 0o100 | 0o1000 | 0o2000000;

static TMP: [u8; 29] = *b"/tmp/t-stat-consistency.lock\0";
static RUN: [u8; 36] = *b"/run/user/0/t-stat-consistency.lock\0";

/// A zeroed 144-byte `struct stat`, so every check reads a known-clean buffer.
fn zero_stat() -> lin::Stat {
    lin::Stat {
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
    }
}

/// The numbers, so a red run carries its own evidence: a divergence is easier
/// to act on with both answers printed than with the verdict alone.
fn print_pair(tag: &[u8], st: &lin::Stat) {
    lin::puts(b"     ");
    lin::puts(tag);
    lin::puts(b": dev=");
    lin::puts_isize(st.st_dev as isize);
    lin::puts(b" ino=");
    lin::puts_isize(st.st_ino as isize);
    lin::puts(b" nlink=");
    lin::puts_isize(st.st_nlink as isize);
    lin::puts(b" size=");
    lin::puts_isize(st.st_size as isize);
    lin::puts(b"\n");
}

/// Make the lock file's own directory exist, without depending on the image's
/// userspace having made it: niri's lock lives in `$XDG_RUNTIME_DIR`, and the
/// point of the second path is to reach the same place it does. A refusal
/// because something is already there is the expected answer after the first
/// run, so neither result is checked - the open below is the check.
fn ensure_runtime_dir() {
    for path in [
        b"/run\0".as_slice(),
        b"/run/user\0".as_slice(),
        b"/run/user/0\0".as_slice(),
    ] {
        lin::mkdirat(lin::AT_FDCWD, path, 0o755);
    }
}

/// The pair against one path. Returns whether the path was reachable at all.
fn check(failed: &mut bool, label: &[u8], path: &[u8]) -> bool {
    lin::puts(b"  -- ");
    lin::puts(label);
    lin::puts(b" --\n");
    let fd = lin::openat(lin::AT_FDCWD, path, CREATE_TRUNC, 0o660);
    if fd < 0 {
        lin::puts(b"skip: the path could not be opened on this boot\n");
        return false;
    }
    let fd = fd as i32;

    let mut by_fd = zero_stat();
    let mut by_path = zero_stat();
    let r_fd = lin::fstat(fd, &raw mut by_fd);
    let r_path = lin::stat(path, &raw mut by_path);
    lin::check_eq(failed, b"fstat succeeds", r_fd, 0);
    lin::check_eq(failed, b"stat succeeds", r_path, 0);
    print_pair(b"fstat", &by_fd);
    print_pair(b"stat ", &by_path);

    lin::check_true(
        failed,
        b"fstat and stat name the same inode",
        by_fd.st_ino == by_path.st_ino,
    );
    lin::check_true(
        failed,
        b"fstat and stat name the same device",
        by_fd.st_dev == by_path.st_dev,
    );
    lin::check_true(
        failed,
        b"the link count agrees",
        by_fd.st_nlink == by_path.st_nlink,
    );
    lin::check_true(failed, b"the size agrees", by_fd.st_size == by_path.st_size);

    // And in the order the lock protocol uses: stat the *path* first, then open
    // (truncating), then fstat the descriptor. The protocol compares the read
    // taken before the open with the one taken after it, so an inode that
    // changed in between - or two that simply disagree - spins the retry.
    let mut before = zero_stat();
    let r = lin::stat(path, &raw mut before);
    lin::check_eq(failed, b"the pre-open stat succeeds", r, 0);
    let fd2 = lin::openat(lin::AT_FDCWD, path, CREATE_TRUNC, 0o660);
    lin::check_true(failed, b"the second open succeeds", fd2 >= 0);
    if fd2 >= 0 {
        let mut after = zero_stat();
        lin::fstat(fd2 as i32, &raw mut after);
        print_pair(b"pre-open stat ", &before);
        print_pair(b"post-open fstat", &after);
        lin::check_true(
            failed,
            b"the pre-open stat and the post-open fstat agree",
            before.st_ino == after.st_ino && before.st_dev == after.st_dev,
        );
        lin::close(fd2 as i32);
    }

    lin::close(fd);
    lin::unlinkat(lin::AT_FDCWD, path, 0);
    true
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-stat-consistency\n");
    let mut failed = false;

    let mut reachable = 0;
    if check(&mut failed, b"ramfs (/tmp)", &TMP) {
        reachable += 1;
    }
    ensure_runtime_dir();
    if check(&mut failed, b"the runtime directory (/run/user/0)", &RUN) {
        reachable += 1;
    }
    // A run that reached neither path proved nothing, so it must not pass.
    lin::check_true(
        &mut failed,
        b"at least one of the two paths was reachable",
        reachable > 0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-stat-consistency: panicked\n")
}
