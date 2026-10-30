//! `/proc/<pid>` names the process it describes, and Linux makes the tree say
//! so: the directory that carries the name, and everything under it, is owned
//! by that process's ids rather than by root.
//!
//! Measured on a current host first (`build/scratch/attr2/powner*.c`, Linux
//! 7.2.11). The two halves are separate rules, which is why this pin walks
//! both:
//!
//! * the **directory that names a process** - `/proc/<pid>` and
//!   `/proc/<pid>/task/<tid>` - carries its `euid`/`egid` whatever happened to
//!   it, so a `setuid`-dropped process still owns its own entry;
//! * **everything inside** follows the process's `dumpable` flag, so the same
//!   dropped process's `status`, `fd` and `ns` answer **root** - its ids are
//!   no longer anybody's business but root's.
//!
//! The kernel's rules are `mpos_cred::task_dump_owner` and
//! `mpos_cred::process_dir_owner`, and its procfs hands them down on the way
//! out of `lookup`, so a node built on demand (the `fd` directory, a task's
//! directory) inherits even though it did not exist when the process directory
//! was made. `status`'s `Uid:`/`Gid:` lines are the same ids, printed four
//! times: real, effective, saved, and the filesystem one.
//!
//! Run as `--child`, the program is the dropped process it describes: it
//! changes its ids and then reports its own view of `/proc/self/status`.

use mpos_user_tests::Report;
use rustix::fs::stat;

/// The ids the child of this pin changes to, and the ones the claims below
/// expect to find on the tree: `nobody`/`nogroup`, which every image here has.
const DROPPED: u32 = 65_534;

fn child_report() -> String {
    // Both halves of the id: the ownership rule uses the pair, and a changed
    // euid is what clears the process's dumpability in the kernel.
    unsafe {
        libc::setgroups(0, core::ptr::null());
        if libc::setgid(DROPPED) != 0 {
            println!("child: setgid failed: {}", std::io::Error::last_os_error());
        }
        if libc::setuid(DROPPED) != 0 {
            println!("child: setuid failed: {}", std::io::Error::last_os_error());
        }
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut uid = String::new();
    let mut gid = String::new();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            uid = rest.trim().replace('\t', " ");
        }
        if let Some(rest) = line.strip_prefix("Gid:") {
            gid = rest.trim().replace('\t', " ");
        }
    }
    format!(
        "pid={} uid={} gid={} status_uid={uid} status_gid={gid}",
        std::process::id(),
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
    )
}

fn owner(path: &str) -> Option<(u32, u32)> {
    stat(path).ok().map(|st| (st.st_uid, st.st_gid))
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--child") {
        println!("{}", child_report());
        // Stay alive: the parent inspects `/proc/<pid>` while the process is
        // still there, and a pid whose process has exited has no directory.
        let mut byte = [0u8; 1];
        unsafe {
            libc::read(0, byte.as_mut_ptr().cast(), 1);
        }
        return;
    }

    let mut r = Report::new();
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        println!("     (the host reference needs root, as the measurements did; run with sudo)");
        r.finish();
    }

    let exe = std::env::current_exe().expect("the test's own path");
    use std::io::{BufRead, BufReader};
    let mut child = std::process::Command::new(exe)
        .arg("--child")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the child fixture");
    let mut said = String::new();
    if let Some(out) = child.stdout.take() {
        let mut reader = BufReader::new(out);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        said = line;
    }
    println!("     child: {}", said.trim());

    let mut pid = 0;
    for field in said.split_whitespace() {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse().unwrap_or(0);
        }
    }
    r.check(
        "the child changed its ids",
        said.contains(&format!("uid={DROPPED} gid={DROPPED}")),
    );
    if pid == 0 {
        r.check("the child reported a pid", false);
        r.finish();
    }

    // The directory that names the process: its own ids, whatever the process
    // did to itself.
    let dir = format!("/proc/{pid}");
    r.check(
        "the process's directory names the user it runs as",
        owner(&dir) == Some((DROPPED, DROPPED)),
    );
    r.check(
        "its thread directory carries the same ids",
        owner(&format!("{dir}/task/{pid}")) == Some((DROPPED, DROPPED)),
    );

    // Everything inside follows the dumpable flag: the process changed its
    // euid, so nothing but the two names above is still its.
    r.check(
        "a file inside it is root's, because the process made itself un-dumpable",
        owner(&format!("{dir}/status")) == Some((0, 0)),
    );
    r.check(
        "the fd directory follows the dumpability, not the name",
        owner(&format!("{dir}/fd")) == Some((0, 0)),
    );

    // The ids are also what the process reports about itself.
    r.check(
        "the status file reports the ids the process holds",
        said.contains(&format!(
            "status_uid={DROPPED} {DROPPED} {DROPPED} {DROPPED}"
        )) && said.contains(&format!(
            "status_gid={DROPPED} {DROPPED} {DROPPED} {DROPPED}"
        )),
    );

    // And a process that never changed them is root's, so the rule above is a
    // rule about a process and not about the tree.
    r.check(
        "a root process's directory is root's",
        owner("/proc/self") == Some((unsafe { libc::geteuid() }, unsafe { libc::getegid() })),
    );

    let _ = child.kill();
    let _ = child.wait();
    r.finish();
}
