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
//!
//! # `clone3`'s `CLONE_INTO_CGROUP`
//!
//! The last claims here are the ones §6.10 deferred and the tree made
//! buildable: a child born in a group the caller names by descriptor rather
//! than where its parent is grouped. Every answer was measured on a current
//! host first (`.workbuddy/probe_clone3_cgroup*.c`, Linux 7.2.11): the child
//! and its own fork read the target's path while the caller's line stays
//! put, the door accepts an `O_PATH` descriptor and refuses a plain file, a
//! directory on another filesystem, a cgroup *attribute* file and an fd that
//! names nothing (`EBADF` each), a group removed after the descriptor was
//! opened is `ENOENT`, `CLONE_THREAD` beside the flag is `EOPNOTSUPP`, an
//! unprivileged caller is `EACCES`, and `CLONE_NEWCGROUP` beside the flag
//! roots the new namespace at the **child's** group - which the child then
//! reads as `0::/`.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, mkdir, open, statat};
use rustix::io::write;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};

/// `struct clone_args`, the uapi's eleven aligned words - `cgroup` last.
/// Spelled here rather than taken from `libc` for the reason `t-clone3`
/// gives: musl binds neither the struct nor a wrapper for the call.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// The struct's size at version 2, which is what this file passes.
const CLONE_ARGS_BYTES: usize = 88;
/// The flag word's two words this file uses, and the thread trio that is
/// refused beside the first.
const CLONE_INTO_CGROUP: usize = 0x0002_0000_0000;
const CLONE_NEWCGROUP: usize = 0x0200_0000;
const CLONE_THREAD: usize = 0x0001_0000;
const CLONE_VM: usize = 0x0000_0100;
const CLONE_SIGHAND: usize = 0x0000_0800;

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

    // ---- clone3's CLONE_INTO_CGROUP ----
    //
    // A group of its own for this section, and the process is still in
    // `/grp` from the section above: the two are siblings, so the first
    // common ancestor is the machine root and the caller's authority is
    // root's - the same question the host probe asked.
    std::fs::create_dir(format!("{AT}/into")).expect("mkdir into");
    let into_dir = open(
        format!("{AT}/into"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the target group");
    let into_fd = into_dir.as_raw_fd();
    let into = run_in_group(into_fd, 0, body_into);
    r.check(
        &format!("clone3(CLONE_INTO_CGROUP) is served and the child lands there (got {into:?})"),
        into == Ok(0),
    );
    let grandchild = run_in_group(into_fd, 0, body_into_fork);
    r.check(
        &format!("the child's own fork stays in the group (got {grandchild:?})"),
        grandchild == Ok(0),
    );
    r.check(
        "and the caller's own line did not move",
        read_path("/proc/self/cgroup").as_deref() == Some("0::/grp\n"),
    );
    // Into the caller's own group is a valid no-op, measured.
    let own = open(
        format!("{AT}/grp"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the caller's group");
    let same = run_in_group(own.as_raw_fd(), 0, body_in_own_group);
    r.check(
        &format!("the caller's own group is a served target (got {same:?})"),
        same == Ok(0),
    );
    // A namespace beside the flag is rooted at the child's group.
    let fresh_ns = run_in_group(into_fd, CLONE_NEWCGROUP, body_ns_root);
    r.check(
        &format!(
            "CLONE_NEWCGROUP beside it roots the namespace at the child's group (got {fresh_ns:?})"
        ),
        fresh_ns == Ok(0),
    );
    // The three descriptors that are not a group, and the one that is gone.
    let devnull = std::fs::File::open("/dev/null").expect("open /dev/null");
    let plain = clone_into(devnull.as_raw_fd(), 0).err();
    r.check(
        &format!("a regular file is EBADF, not a cgroup (got {plain:?})"),
        plain == Some(9),
    );
    let attribute = std::fs::File::open(format!("{AT}/into/cgroup.procs")).expect("open procs");
    let not_group = clone_into(attribute.as_raw_fd(), 0).err();
    r.check(
        &format!("a cgroup attribute file is EBADF too (got {not_group:?})"),
        not_group == Some(9),
    );
    let nowhere = clone_into(9999, 0).err();
    r.check(
        &format!("a descriptor that names nothing is EBADF (got {nowhere:?})"),
        nowhere == Some(9),
    );
    let opath = open(
        format!("{AT}/into"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the group O_PATH");
    let opath_into = run_in_group(opath.as_raw_fd(), 0, body_into);
    r.check(
        &format!("an O_PATH descriptor names the group (got {opath_into:?})"),
        opath_into == Ok(0),
    );
    std::fs::create_dir(format!("{AT}/gone")).expect("mkdir gone");
    let gone = open(
        format!("{AT}/gone"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open the doomed group");
    std::fs::remove_dir(format!("{AT}/gone")).expect("rmdir gone");
    let removed = clone_into(gone.as_raw_fd(), 0).err();
    r.check(
        &format!("a group removed after its descriptor opened is ENOENT (got {removed:?})"),
        removed == Some(2),
    );
    let thread = clone_into(into_fd, CLONE_THREAD | CLONE_VM | CLONE_SIGHAND).err();
    r.check(
        &format!("CLONE_INTO_CGROUP beside CLONE_THREAD is EOPNOTSUPP (got {thread:?})"),
        thread == Some(95),
    );
    // An unprivileged caller: the gate is the *common ancestor's*
    // `cgroup.procs` (0644 root), which is Linux's measured answer for a
    // caller that may not write the machine root's file.
    match unsafe { libc::fork() } {
        0 => {
            unsafe {
                libc::setgid(65534);
                libc::setuid(65534);
            }
            let code = match std::fs::File::open(format!("{AT}/into")) {
                Ok(file) => match clone_into(file.as_raw_fd(), 0) {
                    Err(13) => 0,
                    Err(e) => e,
                    Ok(_) => 9,
                },
                Err(e) => e.raw_os_error().unwrap_or(0),
            };
            unsafe { libc::_exit(code) }
        }
        kid => {
            let denied = wait_pid(kid).unwrap_or(-1);
            r.check(
                &format!(
                    "an unprivileged caller into the machine's group is EACCES (got {denied})"
                ),
                denied == 0,
            );
        }
    }
    std::fs::remove_dir(format!("{AT}/into")).expect("rmdir into");

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

/// A **fork-shaped** `clone3` into `cgfd` - no stack, so the child comes back
/// through this same call the way `fork` does and can run ordinary Rust.
/// `Err` is the door's own answer, read at the call (through musl's errno:
/// this goes through `libc::syscall`, not rustix's raw backend).
fn clone_into(cgfd: i32, extra: usize) -> Result<isize, i32> {
    let args = CloneArgs {
        flags: (CLONE_INTO_CGROUP | extra) as u64,
        cgroup: cgfd as u64,
        exit_signal: libc::SIGCHLD as u64,
        ..Default::default()
    };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs as *const core::ffi::c_void,
            CLONE_ARGS_BYTES as libc::c_long,
        )
    } as isize;
    if ret < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    } else {
        Ok(ret)
    }
}

/// Run `body` in a child born in `cgfd`, answering the child's exit code;
/// `Err` is the clone's own refusal.
fn run_in_group(cgfd: i32, extra: usize, body: fn() -> i32) -> Result<i32, i32> {
    let ret = clone_into(cgfd, extra)?;
    if ret == 0 {
        let code = body();
        unsafe { libc::_exit(code) }
    }
    Ok(wait_pid(ret as i32).unwrap_or(-1))
}

/// Whether this process's own `/proc/self/cgroup` is `want`.
fn line_is(want: &str) -> i32 {
    i32::from(read_path("/proc/self/cgroup").as_deref() != Some(want))
}

/// The child's own line, and then its own fork's: the membership a
/// `CLONE_INTO_CGROUP` child hands down.
fn line_with_own_fork(want: &str) -> i32 {
    if line_is(want) != 0 {
        return 2;
    }
    match unsafe { libc::fork() } {
        0 => unsafe { libc::_exit(line_is(want)) },
        pid if pid > 0 => wait_pid(pid).unwrap_or(3),
        _ => 4,
    }
}

fn body_into() -> i32 {
    line_is("0::/into\n")
}
fn body_into_fork() -> i32 {
    line_with_own_fork("0::/into\n")
}
fn body_in_own_group() -> i32 {
    line_is("0::/grp\n")
}
/// The fresh cgroup namespace's root is the child's *own* group, so it reads
/// its group as `/`.
fn body_ns_root() -> i32 {
    line_is("0::/\n")
}
