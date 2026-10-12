//! t-caller-paths — a path-taking syscall resolves in the caller's *own* tree:
//! the mount namespace it is in, and the root it is jailed to.
//!
//! `src/vfs/mount.rs` states the rule in its own header — *nothing outside this
//! module resolves against `global()` from syscall context* — and a dozen
//! handlers in `src/syscall/linux/fs/file.rs` broke it, reaching for the
//! machine's table (and, at two sites, the machine's *root*). What made the bug
//! survive review is that the divergence is sharp rather than subtle: the
//! *following* walks were caller-routed already, so inside a namespace with a
//! filesystem mounted `stat("/mnt/f")` saw the mount and `lstat("/mnt/f")` did
//! not. One path, two trees — and the wrong one for every `ls -l` in a
//! container, and for `mkdir` inside any jail whose root lacks a name the
//! machine's root has.
//!
//! Each claim below stands in front of a different handler, and each was checked
//! as a *counterfactual*: with the fix reverted, the claim fails. Three more
//! sites were fixed by the same rule and are deliberately **not** claimed here,
//! because their observable is masked and a claim that passes either way pins
//! nothing: `mkdir`/`mknod`'s existence probe inside a *namespace* still reports
//! `EEXIST` because the filesystem's own create refuses the second name, and
//! `linkat`'s cross-filesystem identity test still answers `EXDEV` because the
//! filesystem refuses that link by itself. The chroot half of the `mkdir` probe
//! is claimed, because there the machine's root really does answer for a name
//! the caller's jail does not have.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, StatxFlags, mkdirat, openat, statx, symlinkat};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

/// The directory the child mounts a tmpfs on. It exists in the root filesystem
/// *before* the mount, which is what `mount(2)` requires and what makes the
/// scenario a mount-over rather than a new directory.
const DIR: &str = "/tmp/t-caller-paths";
const FILE: &str = "/tmp/t-caller-paths/file";
const LINK: &str = "/tmp/t-caller-paths/link";
/// A directory the jail scenario chroots into.
const JAIL: &str = "/tmp/t-caller-paths-jail";
/// A name that exists at the machine's root and must not exist in the jail -
/// which is the whole of the `mkdir` claim: the existence probe has to ask the
/// caller's root, and `/tmp` is a name the machine's root has and a fresh jail
/// does not.
const ROOT_NAME: &str = "/tmp";

/// Fork, and in the child give it a mount namespace with a tmpfs at [`DIR`] and
/// a file and a symlink inside it, then run `body`.
fn in_ns_child(body: impl FnOnce() -> bool) -> bool {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let unshared =
            unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_NEWNS as libc::c_long) } == 0;
        // `DIR` lives in the *root* filesystem and survives the child (only the
        // tmpfs over it is the namespace's), so creating it is idempotent: the
        // first child's mkdir is not an error for the next one. The names
        // *inside* it are the tmpfs's and are fresh every time.
        let dir_ready = matches!(
            mkdirat(CWD, DIR, Mode::from_raw_mode(0o755)),
            Ok(()) | Err(Errno::EXIST)
        );
        let set_up = unshared
            && dir_ready
            && mount("tmpfs", DIR, "tmpfs", MountFlags::empty(), None).is_ok()
            && std::fs::write(FILE, b"inside\n").is_ok()
            && symlinkat("file", CWD, LINK).is_ok();
        // A scenario that could not be built says so as a *failure*, not as a
        // silent pass: a claim measured against an empty namespace would be
        // measuring the root filesystem twice.
        let answer = set_up && body();
        unsafe { libc::_exit(if answer { 0 } else { 1 }) };
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    reaped == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// Fork, and in the child chroot into an empty [`JAIL`], then run `body`. No
/// mount namespace: the claim is about the caller's *root* alone.
fn in_jail_child(body: impl FnOnce() -> bool) -> bool {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let jail = std::ffi::CString::new(JAIL).expect("a jail path with no NUL");
        let made = matches!(
            mkdirat(CWD, JAIL, Mode::from_raw_mode(0o755)),
            Ok(()) | Err(Errno::EXIST)
        );
        let jailed = made && unsafe { libc::chroot(jail.as_ptr()) } == 0;
        let answer = jailed && body();
        unsafe { libc::_exit(if answer { 0 } else { 1 }) };
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    reaped == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn main() {
    println!("t-caller-paths");
    let mut r = Report::new();

    // A stale jail from an earlier run would make the `mkdir` claim below read
    // its own leftovers, so the scenario starts from nothing.
    let _ = std::fs::remove_dir_all(JAIL);

    // `lstat` - the no-follow walk - must see the file the child put in its own
    // mount. This is the claim that names the bug: `stat` on the same path
    // worked all along, because it follows the last name through the
    // caller-routed resolver.
    r.check(
        "a mount namespace resolves lstat in its own tree (newfstatat/NOFOLLOW)",
        in_ns_child(|| {
            std::fs::symlink_metadata(FILE)
                .map(|m| m.is_file())
                .unwrap_or(false)
        }),
    );

    // `statx` with the same flag, the other handler on the no-follow walk.
    r.check(
        "statx with AT_SYMLINK_NOFOLLOW sees the symlink inside the mount",
        in_ns_child(|| {
            statx(CWD, LINK, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::empty())
                .map(|buf| buf.stx_mode & 0o170000 == 0o120000)
                .unwrap_or(false)
        }),
    );

    // `readlink` resolves the name it is given, so a link only the caller's
    // table can see has to be readable through it.
    r.check(
        "readlink sees a symlink inside the caller's own mount",
        in_ns_child(|| std::fs::read_link(LINK).is_ok()),
    );

    // `open(O_NOFOLLOW)` must refuse the very link `readlink` just read. The
    // refusal is a no-follow walk's, so a kernel that asked the machine's table
    // would follow the link instead of answering `ELOOP`.
    r.check(
        "open(O_NOFOLLOW) on that symlink is ELOOP, not a followed open",
        in_ns_child(|| {
            matches!(
                openat(CWD, LINK, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()),
                Err(Errno::LOOP)
            )
        }),
    );

    // `mkdir`'s existence probe must ask the *caller's root*. Inside the jail
    // there is no `/tmp`, so the call has to make one - and a probe that walked
    // from the machine's root instead would find the machine's `/tmp` and refuse
    // the caller a directory it does not have.
    r.check(
        "mkdir inside a jail probes the caller's root, not the machine's",
        in_jail_child(|| mkdirat(CWD, ROOT_NAME, Mode::from_raw_mode(0o755)).is_ok()),
    );

    let _ = std::fs::remove_dir_all(JAIL);
    let _ = std::fs::remove_dir_all(DIR);
    r.finish()
}
