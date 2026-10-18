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
//!   kernel's own shape, named here rather than pinned as Linux's;
//! * **whose** capability it is was measured separately
//!   (`.workbuddy/probe_ns_owner.c`, a root caller in four places): Linux's
//!   `ns_capable(ns->user_ns, CAP_SYS_ADMIN)` asks the namespace being joined
//!   about its *owner*, so a caller inside a bare user namespace is `EPERM`
//!   joining the mount namespace it is standing in — the no-op every other kind
//!   answers `0` for, which is what fixes the question's place before the
//!   "already in it" shortcut — served joining one made inside its own, and the
//!   machine's root is served joining a *child* user namespace while a caller
//!   inside one is `EPERM` joining a sibling and `EPERM` joining its parent.
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
/// The sixth rung, and the one this file's last section is about: the namespace
/// the capability question is asked *of*.
const CLONE_NEWUSER: usize = 0x1000_0000;
/// A path only the child's namespace has, so that seeing it is seeing *its*
/// namespace rather than a coincidence.
const INNER: &str = "/tmp/t-ns-setns-inner";
/// And a file inside that mount: its contents are the child's, so reading them
/// after the join is the difference between joining a namespace and joining an
/// empty copy of one.
const MARKER: &str = "/tmp/t-ns-setns-inner/inside";
/// The mount the *owner* section's child makes inside a namespace of its own:
/// the object whose maker is the question.
const OWNED: &str = "/tmp/t-ns-setns-owned";
const OWNED_MARKER: &str = "/tmp/t-ns-setns-owned/inside";

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

