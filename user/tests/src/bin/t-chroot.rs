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
//!
//! # The migration
//!
//! The no-std version forked, wrote its report down a pipe and compared the
//! bytes in the parent; all three of those stay exactly as they were -
//! `libc::fork`, `rustix::pipe::pipe`, `rustix::io::{write, read}`,
//! `rustix::process::waitpid` - because the claim *is* the call sequence, and
//! std's `Command` (vfork+exec) would change it. One thing cannot be inherited:
//! musl runs `exit_group`'s atexit list, and a forked child holding the
//! parent's std runtime would flush the parent's buffered stdout into the
//! serial log twice. So the child leaves through the raw door
//! (`libc::syscall(libc::SYS_exit, code)`: 60 here, 93 on the second machine,
//! from libc's maintained table), which is what the no-std program's
//! `lin::exit_group` was anyway. `chroot` itself is `rustix::process::chroot`;
//! the confinement probes are `rustix::fs::access`.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{Access, AtFlags, CWD, Mode, OFlags, access, mkdirat, openat, unlinkat};
use rustix::io::{Errno, read, write};
use rustix::pipe::pipe;
use rustix::process::{WaitOptions, chdir, chroot, waitpid};

/// The directory the child makes its root. The marker it should see lives
/// underneath; the file it should *not* reach lives above, in `/tmp` beside it,
/// where the parent (unjailed) can still name it.
const NEW_ROOT: &str = "/tmp/cjr";
const INSIDE: &str = "/tmp/cjr/inside";
/// A file at the machine root's view of `/tmp`, one component above `NEW_ROOT`.
const ESCAPE: &str = "/tmp/cjr_escape";

/// What the child reports, one line per check, and only when it held. A missing
/// token (or a `!`-prefixed one) fails the whole comparison at the parent.
const EXPECTED: &[u8] = b"inside\nescape\ndotdot\n";

fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Create `path` as a one-byte file, answering whether the write landed.
fn touch(path: &str) -> bool {
    let fd = openat(
        CWD,
        path,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    );
    match &fd {
        Ok(fd) => matches!(write(fd, b"x"), Ok(1)),
        Err(_) => false,
    }
}

/// `access(path, F_OK)` answered success.
fn access_ok(path: &str) -> bool {
    access(path, Access::EXISTS).is_ok()
}

/// `access(path, F_OK)` answered ENOENT - absent, not merely refused.
fn access_absent(path: &str) -> bool {
    access(path, Access::EXISTS) == Err(Errno::NOENT)
}

/// Leave the child without running any atexit handler: the raw `exit`, not
/// `exit_group`. See the header for why the std-side twin cannot inherit.
fn raw_exit(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit, code as libc::c_long);
    }
    // The door does not return; `libc::_exit` is the maintained fallback
    // spelling of the same word, kept for the case the raw number is gone.
    unsafe { libc::_exit(code) }
}

/// Write the whole line to the report pipe, ignoring a closed reader: the
/// parent's byte comparison is the check, not this write.
/// Write the whole line to the report pipe, ignoring a closed reader: the
/// parent's byte comparison is the check, not this write.
fn say(fd: &OwnedFd, s: &[u8]) {
    let _ = write(fd, s);
}

fn main() {
    println!("t-chroot");
    let mut r = Report::new();

    // ---- The tree: a jail with a marker in it, and one above it -------------
    // The mkdir may return EEXIST from an earlier line of this same run; the
    // two `touch`es are what the child's checks then depend on, and they are
    // asserted, so a tree that did not come up fails loudly rather than as a
    // mysterious confinement miss.
    let _ = mkdirat(CWD, NEW_ROOT, Mode::from_raw_mode(0o755));
    r.check("the jail holds a file the child should see", touch(INSIDE));
    r.check(
        "and the machine's /tmp holds a file above the future jail",
        touch(ESCAPE),
    );

    // ---- The child chroots; the parent's own view must not follow ----------
    //
    // A host without `CAP_SYS_CHROOT` cannot jail anybody (measured: EPERM,
    // errno 1, uid != 0), and the confinement claims below are that call's
    // answer - so on such a host the whole forked-child section is skipped by
    // design, and the run says so. The payload always runs as root.
    let pipes = pipe();
    r.check("the report pipe is made", pipes.is_ok());
    if !pipes.is_ok() {
        r.finish()
    }
    let (ro, wo) = pipes.unwrap();
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: this host user cannot chroot; the payload runs as root");
        r.check("the report pipe is made", true);
        r.finish()
    }
    // The raw door, not `rustix::process::fork`: the latter is typed for the
    // *parent's* return and gives no child half, and this claim is a fork in
    // which only the child calls chroot.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // Close the inherited read end for real - and the write end on the
        // way out. A leaked-but-open descriptor here is what hangs (or, with
        // the writer gone early, EOFs) the parent's drain: the pipe reports
        // end-of-file only once *every* writer of the machine has closed it.
        unsafe { libc::syscall(libc::SYS_close, ro.as_raw_fd() as libc::c_long) };
        if let Err(e) = chroot(NEW_ROOT) {
            let code = e.raw_os_error();
            say(&wo, format!("!chroot{code}\n").as_bytes());
            raw_exit(1);
        }
        // chroot does not move the cwd; step into the new root as the caller
        // (and `switch_root`) must.
        let _ = chdir("/");
        // Inside the jail: the marker is reachable at its new name.
        say(
            &wo,
            if access_ok("/inside") {
                b"inside\n"
            } else {
                b"!inside\n"
            },
        );
        // Above the jail is unreachable by name ...
        say(
            &wo,
            if access_absent("/cjr_escape") {
                b"escape\n"
            } else {
                b"!escape\n"
            },
        );
        // ... and `..` at the root clamps, so it is unreachable by climbing too.
        say(
            &wo,
            if access_absent("/../cjr_escape") {
                b"dotdot\n"
            } else {
                b"!dotdot\n"
            },
        );
        unsafe { libc::syscall(libc::SYS_close, wo.as_raw_fd() as libc::c_long) };
        raw_exit(0);
    }
    drop(wo);
    r.check("the child that chroots was forked", child > 0);
    if child > 0 {
        let mut buf = [0u8; 64];
        let mut total = 0;
        while total < buf.len() {
            match read(&ro, &mut buf[total..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
        drop(ro);
        let status = waitpid(rustix::process::Pid::from_raw(child), WaitOptions::empty());

        let clean = matches!(
            &status,
            Ok(Some((_, st))) if st.exited() && st.exit_status() == Some(0)
        );
        r.check("the child exited cleanly", clean);
        r.check(
            "the jailed child sees its root's tree and not the machine's",
            eq_bytes(&buf[..total], EXPECTED),
        );

        // ---- The counterfactual: the machine's root never moved -------------
        // A global `chroot` would have deleted this file from the parent's view
        // too. The parent - which never called `chroot` - still names it above
        // the jail, so the jail was the child's alone.
        r.check(
            "the parent, which never chrooted, still sees the file above the jail",
            access_ok(ESCAPE),
        );
        // And the parent's `/inside` is still absent - the child's re-homing of
        // what `/` means did not give the parent a file it never had.
        r.check(
            "and the parent's / is its own, not the child's jail",
            access_absent("/inside"),
        );
    }

    // ---- Best-effort cleanup; the ephemeral test image would drop it anyway.
    let _ = unlinkat(CWD, INSIDE, AtFlags::empty());
    let _ = unlinkat(CWD, ESCAPE, AtFlags::empty());
    let _ = unlinkat(CWD, NEW_ROOT, AtFlags::REMOVEDIR);

    r.finish()
}
