//! t-ns-cgroup - the group path a process reports, which a cgroup namespace
//! re-roots and a descriptor joins back out of.
//!
//! `CLONE_NEWCGROUP` is the last rung of §6.3's ladder, and the one kind with
//! **no objects**: its whole state is the root a task's group path renders
//! from. What was measured on a current host first (Linux 7.2.11, root, in
//! this program's own order):
//!
//! * `unshare(CLONE_NEWCGROUP)` needs `CAP_SYS_ADMIN`; an unprivileged caller
//!   gets `EPERM` - the first claim, and on a host run it is the *only* one
//!   (the program notes that and exits green, the shape `t-ns-ipc` has);
//! * the move re-roots the *rendering* and nothing else: a task that was in
//!   `/grp` reports `0::/` after the call, and a group it makes under the
//!   mount afterwards is still a real group of the machine's tree - the
//!   writes reach the real files, whose content is unchanged;
//! * a `fork` stays where its parent is: the child of a moved task reports
//!   the same re-rooted path, and `/proc/<pid>/cgroup` is asked **per task**,
//!   so a task left outside still reports the machine's full path;
//! * `/proc/<pid>/ns/cgroup` changes at the move - the link's target text is
//!   `cgroup:[<inode>]` and the two sides' numbers differ - and `setns` on a
//!   descriptor for the *other* side joins that view: the moved task opening
//!   the machine-side descriptor reads the full path again;
//! * an unshared task's view-root follows its group: moving the task into a
//!   deeper group after the move renders it as `/sub` from the new root.
//!
//! The divergence from Linux this kernel names rather than hides: inside the
//! namespace the **mount** still shows the machine's whole hierarchy (a
//! reader sees the groups above its root), where Linux re-roots the mount to
//! show only the namespace's subtree. The rendering and the identity are
//! Linux's; the mount view is the named remainder
//! (`docs/uapi/` §6's cgroup row).

use std::io::Error;
use std::os::fd::AsRawFd;

use mpos_user_tests::Report;
use rustix::fs::{Mode, mkdir};
use rustix::io::write;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use rustix::thread::{UnshareFlags, unshare_unsafe};

/// The mount this program makes for its own groups: the tree behind it is
/// the machine's one hierarchy, so a later mount binds the same one.
const AT: &str = "/tmp/t-ns-cgroup.mnt";

/// `/proc/<pid>/ns/cgroup`'s target text: `cgroup:[<number>]`.
fn ns_id(pid: u32) -> Option<u64> {
    let text = std::fs::read_link(format!("/proc/{pid}/ns/cgroup")).ok()?;
    let text = text.to_string_lossy();
    let inner = text.strip_prefix("cgroup:[")?.strip_suffix(']')?;
    inner.parse().ok()
}

/// The one line `/proc/<pid>/cgroup` answers, whole.
fn cgroup_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()
}

/// Write `text` to a cgroup attribute, answering whether it took.
fn put(path: &str, text: &str) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map(|mut file| write(&file, text.as_bytes()))
    {
        Ok(Ok(n)) => n == text.len(),
        _ => false,
    }
}

