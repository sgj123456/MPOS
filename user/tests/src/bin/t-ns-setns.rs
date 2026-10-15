//! t-ns-setns — the namespace a *descriptor* can name: `/proc/<pid>/ns` and the
//! `setns` that joins what its links hold.
//!
//! `t-unshare`-family's other half. `unshare(CLONE_NEWNS)` moves the caller to a
//! *copy* of its namespace, and `t-ns-mnt` pins what that means; `setns` is the
//! opposite call and exists so that a program can adopt a namespace another
//! process already made — the descriptor is the namespace, so joining it
//! *shares* rather than copies, and a mount made by one member of a namespace is
//! seen by every other member from then on. Two processes in one mount
//! namespace with a third outside it is the shape a container runtime is built
//! out of, and this file is the first place in the tree where the two halves
//! meet: the directory that hands out names, and the call that joins them.
//!
//! # What was measured before any of it was written
//!
//! `.workbuddy/probe_setns.c`, host 7.2.9 — every claim below that pins *Linux's*
//! answer comes from there rather than from a memory of the man page:
//!
//! * `/proc/self/ns` holds ten names on that host (`net`, `uts`, `ipc`, `pid`,
//!   `pid_for_children`, `user`, `mnt`, `cgroup`, `time`, `time_for_children`),
//!   every one a symlink, every target `kind:[<n>]`, and the numbers come from
//!   **one pool** (`net` 4026531833 down to `mnt` 4026531832, consecutive in the
//!   order the namespaces were made);
//! * `open` on a link gives a descriptor for the *namespace*, whose `read` is
//!   `EINVAL`, whose `write` is `EBADF` (it was opened read-only), and whose
//!   `lseek` is `ESPIPE` — and whose `fstat().st_ino` (148898) is *not* the
//!   number the link reports, so the text is what a reader uses as identity;
//! * `setns(fd, nstype)` answers `EINVAL` when the word is a *sibling* kind's
//!   bit, when it is **two** bits at once, and when it names a kind the
//!   descriptor does not (`0x10000000`, `CLONE_NEWUSER`) — all three `-22`, not
//!   `EPERM`;
//! * a descriptor that is not a namespace is `EINVAL` (measured with a pipe, a
//!   plain file and a directory), a number that names nothing is `EBADF`, and
//!   `/proc/<pid>/ns/mnt` for a pid that does not exist is `ENOENT`;
//! * the *capability* comes after all of that: the probe's own user was
//!   unprivileged, and its calls with the **matching** bit were refused `EPERM`
//!   while the mismatched ones were refused `EINVAL`. What the sandbox would not
//!   let it reach is a *successful* join — this host refuses `setns` into one's
//!   own namespace (`EPERM`) — so the success this kernel answers is this
//!   kernel's own shape, named here rather than pinned as Linux's.
//!
//! # What this kernel has, and what it therefore names
//!
//! Two of those ten exist here (the mount table and the names, §6.3's ladder),
//! so `/proc/<pid>/ns` lists **two** names and the rest are `ENOENT` — which is
//! not a gap being hidden but the surface a real kernel has for a namespace it
//! was built without: a machine with `CONFIG_TIME_NS` off has no
//! `/proc/self/ns/time` either, and no descriptor for one can exist to hand
//! `setns`. The `ENOENT` half is therefore a claim here, not an absence.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
use rustix::io::Errno;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use rustix::pipe::pipe;

const CLONE_NEWNS: usize = 0x0002_0000;
const CLONE_NEWUTS: usize = 0x0400_0000;
const CLONE_NEWIPC: usize = 0x2000_0000;
/// A path only the child's namespace has, so that seeing it is seeing *its*
/// namespace rather than a coincidence.
const INNER: &str = "/tmp/t-ns-setns-inner";
/// And a file inside that mount: its contents are the child's, so reading them
/// after the join is the difference between joining a namespace and joining an
/// empty copy of one.
const MARKER: &str = "/tmp/t-ns-setns-inner/inside";

/// `setns(fd, nstype)` through the raw door: the answer is the kernel's, and
/// the errno is read back beside it (musl's `-1` protocol, the shape the other
/// namespace tests carry).
fn setns(fd: i32, nstype: usize) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_setns, fd as libc::c_long, nstype as libc::c_long) };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

fn readlink(path: &str) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|target| target.to_string_lossy().into_owned())
}