/// `unshare(flags)` through the raw door, in the same shape: musl's `-1`
/// protocol, with the errno read back beside it.
fn unshare(flags: usize) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_unshare, flags as libc::c_long) };
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

    // ---- Whose capability the gate asks for ----
    //
    // The gate above asked *whether* the caller holds `CAP_SYS_ADMIN`; this
    // section asks **of whom**, which is the half the host probe measured and
    // this kernel had wrong: Linux's `ns_capable(ns->user_ns, CAP_SYS_ADMIN)`
    // asks the namespace being **joined** about its owner, so a caller inside a
    // bare user namespace is refused joining the mount namespace it is
    // *standing in* - the no-op every other kind answers `0` for, which is what
    // proves the question comes before the "already in it" shortcut - and served
    // joining one made inside its own. A child does all of it and reports over
    // the pipe, because the caller of this test cannot make a user namespace
    // for itself and then be inside it.
    //
    // Two pipes, not one, and the reason is this file's own lesson: the child
    // parks on the *second* one. A child that parks on the pipe it just wrote
    // its report into can consume the report's first byte before the parent has
    // read it, which is a race this section lost the first time it was run.
    let report_go = pipe();
    let stop_go = pipe();
    let (report_r, report_w) = match report_go {
        Ok(pair) => pair,
        Err(_) => {
            r.check("the report pipes are made", false);
            r.finish();
        }
    };
    let (stop_r, stop_w) = match stop_go {
        Ok(pair) => pair,
        Err(_) => {
            r.check("the stop pipes are made", false);
            r.finish();
        }
    };
    let owner = unsafe { libc::fork() };
    if owner == 0 {
        // The child: one user namespace of its own - empty maps, and the full
        // capability set *in it*, which is what `unshare(CLONE_NEWUSER)` hands
        // its maker - and then a mount namespace of its own, owned by it.
        drop(report_r);
        drop(stop_w);
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("user:{}", unshare(CLONE_NEWUSER)));
        // The refusing half: the mount namespace the child is standing in is
        // the machine's, so its owner is the machine's user namespace, and the
        // child is not privileged over it from below. The same call from the
        // machine's own namespace is the `0` above, which is what makes this
        // the order's claim rather than a plain refusal.
        let machine_mnt = open_ns("/proc/self/ns/mnt");
        lines.push(format!(
            "machine-mnt:{}",
            match &machine_mnt {
                Some(fd) => setns(std::os::fd::AsRawFd::as_raw_fd(fd), 0),
                None => -(libc::ENOENT as isize),
            }
        ));
        // A namespace of its own: a copy of the machine's table, whose owner is
        // the namespace the child stands in, with one mount only it can see.
        lines.push(format!("own-mnt:{}", unshare(CLONE_NEWNS)));
        let _ = mkdirat(CWD, OWNED, Mode::from_raw_mode(0o755));
        let mounted = match mount("tmpfs", OWNED, "tmpfs", MountFlags::empty(), None) {
            Ok(()) => None,
            Err(e) => Some(e),
        };
        let marker = match openat(
            CWD,
            OWNED_MARKER,
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o644),
        ) {
            Ok(file) => rustix::io::write(&file, b"owned\n").err(),
            Err(e) => Some(e),
        };
        lines.push(match (mounted, marker) {
            (None, None) => "mounted:0".to_string(),
            (None, Some(e)) => format!("mounted-no-marker:{}", e.raw_os_error()),
            (Some(e), _) => format!("!mounted:{}", e.raw_os_error()),
        });
        // The serving half, and it is the *same* no-op the machine's namespace
        // answers `0` for: the question is whose the namespace is, and now it is
        // the child's own. The join is a no-op here because a descriptor in this
        // kernel names the namespace its maker is standing in *now* rather than
        // the one it was opened in - the divergence §5.2 names - so the
        // discriminating pair is this `0` against the `machine-mnt` row above,
        // both asked of the same caller with the same set.
        lines.push(format!(
            "own-join:{}",
            match open_ns("/proc/self/ns/mnt") {
                Some(fd) => setns(std::os::fd::AsRawFd::as_raw_fd(&fd), 0),
                None => -(libc::ENOENT as isize),
            }
        ));
        let _ = rustix::io::write(&report_w, lines.join("\n").as_bytes());
        drop(report_w);
        // Parked on the *other* pipe, whose write end the parent holds: the
        // parent reads the report above and kills this process when it is done
        // looking at the namespace the child is standing in.
        let mut go = [0u8; 1];
        let _ = rustix::io::read(&stop_r, &mut go);
        raw_exit(0);
    }
    r.check("the owner child is forked", owner > 0);
    drop(report_w);
    drop(stop_r);
    let mut said = [0u8; 256];
    let n = rustix::io::read(&report_r, &mut said).unwrap_or(0);
    let lines: Vec<String> = String::from_utf8_lossy(&said[..n])
        .split('\n')
        .map(|s| s.to_string())
        .collect();
    let field = |name: &str| -> Option<isize> {
        lines
            .iter()
            .find(|l| l.starts_with(&format!("{name}:")))
            .and_then(|l| l.split_once(':').and_then(|(_, v)| v.parse().ok()))
    };
    r.check(
        "the child made a user namespace of its own",
        field("user") == Some(0),
    );
    // The order the probe measured: the flag word and the descriptor come
    // first, and *then* this question - so the answer here is `EPERM`, not the
    // `EINVAL` a wrong word gives, from a caller that could not have joined
    // anything anyway.
    r.check(
        "and is EPERM joining the mount namespace it is standing in (the no-op every other kind serves)",
        field("machine-mnt") == Some(-(libc::EPERM as isize)),
    );
    r.check(
        "the child made a mount namespace of its own",
        field("own-mnt") == Some(0),
    );
    r.check(
        "and mounted inside it, where only it can see",
        field("mounted") == Some(0),
    );
    r.check(
        "and is served joining that namespace, which is its own",
        field("own-join") == Some(0),
    );
    if field("user").is_none() || field("machine-mnt").is_none() {
        println!("  the child said: {}", lines.join(" | "));
    }
    // The other direction, from here: the machine's root *made* the child's
    // user namespace, so it holds every capability in it and below it - the
    // walk's third clause, and the reason the answer is the opposite of the one
    // a plain "is the caller root" question would give. The two claims that
    // look at the child's mount need the child to have made one, which is the
    // child's own report's business: a host run's container refuses a mount
    // inside a user namespace, and a claim read against a mount that was never
    // made would pin nothing.
    let owned_by_child = open_ns(&format!("/proc/{owner}/ns/mnt"));
    let joined_owned = match &owned_by_child {
        Some(fd) => setns(std::os::fd::AsRawFd::as_raw_fd(fd), 0),
        None => -(libc::ENOENT as isize),
    };
    r.check(
        "the machine's root is served joining a namespace its child user namespace owns",
        joined_owned == 0,
    );
    if field("mounted") != Some(0) {
        println!(
            "note: the child's mount inside its own namespace did not happen here, so the two claims about seeing it are not made"
        );
    } else {
        r.check(
            "and the child's mount is visible from there",
            std::fs::read_to_string(OWNED_MARKER).ok().as_deref() == Some("owned\n"),
        );
        // The way home is **another process's** link, and that is the pinning
        // gap §5.2 names made visible: the descriptor this caller came through
        // answers the namespace the caller is *standing in* now, so joining it
        // again is the no-op it would have been if it had named the machine's.
        // `/proc/1` is the machine's own - the one place a caller outside its
        // namespace can still name it - and the marker going away is what says
        // the move happened rather than that a door answered `0`.
        let home = open_ns("/proc/1/ns/mnt");
        let back_home = match &home {
            Some(fd) => setns(std::os::fd::AsRawFd::as_raw_fd(fd), 0),
            None => -(libc::ENOENT as isize),
        };
        r.check(
            "and the way back is the machine's own link (/proc/1), which takes the child's mount away",
            back_home == 0 && std::fs::read_to_string(OWNED_MARKER).is_err(),
        );
        // And the descriptor that was opened before all of this now answers
        // where the caller stands, which is the same fact from the other side.
        let again = setns(std::os::fd::AsRawFd::as_raw_fd(&own_mnt), 0);
        r.check(
            "the descriptor it came through answers where it stands now (a descriptor follows its maker)",
            again == 0 && std::fs::read_to_string(OWNED_MARKER).is_err(),
        );
    }
    unsafe {
        libc::kill(owner, libc::SIGKILL);
    }
    let mut owner_status = 0;
    let owner_reaped = unsafe { libc::waitpid(owner, &mut owner_status, 0) };
    r.check("the owner child is reaped", owner_reaped == owner);
    let _ = std::fs::remove_dir_all(OWNED);

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
