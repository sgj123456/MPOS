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
//! The same rule governs what a *file* reports: an id is stored once, in the
//! parent namespace's numbering, and `stat`'s `st_uid`/`st_gid` (and `statx`'s
//! `stx_uid`/`stx_gid`) are that stored id mapped through the **reader's** table.
//! The two halves are one mechanism observed twice, so this program asks both -
//! a file created before the unshare reads the stored id from the machine map and
//! the inside id from the new one, with nothing about the file changed.
//!
//! # The grant, the other half of the rung
//!
//! Making a user namespace needs no capability, because it **is** the grant: the
//! caller holds the full capability set the instant it is in a namespace it made,
//! scoped to that namespace. On this single-user image every task starts with the
//! full set, so the only way to observe the grant is to *drop* first - a child
//! clears its effective and permitted sets (and a bounding capability), unshares
//! a namespace nested inside this one, and reads the full set back. Without the
//! grant the child would still be empty, which is the counterfactual the claims
//! are written against.
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
use rustix::fs::{AtFlags, CWD, Mode, OFlags, StatxFlags, openat};
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

/// The v3 capability wire shapes, as `<linux/capability.h>` spells them.
#[repr(C)]
#[derive(Clone, Copy)]
struct CapRecord {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

/// The uapi's `CAP_LAST_CAP` (40 capabilities, 0..=40): bits 0..=31 in the low
/// word, the top nine in the high one, so a full set reads `u32::MAX` and
/// `0x1ff`.
const CAP_HIGH: u32 = (1u32 << (40 - 31)) - 1;

/// `CAP_SYS_ADMIN`: the capability the namespace kinds are gated on, and the one
/// the bounding-set half of the grant drops and looks for again.
const CAP_SYS_ADMIN: u32 = 21;

/// `capget(header, data)` through the raw door - `libc` does not wrap it on this
/// target. The v3 version is the two-record shape.
fn capget(data: &mut [CapRecord; 2]) -> isize {
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let r = unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapHeader as libc::c_long,
            data.as_mut_ptr() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// `capset(header, data)`, setting the whole v3 set at once. The high word is
/// left empty: no capability this pins lives above bit 31.
fn capset(effective: u32, permitted: u32, inheritable: u32) -> isize {
    let mut header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [
        CapRecord {
            effective,
            permitted,
            inheritable,
        },
        CapRecord {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    let r = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &mut header as *mut CapHeader as libc::c_long,
            data.as_ptr() as libc::c_long,
        )
    };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
    }
}

/// `prctl` through the raw door, for the two bounding-set options the grant is
/// measured against.
fn prctl(op: libc::c_int, a2: usize, a3: usize, a4: usize, a5: usize) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_prctl, op as libc::c_long, a2, a3, a4, a5) };
    if r < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        r as isize
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

    // ---- The other half of the same translation: what a *file* reports ----
    //
    // `getuid` reads the ids the task stores about itself; `stat` reads the ids
    // an inode stores, and an inode keeps the id it was stamped with. Creating
    // the file *before* the unshare is what makes the boundary sharp: from here
    // the machine map is the identity, so its owner reads back as the stored id,
    // and the same file read from inside the new namespace reads the inside id -
    // the only thing that moved is the observer.
    let owned = "/tmp/mpos-userns-owner";
    let mut created = false;
    if let Ok(fd) = openat(
        CWD,
        owned,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    ) {
        created = true;
        let _ = write(&fd, b"owned\n");
    }
    let before = rustix::fs::lstat(owned).ok();
    r.check(
        "a file's owner reads back as the stored id under the identity map",
        created
            && before
                .as_ref()
                .is_some_and(|m| m.st_uid == outer && m.st_gid == outer),
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

    // ---- The same file, observed from inside the new namespace ----
    //
    // The claim cannot pass by accident: the inside id is **7**, a number no
    // stored id here is, so `st_uid == 7` is only possible if the kernel mapped
    // the stored id through the namespace's table. Without the translation this
    // reports the stored id (`0` here, `outer` on a host), which is exactly the
    // counterfactual the `getuid` half was written against.
    let inside = rustix::fs::lstat(owned).ok();
    r.check(
        "the same file, read from inside, reports the inside id the map gave it",
        inside
            .as_ref()
            .is_some_and(|m| m.st_uid == 7 && m.st_gid == 7),
    );

    // A file created *inside* is stamped with the caller's stored id too, and is
    // observed the same way - creation and observation agree.
    let made_inside = "/tmp/mpos-userns-created-inside";
    let created_inside = openat(
        CWD,
        made_inside,
        OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    )
    .is_ok();
    r.check(
        "a file made inside reads the same inside id",
        created_inside
            && rustix::fs::lstat(made_inside).is_ok_and(|m| m.st_uid == 7 && m.st_gid == 7),
    );
    let _ = rustix::fs::unlink(made_inside);

    // `statx` is the other door to the same question, and two answers to one
    // question that disagree are worse than two that are both approximate.
    match rustix::fs::statx(CWD, owned, AtFlags::empty(), StatxFlags::BASIC_STATS) {
        Ok(x) => r.check(
            &format!("statx agrees ({}, {})", x.stx_uid, x.stx_gid),
            x.stx_uid == 7 && x.stx_gid == 7,
        ),
        Err(_) => r.check("statx agrees with stat", false),
    }

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
    if cloned == 0 {
        // A host that served it: the child resumes here (the raw `clone` with a
        // null stack returns twice), in a fresh namespace with no map. It has no
        // claim of its own to make, so it leaves rather than running the rest of
        // the program a second time - which is what it did before this line
        // existed, and why the host run printed everything twice.
        unsafe { libc::_exit(0) }
    }
    if cloned > 0 {
        // The parent of a served clone: reap the child and accept the divergence.
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

    // ---- The rung's other half: entering a namespace *is* the grant ----
    //
    // The id translation above is one half of this kind; the other is that making
    // a user namespace needs no capability, because it hands the maker every one
    // of them - scoped to the namespace it just made. The claim cannot pass by
    // accident: the caller's own effective and permitted sets are read back
    // *empty* before the unshare and full after it, with only the unshare in
    // between, and a capability it dropped from its bounding set is read back
    // present afterwards. A child does this in a namespace nested inside the one
    // above, so nothing about the main flow moves.
    //
    // Dropping first is what makes the claim sharp: every task on this
    // single-user image *starts* with the full set, so "full inside" would be
    // indistinguishable from "full because it never dropped anything" unless the
    // caller empties itself going in.
    if let Ok((grant_r, grant_w)) = rustix::pipe::pipe() {
        let child = unsafe { libc::fork() };
        if child == 0 {
            drop(grant_r);
            // Drop CAP_SYS_ADMIN from the bounding set while CAP_SETPCAP is
            // still held to do it, then clear the sets outright.
            let dropped = prctl(libc::PR_CAPBSET_DROP, CAP_SYS_ADMIN as usize, 0, 0, 0);
            let bnd_before = prctl(libc::PR_CAPBSET_READ, CAP_SYS_ADMIN as usize, 0, 0, 0);
            let cleared = capset(0, 0, 0);
            let mut data = [CapRecord {
                effective: u32::MAX,
                permitted: u32::MAX,
                inheritable: u32::MAX,
            }; 2];
            let read_before = capget(&mut data);
            let empty = data[0].effective == 0
                && data[1].effective == 0
                && data[0].permitted == 0
                && data[1].permitted == 0;
            let moved = unshare_user();
            let read_after = capget(&mut data);
            let full = data[0].effective == u32::MAX
                && data[1].effective == CAP_HIGH
                && data[0].permitted == u32::MAX
                && data[1].permitted == CAP_HIGH;
            let bnd_after = prctl(libc::PR_CAPBSET_READ, CAP_SYS_ADMIN as usize, 0, 0, 0);
            let line = format!(
                "{} {} {} {} {} {} {} {} {}\n",
                dropped,
                bnd_before,
                cleared,
                read_before,
                empty as i32,
                moved,
                read_after,
                full as i32,
                bnd_after
            );
            let _ = write(&grant_w, line.as_bytes());
            drop(grant_w);
            unsafe { libc::_exit(0) }
        }
        drop(grant_w);
        let mut buf = [0u8; 256];
        let n = read(&grant_r, &mut buf).unwrap_or(0);
        drop(grant_r);
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        let mut w = text.split_whitespace().map(|s| s.parse::<i64>().ok());
        let dropped = w.next().flatten();
        let bnd_before = w.next().flatten();
        let cleared = w.next().flatten();
        let read_before = w.next().flatten();
        let empty = w.next().flatten();
        let moved = w.next().flatten();
        let read_after = w.next().flatten();
        let full = w.next().flatten();
        let bnd_after = w.next().flatten();
        r.check(
            "the caller empties its own effective and permitted sets",
            dropped == Some(0) && cleared == Some(0) && read_before == Some(0) && empty == Some(1),
        );
        r.check(
            "and CAP_SYS_ADMIN is out of its bounding set",
            bnd_before == Some(0),
        );
        r.check(
            "unshare(CLONE_NEWUSER) succeeds from inside a namespace (nesting)",
            moved == Some(0),
        );
        r.check(
            "entering the namespace hands the maker the full effective and permitted sets",
            read_after == Some(0) && full == Some(1),
        );
        r.check(
            "and a capability dropped from the bounding set is back",
            bnd_after == Some(1),
        );
    }

    // ---- The grant has a second door: setns into a child namespace ----
    //
    // Joining a namespace grants the same way `unshare` does (measured:
    // probe-userns7.c scenario E). A keeper child makes a namespace to join; the
    // caller keeps `CAP_SYS_ADMIN` alone - the one capability the join needs -
    // and reads the full set back afterwards. This is the last section, so the
    // caller is left in the joined namespace with nothing after it to disturb.
    if let Ok((keep_r, keep_w)) = rustix::pipe::pipe() {
        let keeper = unsafe { libc::fork() };
        if keeper == 0 {
            drop(keep_r);
            let moved = unshare_user();
            let _ = write(&keep_w, if moved == 0 { b"1" } else { b"0" });
            drop(keep_w);
            unsafe { libc::pause() };
            unsafe { libc::_exit(0) }
        }
        drop(keep_w);
        let mut ready = [0u8; 1];
        let _ = read(&keep_r, &mut ready);
        drop(keep_r);
        if ready[0] == b'1' {
            let kept = capset(1 << CAP_SYS_ADMIN, 1 << CAP_SYS_ADMIN, 0);
            let mut data = [CapRecord {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            }; 2];
            let read_kept = capget(&mut data);
            let only_admin = data[0].effective == (1 << CAP_SYS_ADMIN)
                && data[1].effective == 0
                && data[0].permitted == (1 << CAP_SYS_ADMIN);
            let path = format!("/proc/{keeper}/ns/user");
            let joined = match openat(CWD, path.as_str(), OFlags::RDONLY, Mode::empty()) {
                Ok(fd) => {
                    let rc = unsafe {
                        libc::setns(std::os::fd::AsRawFd::as_raw_fd(&fd), libc::CLONE_NEWUSER)
                    };
                    let e = unsafe { *libc::__errno_location() };
                    drop(fd);
                    if rc == 0 { 0 } else { -(e as isize) }
                }
                Err(e) => -(e.raw_os_error() as isize),
            };
            let read_joined = capget(&mut data);
            let full = data[0].effective == u32::MAX
                && data[1].effective == CAP_HIGH
                && data[0].permitted == u32::MAX
                && data[1].permitted == CAP_HIGH;
            r.check(
                "a caller may hold CAP_SYS_ADMIN alone",
                kept == 0 && read_kept == 0 && only_admin,
            );
            r.check(
                "setns(CLONE_NEWUSER) into a child namespace succeeds",
                joined == 0,
            );
            r.check(
                "and joining hands the caller the full effective and permitted sets",
                read_joined == 0 && full,
            );
        } else {
            r.check("the keeper child made a user namespace to join", false);
        }
        unsafe { libc::kill(keeper, libc::SIGKILL) };
        let mut status = 0;
        unsafe { libc::waitpid(keeper, &mut status, 0) };
    }

    let _ = rustix::fs::unlink(owned);
    let _ = Errno::INVAL;
    r.finish();
}
