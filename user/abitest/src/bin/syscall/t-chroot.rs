//! t-chroot - the root that belongs to a *task*, not to the machine.
//!
//! `chroot(2)` changes what `/` means to the caller and to the children it
//! forks afterwards - not to the whole machine. What makes that a fact worth a
//! test is the shape shared by every per-process view in this kernel (UTS's
//! `t-ns-uts` is the sibling): **the child's view differs and the parent's does
//! not.** A `chroot` that really moved the machine's root would pass the child's
//! half of every check and fail the parent's, and it is the parent's half that
//! keeps an unprivileged `chroot` from ending the world out from under everybody
//! else.
//!
//! The jail is checked three ways, because the interesting half of containment is
//! `..`: an absolute path must start at the new root, and a `..` that reaches it
//! must stop there rather than climb to the directory above. A file that exists
//! *inside* the jail is confirmed reachable, so the confinement is a live tree
//! the child stands in - not an empty refusal.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The directory the child makes its root. The marker it should see lives
/// underneath; the file it should *not* reach lives above, in `/tmp` beside it,
/// where the parent (unjailed) can still name it.
const NEW_ROOT: &[u8] = b"/tmp/cjr\0";
const INSIDE: &[u8] = b"/tmp/cjr/inside\0";
/// A file at the machine root's view of `/tmp`, one component above `NEW_ROOT`.
const ESCAPE: &[u8] = b"/tmp/cjr_escape\0";

/// What the child reports, one line per check, and only when it held. A missing
/// token (or a `!`-prefixed one) fails the whole comparison at the parent.
const EXPECTED: &[u8] = b"inside\nescape\ndotdot\n";

fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Create `path` as a one-byte file, answering the write count or the errno.
fn touch(path: &[u8]) -> isize {
    let fd = lin::openat(
        lin::AT_FDCWD,
        path,
        lin::O_CREAT | lin::O_WRONLY | lin::O_TRUNC,
        0o644,
    );
    if fd < 0 {
        return fd;
    }
    let n = lin::write(fd as i32, b"x");
    lin::close(fd as i32);
    n
}

/// Read a pipe to end-of-file.
fn drain(fd: i32, buf: &mut [u8]) -> usize {
    let mut total = 0;
    while total < buf.len() {
        let n = lin::read(fd, &mut buf[total..]);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    total
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-chroot\n");
    let mut failed = false;

    // ---- The tree: a jail with a marker in it, and one above it -------------
    // The mkdir may return EEXIST from an earlier line of this same run; the
    // two `touch`es are what the child's checks then depend on, and they are
    // asserted, so a tree that did not come up fails loudly rather than as a
    // mysterious confinement miss.
    let _ = lin::mkdirat(lin::AT_FDCWD, NEW_ROOT, 0o755);
    lin::check_true(
        &mut failed,
        b"the jail holds a file the child should see",
        touch(INSIDE) == 1,
    );
    lin::check_true(
        &mut failed,
        b"and the machine's /tmp holds a file above the future jail",
        touch(ESCAPE) == 1,
    );

    // ---- The child chroots; the parent's own view must not follow ----------
    let mut report = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"the report pipe is made",
        lin::pipe2(&mut report, 0),
        0,
    );
    let child = lin::fork();
    if child == 0 {
        lin::close(report[0]);
        let w = report[1];
        if lin::chroot(NEW_ROOT) != 0 {
            lin::write_all(w, b"!chroot\n");
            lin::close(w);
            lin::exit_group(1);
        }
        // chroot does not move the cwd; step into the new root as the caller
        // (and `switch_root`) must.
        let _ = lin::chdir(b"/\0");
        // Inside the jail: the marker is reachable at its new name.
        if lin::access(b"/inside\0", 0) == 0 {
            lin::write_all(w, b"inside\n");
        } else {
            lin::write_all(w, b"!inside\n");
        }
        // Above the jail is unreachable by name ...
        if lin::access(b"/cjr_escape\0", 0) == lin::ENOENT {
            lin::write_all(w, b"escape\n");
        } else {
            lin::write_all(w, b"!escape\n");
        }
        // ... and `..` at the root clamps, so it is unreachable by climbing too.
        if lin::access(b"/../cjr_escape\0", 0) == lin::ENOENT {
            lin::write_all(w, b"dotdot\n");
        } else {
            lin::write_all(w, b"!dotdot\n");
        }
        lin::close(w);
        lin::exit_group(0);
    }
    lin::close(report[1]);
    lin::check_true(&mut failed, b"the child that chroots was forked", child > 0);
    if child > 0 {
        let mut buf = [0u8; 64];
        let n = drain(report[0], &mut buf);
        lin::close(report[0]);
        let mut status = 0i32;
        lin::check_true(
            &mut failed,
            b"the child exited cleanly",
            lin::waitpid(child as usize, &mut status, 0) == child && status == 0,
        );
        lin::check_true(
            &mut failed,
            b"the jailed child sees its root's tree and not the machine's",
            eq_bytes(&buf[..n], EXPECTED),
        );

        // ---- The counterfactual: the machine's root never moved -------------
        // A global `chroot` would have deleted this file from the parent's view
        // too. The parent - which never called `chroot` - still names it above
        // the jail, so the jail was the child's alone.
        lin::check_eq(
            &mut failed,
            b"the parent, which never chrooted, still sees the file above the jail",
            lin::access(ESCAPE, 0),
            0,
        );
        // And the parent's `/inside` is still absent - the child's re-homing of
        // what `/` means did not give the parent a file it never had.
        lin::check_eq(
            &mut failed,
            b"and the parent's / is its own, not the child's jail",
            lin::access(b"/inside\0", 0),
            lin::ENOENT,
        );
    }

    // ---- Best-effort cleanup; the ephemeral test image would drop it anyway.
    let _ = lin::unlinkat(lin::AT_FDCWD, INSIDE, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, ESCAPE, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, NEW_ROOT, lin::AT_REMOVEDIR);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-chroot: panicked\n")
}