/// The name a link reports, and the number inside it.
fn split_link(text: &str) -> Option<(&str, &str)> {
    let (name, rest) = text.split_once(':')?;
    let id = rest.strip_prefix('[')?.strip_suffix(']')?;
    Some((name, id))
}

fn open_ns(path: &str) -> Option<std::os::fd::OwnedFd> {
    openat(CWD, path, OFlags::RDONLY, Mode::empty()).ok()
}

fn raw_exit(code: i32) -> ! {
    unsafe {
        libc::syscall(libc::SYS_exit, code as libc::c_long);
        libc::_exit(code)
    }
}

fn main() {
    println!("t-ns-setns");
    let mut r = Report::new();

    // ---- The directory, and what its names are ----
    let names: Vec<String> = std::fs::read_dir("/proc/self/ns")
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut sorted = names.clone();
    sorted.sort();
    // Eight names now: USER joined the directory as the ladder's sixth rung, and
    // it brought *one* name rather than two because it moves its caller where PID
    // and TIME each have a `_for_children` slot (§6.3). The list is the kernel's
    // claim about what it can isolate, so a set written down here drifts the
    // moment it grows - which is why the probe below is the *complete* list rather
    // than a spot check.
    r.check(
        "the ns directory names the six namespaces this kernel has, eight names",
        sorted
            == vec![
                "mnt".to_string(),
                "net".to_string(),
                "pid".to_string(),
                "pid_for_children".to_string(),
                "time".to_string(),
                "time_for_children".to_string(),
                "user".to_string(),
                "uts".to_string(),
            ],
    );

    let mnt_link = readlink("/proc/self/ns/mnt");
    let uts_link = readlink("/proc/self/ns/uts");
    r.check(
        "the mnt link reads back as mnt:[<id>]",
        mnt_link
            .as_deref()
            .and_then(split_link)
            .is_some_and(|(name, id)| name == "mnt" && !id.is_empty()),
    );
    r.check(
        "the uts link reads back as uts:[<id>]",
        uts_link
            .as_deref()
            .and_then(split_link)
            .is_some_and(|(name, id)| name == "uts" && !id.is_empty()),
    );
    // One pool, so two namespaces of two *kinds* cannot claim one number
    // (measured on the host: its ten numbers are consecutive and distinct).
    r.check(
        "and the two kinds' numbers are different",
        match (
            mnt_link.as_deref().and_then(split_link),
            uts_link.as_deref().and_then(split_link),
        ) {
            (Some((_, a)), Some((_, b))) => a != b,
            _ => false,
        },
    );
    // The names this machine has no namespace for are the same answer a real
    // kernel gives for the ones it was built without: no entry at all. This list
    // has shrunk once per rung - TIME left it with the fifth (the `t-ns-time` pin
    // walks that entry end to end) and USER with the sixth (`t-ns-user` does the
    // same for its own).
    for absent in ["ipc", "cgroup"] {
        let path = format!("/proc/self/ns/{absent}");
        r.check(
            &format!("a namespace this kernel has not is ENOENT ({absent})"),
            readlink(&path).is_none() && open_ns(&path).is_none(),
        );
    }
    r.check(
        "and a pid that does not exist is ENOENT",
        open_ns("/proc/999999/ns/mnt").is_none(),
    );

    // ---- The descriptor a link gives ----
    let own_mnt = match open_ns("/proc/self/ns/mnt") {
        Some(fd) => fd,
        None => {
            r.check("opening /proc/self/ns/mnt gives a descriptor", false);
            r.finish();
        }
    };
    r.check("opening /proc/self/ns/mnt gives a descriptor", true);
    let mut buf = [0u8; 8];
    r.check(
        "and reading it is EINVAL, as it is on Linux",
        rustix::io::read(&own_mnt, &mut buf) == Err(Errno::INVAL),
    );
    r.check(
        "and seeking on it is ESPIPE",
        rustix::fs::seek(&own_mnt, rustix::fs::SeekFrom::Start(0)) == Err(Errno::SPIPE),
    );

    // ---- setns's flag word, and what is not a namespace (all measured) ----
    let raw = std::os::fd::AsRawFd::as_raw_fd(&own_mnt);
    r.check(
        "setns on a mount namespace with CLONE_NEWNS is served",
        setns(raw, CLONE_NEWNS) == 0,
    );
    r.check(
        "setns with a sibling kind's bit is EINVAL",
        setns(raw, CLONE_NEWUTS) == -22,
    );
    r.check(
        "setns with two namespace bits is EINVAL",
        setns(raw, CLONE_NEWNS | CLONE_NEWUTS) == -22,
    );
    r.check(
        // The word is `CLONE_NEWIPC`: a member of the namespace vocabulary this
        // machine has no descriptor for, so the bit cannot match the kind the
        // descriptor names. It has moved once per rung built (PID, then NET,
        // then TIME, then USER), which is the drift a hardcoded served set
        // always has.
        "setns with a word that names another namespace is EINVAL",
        setns(raw, CLONE_NEWIPC) == -22,
    );
    r.check(
        "setns with a bit that names a kind the descriptor is not is EINVAL",
        setns(raw, 0x1000_0000) == -22,
    );
    r.check(
        "setns with no word at all joins what the descriptor names",
        setns(raw, 0) == 0,
    );
    let (rx, tx) = match pipe() {
        Ok(pair) => pair,
        Err(_) => {
            r.check("a pipe is made for the not-a-namespace probe", false);
            r.finish();
        }
    };
    r.check(
        "setns on a pipe is EINVAL, not a wrong-shape success",
        setns(std::os::fd::AsRawFd::as_raw_fd(&rx), CLONE_NEWNS) == -22,
    );
    r.check(
        "setns on a number that names nothing is EBADF",
        setns(9999, CLONE_NEWNS) == -9,
    );

    // The privilege gate comes *after* all of that, which is the order the host
    // probe measured: a call whose word is wrong hears EINVAL even from a
    // caller who could not have joined anything.
    if host_run() {
        println!(
            "note: the sandbox refuses setns into one's own namespace (EPERM), so the joins above are this kernel's shape; the refusals are the host's own"
        );
    }

    // ---- The point of the surface: a descriptor *shares* a namespace ----
    //
    // The child unshares, mounts where only its namespace can see, and stays
    // alive; the caller then opens the child's link and joins it. If `setns`
    // copied - as `unshare` does - the caller would join an empty copy and see
    // nothing. Seeing the child's mount is the whole claim.
    let go = pipe();
    let (ready_r, ready_w) = match go {
        Ok(pair) => pair,
        Err(_) => {
            r.check("the handshake pipes are made", false);
            r.finish();
        }
    };
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child: its own mount namespace, one mount only it can see, then
        // wait until the parent says it has looked.
        let _ = unsafe { libc::syscall(libc::SYS_unshare, CLONE_NEWNS as libc::c_long) };
        let _ = mkdirat(CWD, INNER, Mode::from_raw_mode(0o755));
        let mounted = match mount("tmpfs", INNER, "tmpfs", MountFlags::empty(), None) {
            Ok(()) => None,
            Err(e) => Some(e),
        };
        // A marker inside that mount: the only way a *positive* check can tell
        // the child's namespace from a copy that happens to be empty. What the
        // child reports is *both* halves, so a marker that could not be written
        // is said out loud rather than reading as "the parent joined a copy".
        let marker = match openat(
            CWD,
            MARKER,
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o644),
        ) {
            Ok(file) => rustix::io::write(&file, b"inside\n").err(),
            Err(e) => Some(e),
        };
        let msg = match (mounted, marker) {
            (None, None) => String::from("mounted\n"),
            (None, Some(e)) => format!("mounted-no-marker:{}\n", e.raw_os_error()),
            (Some(e), _) => format!("!mounted:{}\n", e.raw_os_error()),
        };
        let _ = rustix::io::write(&ready_w, msg.as_bytes());
        drop(ready_w);
        // Hold still until the parent closes its end: the descriptor it opens
        // names *this* process's namespace, so the process has to be here.
        let mut go = [0u8; 1];
        let _ = rustix::io::read(&rx, &mut go);
        raw_exit(0);
    }
    r.check("the child is forked", child > 0);
    drop(ready_w);
    drop(rx);
    let mut said = [0u8; 32];
    let n = rustix::io::read(&ready_r, &mut said).unwrap_or(0);
    let report = String::from_utf8_lossy(&said[..n]).to_string();
    r.check(
        "the child unshared and mounted inside its own namespace",
        report.starts_with("mounted"),
    );
    // The marker has to be *there* for the claims below to mean anything: a
    // negative probe against a file that was never written pins nothing (the
    // rule this repository states as "a test that passes with and without the
    // change pins nothing"). The child is the one who can say so - the parent
    // cannot read the file by construction, which is exactly the claim right
    // after this one - so the verdict comes from its report, not from here.
    r.check(
        "and wrote its marker inside that mount (in its own namespace)",
        report == "mounted\n",
    );
    if !report.starts_with("mounted\n") {
        println!("  the child said: {report}");
    }
    if host_run() {
        println!(
            "note: the sharing claims need this kernel's namespaces; a host run measures only the refusals above"
        );
        r.finish();
    }
    // The caller's own namespace does not have it - which is `t-ns-mnt`'s
    // divergence, and the counterfactual for what follows. A *positive* probe:
    // the marker the child wrote is not readable from here.
    r.check(
        "the caller's namespace cannot see the child's marker",
        std::fs::read_to_string(MARKER).is_err(),
    );
    let child_link = format!("/proc/{child}/ns/mnt");
    let child_mnt = open_ns(&child_link);
    r.check(
        "another process's mount namespace opens by its pid",
        child_mnt.is_some(),
    );
    // The link text the caller reads once it has joined, kept so that the claim
    // after the mounter dies can say the *number* did not change rather than
    // that some link still answers.
    let mut joined_link: Option<String> = None;
    if let Some(fd) = &child_mnt {
        r.check(
            "and joining it is served",
            setns(std::os::fd::AsRawFd::as_raw_fd(fd), 0) == 0,
        );
        // The join's own observable, and a sharper one than any mount: the
        // caller's *own* link now reads the number it joined. A `setns` that
        // copied - or one that recorded the namespace but kept resolving
        // through the old table - would leave this saying the old number.
        r.check(
            "and the caller is now in the namespace it joined (its own link says so)",
            match (
                child_link.as_str(),
                readlink("/proc/self/ns/mnt").as_deref(),
            ) {
                (_, Some(mine)) => readlink(&child_link).as_deref() == Some(mine),
                _ => false,
            },
        );
        joined_link = readlink("/proc/self/ns/mnt");
        // Three views of the same question, because "the mount is in the
        // namespace" and "the walk crosses it" are answered by different code
        // and the measurement has to say which one is missing. `/proc/mounts`
        // renders the *caller's* table, so if the joined namespace carries the
        // mount the file names it; the walk over it is the claim beside that.
        let mine_mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        r.check(
            "the joined namespace's mount table names the child's mount",
            mine_mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(INNER)),
        );
        // The mount the child made is *in* the namespace the caller joined:
        // this is what "setns shares, unshare copies" means in one read.
        r.check(
            "the joined namespace carries the child's marker",
            std::fs::read_to_string(MARKER).ok().as_deref() == Some("inside\n"),
        );
        // No claim about mounting *again* here: a second mount at a busy
        // mountpoint is a mount stack, which Linux allows and this kernel does
        // too, so the observation would say nothing about the join. What the
        // join did is the marker above.
        //
        // The peel happens *once*, at the end of this function. An earlier
        // draft peeled here - before the child was killed - which made the
        // "outlives the mounter" claims below unmeasurable: the mount was
        // already gone, by the caller's own hand, so reading its absence as the
        // child's doing was reading the test's own act. That is the whole of
        // what this ordering fixes.
    }
    // The namespace outlives the process that made it, because the caller is in
    // it now: killing the child must not take the namespace or its contents with
    // it. Three claims, one per half - the number, the table entry, the bytes -
    // and all three taken with the mount still in place.
    unsafe {
        libc::kill(child, libc::SIGKILL);
    }
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(child, &mut status, 0) };
    r.check("the child is reaped", reaped == child);
    let link_after = readlink("/proc/self/ns/mnt");
    r.check(
        "the namespace outlives the process that made it (the caller's link still names it)",
        joined_link.is_some() && link_after == joined_link,
    );
    let mounts_after = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    r.check(
        "and its mount is still in the table the caller resolves through",
        mounts_after
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(INNER)),
    );
    r.check(
        "and the storage behind that mount is still readable",
        std::fs::read_to_string(MARKER).ok().as_deref() == Some("inside\n"),
    );
    // The peel, once, and last: the mount is the caller's to remove now, and
    // removing it is the *caller's* act rather than a side effect of the
    // process that made it going away.
    r.check(
        "and only the caller's own peel removes it",
        unmount(INNER, UnmountFlags::empty()).is_ok() && std::fs::read_to_string(MARKER).is_err(),
    );

    r.finish()
}
