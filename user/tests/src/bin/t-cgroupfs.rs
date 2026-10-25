//! t-cgroupfs - the cgroup2 tree: the nine files, the membership moves, the
//! removal rules, and `/proc/<pid>/cgroup` answering them.
//!
//! The shapes this pins were measured on a current host before the tree was
//! built (`src/vfs/cgroupfs.rs`'s header carries the probe): a
//! controller-less v2 group carries nine files, the empty half answers zero
//! bytes, `cgroup.procs` moves a process by any of its threads' numbers,
//! `cgroup.threads` moves one, and `rmdir` takes the two empties - no
//! children (`ENOTEMPTY`), no processes (`EBUSY`). A pid that names nothing
//! is `ESRCH`, junk is `EINVAL`, and an unknown controller is `ENOENT`.
//!
//! The claims that need *tasks* to move are this program's half of the
//! measurement - the host suite pins the file shapes, and a membership write
//! needs a live thread whose tid the file can name. The thread is
//! `std::thread::spawn`'s, whose kernel shape `t-clone` pins; what this test
//! adds is that its tid moves with its group and that `/proc/<tid>/cgroup`
//! says the same thing its leader's file does.
//!
//! The mount is the program's own (`mount -t cgroup2 none <dir>` on a
//! directory of `/tmp`'s tmpfs, unmounted at the end): the tree behind the
//! mount is the machine's one hierarchy, and a later mount binds the same
//! one - Linux's rule for the single writable hierarchy, and the reason a
//! test that cleans its groups leaves nothing behind but the mount point.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, mkdir, statat};
use rustix::io::write;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use std::sync::atomic::{AtomicI32, Ordering};

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A file's permission bits, through the call the other mode pins use.
fn mode_of(path: &str) -> Option<u32> {
    statat(CWD, path, AtFlags::empty())
        .ok()
        .map(|st| st.st_mode & 0o777)
}

const AT: &str = "/tmp/t-cgroupfs.mnt";

/// The errno the last failed call answered, for the claims that name one:
/// `ESRCH` for a pid that is not there, `EINVAL` for junk, `ENOENT` for a
/// controller the kernel has not, `ENOTEMPTY` and `EBUSY` for the two
/// removal rules.
static ERRNO: AtomicI32 = AtomicI32::new(0);

fn write_path(path: &str, text: &str) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map(|mut file| write(&file, text.as_bytes()))
    {
        Ok(Ok(n)) => n == text.len(),
        Ok(Err(_)) | Err(_) => {
            ERRNO.store(errno(), Ordering::Relaxed);
            false
        }
    }
}

