//! t-ns-user - the ids a process reads, which are a namespace's translation of
//! the ones the machine stores.
//!
//! §6.3's ladder puts the user namespace sixth (UTS, MNT, PID, NET, TIME, USER)
//! and this program holds the pair the plan fixed for every rung - **the caller's
//! view differs and it changes because of what it did** - which for this kind is
//! the whole mechanism:
//!
//! * the machine's map is the **identity** (`0 0 4294967295`), so before anything
//!   unshares every id reads back as itself and nothing here changes a task that
//!   never asks;
//! * `unshare(CLONE_NEWUSER)` **moves the caller** into a namespace whose table is
//!   **empty**, so its own id immediately reads `65534` - the overflow id - until
//!   it writes the map;
//! * writing `/proc/self/uid_map` is what gives the inside an id a name, and the
//!   line is `inside outside length` in the **parent's** numbering, which is why
//!   the write has to name the id the caller is *stored* under and not the one it
//!   is currently shown as;
//! * the map is written **once**: a second write is `EPERM`, because a process's
//!   view of "who am I" must not move under it;
//! * `gid_map` and `setgroups` are a pair the other way round - turning
//!   `setgroups` to `deny` is what lets an unprivileged writer set the gid map -
//!   and once the gid map exists `setgroups` cannot be turned back on.
//!
//! # The doors
//!
//! `rustix::thread` wraps `unshare`, but the claims here are about the *numbers*
//! - the exact text of three procfs files, the errno of a second write, the
//! `getuid()` between two calls - so the raw `libc` doors are used throughout:
//! `unshare`, `setns`, and a bare `SYS_clone` for the flag word. The `getuid()`
//! family is the libc one, because what is being pinned is what a C program
//! reads.
//!
//! # What a host run measures
//!
//! Most of it, and that is unusual for this ladder: unprivileged user namespaces
//! are enabled on the host this was written against, so `unshare(CLONE_NEWUSER)`
//! really answers `0` there and the moves below are observable. The one claim
//! that differs on a real kernel is `clone(CLONE_NEWUSER)`, which Linux *serves*
//! and this kernel refuses with `EOPNOTSUPP` because the child-side creation is
//! the piece of this rung that is named and not built (§6.3); that claim accepts
//! the host's served answer only under `host_run()`.
//!
//! # What is *not* pinned here, and why
//!
//! The unprivileged half of the map rules - one line, one id wide, naming only
//! the writer's own id - is a rule about a caller without `CAP_SETUID`, and every
//! task on this machine holds it. Those three refusals are argued with by the leaf
//! crate's host tests (`crates/userns`, `an_unprivileged_writer_may_name_only_itself_once`),
//! which is where a rule that needs two privilege levels belongs.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::{Errno, read, write};

/// The ids of a real kernel's identity map, as the map file prints them.
const IDENTITY: &str = "         0          0 4294967295\n";

/// The overflow id: what an id the table does not cover reads back as, and what
/// a caller reads for everything the instant it has unshared into an empty one.
const NO_MAP_ID: u32 = 65534;