fn main() {
    let mut r = Report::new();
    let pid = std::process::id();

    // ---- the privilege probe, alone on a host run ----
    //
    // SAFETY: `unshare` touches no caller memory; it moves this process into
    // a fresh namespace, or refuses. Either answer is the probe's, and the
    // probes' own note is what the rest of the run branches on.
    let unprivileged = unsafe { unshare_unsafe(UnshareFlags::NEWCGROUP) };
    if unprivileged.is_err() {
        let e = Error::last_os_error();
        r.check(
            &format!("unshare(CLONE_NEWCGROUP) without CAP_SYS_ADMIN is EPERM (got {e})"),
            e.raw_os_error() == Some(1),
        );
        r.finish();
    }
    r.check("unshare(CLONE_NEWCGROUP) with the capability is 0", true);
    // This process is inside a cgroup namespace of root "/" now - the
    // machine's own view, since it had moved nowhere. Undo nothing: the
    // rendering below is relative to wherever this process is.

    // ---- the tree, mounted for the program's own groups ----
    mkdir(AT, Mode::from_bits(0o755).unwrap()).ok();
    match mount("none", AT, "cgroup2", MountFlags::empty(), None) {
        Ok(()) => r.check("mount -t cgroup2 succeeds", true),
        Err(e) => {
            r.check(&format!("mount -t cgroup2 succeeds ({e:?})"), false);
            r.finish();
        }
    }

    // Make the group this test owns, and move this process into it. From
    // the machine's view the path is /t-ns-cgroup.
    mkdir(format!("{AT}/grp"), Mode::from_bits(0o755).unwrap()).expect("mkdir grp");
    r.check(
        "this process moves into the group",
        put(&format!("{AT}/grp/cgroup.procs"), &format!("{pid}\n")),
    );
    r.check(
        "/proc/self/cgroup says 0::/grp",
        cgroup_of(pid).as_deref() == Some("0::/grp\n"),
    );

    // ---- the move: the rendering re-roots, the tree does not ----
    //
    // SAFETY: as the probe above.
    unsafe { unshare_unsafe(UnshareFlags::NEWCGROUP) }.expect("the move, with the capability");
    r.check(
        "after unshare(CLONE_NEWCGROUP), /proc/self/cgroup says 0::/",
        cgroup_of(pid).as_deref() == Some("0::/\n"),
    );
    // The view's own identity moved with it.
    let inside_id = ns_id(pid);
    r.check(
        "/proc/self/ns/cgroup names a namespace of its own",
        inside_id.is_some(),
    );

    // A group made *inside* the namespace is a real group of the machine's
    // tree - the writes reach the real files, whose content is unchanged.
    mkdir(format!("{AT}/grp/sub"), Mode::from_bits(0o755).unwrap()).expect("mkdir grp/sub");
    r.check(
        "mkdir inside the namespace makes a real group",
        std::path::Path::new(&format!("{AT}/grp/sub/cgroup.procs")).exists(),
    );
    r.check(
        "and it renders as /sub from the view's root",
        put(&format!("{AT}/grp/sub/cgroup.procs"), &format!("{pid}\n"))
            && cgroup_of(pid).as_deref() == Some("0::/sub\n"),
    );

    // ---- the child stays where its parent is, and /proc asks per task ----
    match unsafe { libc::fork() } {
        -1 => r.check("fork succeeds", false),
        0 => {
            // The child inherited the view and the group; it says so from
            // its own side and leaves with the code the parent reads.
            let mine = cgroup_of(std::process::id());
            let ok = mine.as_deref() == Some("0::/sub\n");
            let code = if ok { 0 } else { 1 };
            unsafe {
                libc::syscall(libc::SYS_exit_group, code as libc::c_long);
                libc::_exit(code)
            }
        }
        child => {
            use rustix::process::{Pid, WaitOptions, waitpid};
            let status = waitpid(Pid::from_raw(child), WaitOptions::empty());
            let ok = match status {
                Ok(Some((_, st))) => rustix::process::WaitStatus::exit_status(st) == Some(0),
                _ => false,
            };
            r.check("a fork of the moved task reports the view's own path", ok);
        }
    }

    // ---- the descriptor: the other side's view, joined back ----
    //
    // The parent process that spawned this test is outside the namespace:
    // its view is the machine's, so its ns/cgroup names a different
    // namespace. Opening it and calling setns joins *that* view - the
    // rendering goes back to the full path, while the group this process
    // sits in is the same one throughout.
    let parent = {
        // The sweeper starts this program directly; pid 1 is the machine's
        // init and every task of the machine's own view is a valid donor.
        // pid 1 is the safest: it has moved nowhere.
        1u32
    };
    let outside_id = ns_id(parent);
    r.check(
        "the machine-side task's ns/cgroup is a different namespace",
        match (inside_id, outside_id) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        },
    );
    let joined = std::fs::OpenOptions::new()
        .read(true)
        .open(format!("/proc/{parent}/ns/cgroup"))
        .map(|file| {
            // SAFETY: `setns` on an open namespace descriptor; the fd is
            // valid for the call, and the flag word names the kind the
            // descriptor carries.
            unsafe { libc::syscall(libc::SYS_setns, file.as_raw_fd(), libc::CLONE_NEWCGROUP) }
        });
    match joined {
        Ok(0) => {
            r.check("setns to the machine-side descriptor is 0", true);
            r.check(
                "and the rendering is the machine's path again",
                cgroup_of(pid).as_deref() == Some("0::/grp/sub\n"),
            );
        }
        other => r.check(
            &format!("setns to the machine-side descriptor ({other:?})"),
            false,
        ),
    }

    // ---- leave the tree as it was found ----
    put(&format!("{AT}/cgroup.procs"), &format!("{pid}\n"));
    let _ = unmount(AT, UnmountFlags::empty());
    r.finish()
}