fn read_path(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn pid_of_file(path: &str) -> Vec<u64> {
    read_path(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn main() {
    let mut r = Report::new();
    let pid = std::process::id();

    mkdir(AT, Mode::from_bits(0o755).unwrap()).ok();
    match mount("none", AT, "cgroup2", MountFlags::empty(), None) {
        Ok(()) => r.check("mount -t cgroup2 succeeds", true),
        Err(e) => {
            r.check(&format!("mount -t cgroup2 succeeds ({e:?})"), false);
            r.finish();
        }
    }

    // ---- the root's shape: no type file, the empty half, the members ----
    let root = |name: &str| format!("{AT}/{name}");
    r.check(
        "the root carries no cgroup.type",
        read_path(&root("cgroup.type")).is_none(),
    );
    r.check(
        "cgroup.controllers is the measured zero bytes",
        read_path(&root("cgroup.controllers")).as_deref() == Some(""),
    );
    r.check(
        "cgroup.subtree_control is empty too",
        read_path(&root("cgroup.subtree_control")).as_deref() == Some(""),
    );
    r.check(
        "the root's cgroup.procs lists this program",
        pid_of_file(&root("cgroup.procs")).contains(&(pid as u64)),
    );
    r.check(
        "the mount root is 0755 and the read-only half is 0444",
        mode_of(AT) == Some(0o755) && mode_of(&root("cgroup.stat")) == Some(0o444),
    );
    r.check(
        "cgroup.procs is one of the 0644 half",
        mode_of(&root("cgroup.procs")) == Some(0o644),
    );

    // ---- a group: the nine files, type among them ----
    std::fs::create_dir(format!("{AT}/grp")).expect("mkdir grp");
    let grp = |name: &str| format!("{AT}/grp/{name}");
    r.check(
        "a group carries cgroup.type, answering domain",
        read_path(&grp("cgroup.type")).as_deref() == Some("domain\n"),
    );
    r.check(
        "the group's max.depth answers max",
        read_path(&grp("cgroup.max.depth")).as_deref() == Some("max\n"),
    );
    r.check(
        "the group's events answer unpopulated and unfrozen",
        read_path(&grp("cgroup.events")).as_deref() == Some("populated 0\nfrozen 0\n"),
    );
    r.check(
        "the group's stat counts its descendants",
        read_path(&grp("cgroup.stat")).as_deref()
            == Some("nr_descendants 0\nnr_dying_descendants 0\n"),
    );

    // ---- the membership moves ----
    r.check(
        "writing this pid moves this process into the group",
        write_path(&grp("cgroup.procs"), &format!("{pid}\n")),
    );
    r.check(
        "/proc/self/cgroup says 0::/grp",
        read_path("/proc/self/cgroup").as_deref() == Some("0::/grp\n"),
    );
    r.check(
        "the group's procs file lists the pid back",
        pid_of_file(&grp("cgroup.procs")) == vec![pid as u64],
    );
    r.check(
        "the group is populated now",
        read_path(&grp("cgroup.events")).as_deref() == Some("populated 1\nfrozen 0\n"),
    );

    // A thread of this process: its tid is not the pid, and the whole group
    // moves with a procs write.
    let tid = std::sync::Arc::new(AtomicI32::new(0));
    let tid_in_thread = tid.clone();
    let handle = std::thread::spawn(move || {
        use rustix::thread::gettid;
        tid_in_thread.store(gettid().as_raw_nonzero().get() as i32, Ordering::Relaxed);
        // The thread parks where its creator left it: the move below makes
        // its group the one being read.
        loop {
            std::thread::yield_now();
        }
    });
    let mut spins = 0;
    while tid.load(Ordering::Acquire) == 0 && spins < 100_000_000 {
        spins += 1;
        std::thread::yield_now();
    }
    let tid = tid.load(Ordering::Acquire) as u64;
    r.check(
        "the thread has a tid of its own",
        tid > 0 && tid != pid as u64,
    );
    r.check(
        "the thread's own cgroup file agrees with its leader's",
        read_path(&format!("/proc/{tid}/cgroup")).as_deref() == Some("0::/grp\n"),
    );

    // ---- the refusals, each with its errno ----
    //
    // Every errno here is read at the call that made it - a shared cell goes
    // stale the moment any other call intervenes, which is exactly the bug
    // the first draft had (the rmdir probe below compared the PREVIOUS
    // write's errno).
    // The write's errno is **rustix's own** (`Errno::raw_os_error`), and that
    // is a measured fact about this payload, not a style choice: rustix's
    // linux_raw backend issues the raw syscall and maps the negative return
    // itself, never touching musl's TLS errno - so `last_os_error` here reads
    // whatever earlier musl call set it (measured in the gate: the kernel
    // answered -22 while `last_os_error` still said 2). The std doors below
    // (`remove_dir`) do go through musl, and their errnos come from
    // `std::io::Error`.
    let put_err = |text: &str, path: &str| -> i32 {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Err(e) => e.raw_os_error().unwrap_or(0),
            Ok(mut file) => match write(&file, text.as_bytes()) {
                Ok(n) if n == text.len() => 0,
                Ok(_) => errno(),
                Err(e) => e.raw_os_error(),
            },
        }
    };
    let junk = put_err("notapid", &grp("cgroup.procs"));
    r.check(
        &format!("junk in cgroup.procs is EINVAL (got {junk})"),
        junk == 22,
    );
    let absent = put_err("999999999", &grp("cgroup.procs"));
    r.check(
        &format!("a pid that names nothing is ESRCH (got {absent})"),
        absent == 3,
    );
    let unknown = put_err("+nosuch", &grp("cgroup.subtree_control"));
    r.check(
        &format!("an unknown controller is ENOENT (got {unknown})"),
        unknown == 2,
    );

    // ---- the removal rules, in Linux's order ----
    std::fs::create_dir(format!("{AT}/grp/sub")).expect("mkdir grp/sub");
    let with_child = std::fs::remove_dir(format!("{AT}/grp"))
        .err()
        .and_then(|e| e.raw_os_error());
    r.check(
        &format!("rmdir of a group with a child is ENOTEMPTY (got {with_child:?})"),
        with_child == Some(39),
    );
    std::fs::remove_dir(format!("{AT}/grp/sub")).expect("rmdir grp/sub");
    // Still populated (this process is in the group): a rmdir from outside
    // - a forked child, since this program *is* the member - is EBUSY.
    match unsafe { libc::fork() } {
        -1 => r.check("fork succeeds for the rmdir probe", false),
        0 => {
            let busy = std::fs::remove_dir(format!("{AT}/grp")).is_err()
                && std::io::Error::last_os_error().raw_os_error() == Some(16);
            let code = if busy { 0 } else { 1 };
            unsafe {
                libc::syscall(libc::SYS_exit_group, code as libc::c_long);
                libc::_exit(code)
            }
        }
        child => {
            let status = wait_pid(child);
            r.check(
                "rmdir of a populated group, from outside it, is EBUSY",
                status == Some(0),
            );
        }
    }

    // ---- back out, and the tree closes cleanly ----
    r.check(
        "writing this pid to the root's procs moves it back",
        write_path(&root("cgroup.procs"), &format!("{pid}\n")),
    );
    r.check(
        "/proc/self/cgroup says 0::/ again",
        read_path("/proc/self/cgroup").as_deref() == Some("0::/\n"),
    );
    drop(handle);
    std::fs::remove_dir(format!("{AT}/grp")).expect("rmdir the emptied group");
    r.check(
        "the emptied group removes",
        !std::path::Path::new(&format!("{AT}/grp")).exists(),
    );

    // The written depth limit is the mkdir answer, measured live.
    write_path(&root("cgroup.max.depth"), "0");
    match std::fs::create_dir(format!("{AT}/deepend")) {
        Err(e) => r.check(
            "a max.depth of 0 makes mkdir answer EBUSY",
            e.raw_os_error() == Some(16),
        ),
        Ok(()) => {
            r.check("a max.depth of 0 makes mkdir answer EBUSY", false);
            std::fs::remove_dir(format!("{AT}/deepend")).ok();
        }
    }
    write_path(&root("cgroup.max.depth"), "max");

    let _ = unmount(AT, UnmountFlags::empty());
    r.finish()
}

/// The blocking `wait4(pid)`, answering the child's own exit code.
fn wait_pid(child: i32) -> Option<i32> {
    use rustix::process::{Pid, WaitOptions, waitpid};
    match waitpid(Pid::from_raw(child), WaitOptions::empty()) {
        Ok(Some((_, status))) => rustix::process::WaitStatus::exit_status(status),
        _ => None,
    }
}