/// Read a whole procfs file, or `None` if it cannot be opened or read.
fn read_file(path: &str) -> Option<String> {
    let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut buf = vec![0u8; 512];
    let mut len = 0;
    loop {
        match read(&fd, &mut buf[len..]) {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if len == buf.len() {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

/// Write a whole procfs file, answering the errno (`0` for success) - the errno
/// *is* the claim for several of the writes below.
fn write_file(path: &str, text: &str) -> isize {
    let fd = match openat(CWD, path, OFlags::WRONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => return -(e.raw_os_error() as isize),
    };
    match write(&fd, text.as_bytes()) {
        Ok(_) => 0,
        Err(e) => -(e.raw_os_error() as isize),
    }
}

fn readlink(path: &str) -> Option<String> {
    rustix::fs::readlinkat(CWD, path, Vec::new())
        .ok()?
        .into_string()
        .ok()
}

/// `unshare(CLONE_NEWUSER)`, as an errno (`0` for success).
fn unshare_user() -> isize {
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
    if rc == 0 {
        0
    } else {
        -(unsafe { *libc::__errno_location() } as isize)
    }
}

fn main() {
    let mut r = Report::new();

    // ---- The machine's namespace, before anything moves ----
    //
    // Every observation below is of the state a task that has never unshared is
    // in, and the identity map is what makes this rung a no-op for such a task.
    let machine_link = readlink("/proc/self/ns/user");
    r.check(
        "the machine's user namespace is named",
        machine_link
            .as_deref()
            .is_some_and(|t| t.starts_with("user:[")),
    );

    // The kind moves its caller, so it has no children's slot and the directory
    // has no `user_for_children` name - the measured shape PID and TIME have
    // differently (both of those *do*).
    r.check(
        "there is no user_for_children, because the kind moves its caller",
        readlink("/proc/self/ns/user_for_children").is_none(),
    );

    r.check(
        "the machine's uid map is the identity, in a real kernel's exact bytes",
        read_file("/proc/self/uid_map").as_deref() == Some(IDENTITY),
    );
    r.check(
        "and its gid map is the same",
        read_file("/proc/self/gid_map").as_deref() == Some(IDENTITY),
    );
    r.check(
        "and setgroups reads allow",
        read_file("/proc/self/setgroups").as_deref() == Some("allow\n"),
    );

    // The identity map is *live*: the id a task is stored under reads back
    // unchanged, which is the property that keeps this rung from disturbing
    // anything that never unshares. Captured here so the write below can name it.
    let outer = unsafe { libc::getuid() } as u32;
    r.check(
        "an id reads as itself under the identity map",
        outer == unsafe { libc::geteuid() } as u32,
    );

    // The machine's map is already written, so a write to it is refused - there
    // is nothing left for it to say.
    r.check(
        "the machine's uid map refuses a write with EPERM",
        write_file("/proc/self/uid_map", "0 0 1\n") == -(libc::EPERM as isize),
    );

    // ---- The door: unshare moves the caller into an empty namespace ----
    r.check("unshare(CLONE_NEWUSER) succeeds", unshare_user() == 0);
    let now_link = readlink("/proc/self/ns/user");
    r.check(
        "...and the caller itself moved, unlike PID's and TIME's rule",
        now_link.is_some() && now_link != machine_link,
    );

    // The headline: the new table is empty, so the caller's own stored id has no
    // inside name yet and everything reads as the overflow id.
    r.check(
        "the caller reads the overflow id for everything, its own id included",
        unsafe { libc::getuid() } as u32 == NO_MAP_ID
            && unsafe { libc::geteuid() } as u32 == NO_MAP_ID
            && unsafe { libc::getgid() } as u32 == NO_MAP_ID,
    );
    r.check(
        "and the new map reads empty",
        read_file("/proc/self/uid_map").as_deref() == Some(""),
    );

    // ---- The map is what gives the inside an id a name ----
    //
    // The line names the id the caller is *stored* under, which is the machine's
    // numbering - the point of a map being a translation at the boundary rather
    // than a rename. The inside id is deliberately **7 and not 0**: this machine
    // stores every task under id 0, so a map to 0 would read back the same number
    // whether the kernel translated at all, and the claim would pass for the wrong
    // reason. 7 is a number no stored id here is, so reading it back is only
    // possible if the translation really ran.
    let written = write_file("/proc/self/uid_map", &format!("7 {outer} 1\n"));
    r.check("writing the uid map is accepted once", written == 0);
    r.check(
        "...and then the caller reads the inside id the map gave it",
        unsafe { libc::getuid() } as u32 == 7 && unsafe { libc::geteuid() } as u32 == 7,
    );
    let expect_map = format!("{:>10} {:>10} {:>10}\n", 7, outer, 1);
    r.check(
        "the map reads back in a real kernel's field widths",
        read_file("/proc/self/uid_map").as_deref() == Some(expect_map.as_str()),
    );
    r.check(
        "a second write is EPERM - the map is a promise made once",
        write_file("/proc/self/uid_map", &format!("7 {outer} 1\n")) == -(libc::EPERM as isize),
    );

    // ---- gid_map and setgroups are a pair the other way round ----
    r.check(
        "setgroups can be turned to deny",
        write_file("/proc/self/setgroups", "deny\n") == 0,
    );
    r.check(
        "...and then reads deny",
        read_file("/proc/self/setgroups").as_deref() == Some("deny\n"),
    );
    r.check(
        "the gid map takes the same one-line write",
        write_file("/proc/self/gid_map", &format!("7 {outer} 1\n")) == 0,
    );
    r.check(
        "...and the caller's gid reads the mapped id",
        unsafe { libc::getgid() } as u32 == 7 && unsafe { libc::getegid() } as u32 == 7,
    );
    r.check(
        "setgroups cannot be turned back on once the gid map exists",
        write_file("/proc/self/setgroups", "allow\n") == -(libc::EPERM as isize),
    );

    // ---- A child born here is in the same namespace, reading the same ids ----
    let report = rustix::pipe::pipe();
    if let Ok((report_r, report_w)) = report {
        let child = unsafe { libc::fork() };
        if child == 0 {
            drop(report_r);
            let line = format!(
                "{} {} {}\n",
                unsafe { libc::getuid() },
                readlink("/proc/self/ns/user").unwrap_or_default(),
                now_link.clone().unwrap_or_default()
            );
            let _ = write(&report_w, line.as_bytes());
            drop(report_w);
            unsafe { libc::_exit(0) }
        }
        drop(report_w);
        let mut buf = [0u8; 256];
        let n = read(&report_r, &mut buf).unwrap_or(0);
        drop(report_r);
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        let mut words = text.split_whitespace();
        let child_uid = words.next().and_then(|w| w.parse::<u32>().ok());
        let child_ns = words.next().unwrap_or_default();
        r.check(
            "a child of the namespace reads the mapped id and names the same namespace",
            child_uid == Some(7)
                && now_link.as_deref() == Some(child_ns)
                && libc::WIFEXITED(status)
                && libc::WEXITSTATUS(status) == 0,
        );
    }

    // ---- The one door this kernel does not serve ----
    //
    // `clone(CLONE_NEWUSER)` is a real door on Linux (its bit, `0x10000000`, is
    // outside the signal byte, so unlike `CLONE_NEWTIME` a caller *can* name it),
    // and this kernel refuses it by name because the child-side creation is the
    // part of the rung that is named and not built (§6.3). The claim accepts the
    // host's served answer only when the run says it is on a host.
    let cloned = unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::CLONE_NEWUSER | libc::SIGCHLD,
            0usize,
            0usize,
            0usize,
            0usize,
        )
    };
    if cloned > 0 {
        // A host that served it: reap the child and accept the divergence.
        let mut status = 0;
        unsafe { libc::waitpid(cloned as libc::pid_t, &mut status, 0) };
    }
    r.check(
        "clone(CLONE_NEWUSER) is EOPNOTSUPP here - Linux serves it, mpos does not yet",
        cloned == -1 && unsafe { *libc::__errno_location() } == libc::EOPNOTSUPP
            || (host_run() && cloned > 0),
    );

    // ---- Joining the namespace the caller is already in ----
    //
    // The measured oddity of this kind: every other `setns` onto one's own
    // namespace is a no-op that succeeds, and this one is `EINVAL`.
    match openat(CWD, "/proc/self/ns/user", OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => {
            let rc =
                unsafe { libc::setns(std::os::fd::AsRawFd::as_raw_fd(&fd), libc::CLONE_NEWUSER) };
            let errno = unsafe { *libc::__errno_location() };
            r.check(
                "setns onto the namespace the caller is already in is EINVAL",
                rc == -1 && errno == libc::EINVAL,
            );
        }
        Err(_) => r.check("the namespace descriptor is openable", false),
    }

    let _ = Errno::INVAL;
    r.finish();
}
