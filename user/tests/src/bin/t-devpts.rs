//! t-devpts - what a pair's slave under `/dev/pts` is *born* with, and which
//! of `mount(2)`'s devpts options this kernel can make true.
//!
//! `mode=` and `gid=` are the two options an image's init asks for
//! (`mount -t devpts devpts /dev/pts -o mode=0620,gid=5`), and the mount's
//! third behaviour - the slave's uid - is the minting process's. Measured on
//! Linux 7.2.11 before any of this was written, with the probes
//! `build/scratch/devpts-probe/probe{2,3,4}.py` under `unshare -Urm`:
//!
//! - a mount with no options births the slave `0600 0:0` - the kernel's own
//!   default, not the `0620` a comment here used to claim (the `0620` a normal
//!   image shows is its init's request);
//! - `mode=` sets the permission bits and `gid=` the group, in the mount's
//!   namespace;
//! - `uid=` overrides the minting process's effective uid, which is otherwise
//!   what the slave is born owned by - and the minting process's *group* is
//!   ignored, because the mount's `gid=` wins (a root creator with `gid=` unset
//!   makes a `gid=0` slave even when its own gid is something else);
//! - an option the kernel cannot make true fails the mount with `EINVAL`
//!   rather than being ignored - measured with `nonsense=1`;
//! - each mount is its own instance by default (two mounts with different
//!   options, one after the other, do not disturb each other's pairs), which
//!   is why a pair's numbers are fixed when it is minted and never follow a
//!   later mount.
//!
//! What is claimed here, and why each is the shape a caller branches on:
//! `stat` of a slave minted under each mount answers that mount's numbers (the
//! claim `login`-style software reads before handing the terminal over);
//! a pair minted under an earlier mount keeps its numbers after later mounts;
//! `chown` on the slave moves the owner and leaves the mode (the hand-over
//! `login` performs); an unknown option is `EINVAL`; and `newinstance`,
//! `ptmxmode=` and `max=` - options this kernel cannot make true, because it
//! has one `/dev/ptmx` and no `/dev/pts/ptmx` node - are `EOPNOTSUPP` rather
//! than silently accepted.
//!
//! On a host run (`MPOS_ON_HOST=1`) mounting needs `CAP_SYS_ADMIN`, so the
//! ordinary-user case skips with its verdict printed, as `t-mount` does; the
//! payload always runs as root.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{AtFlags, CWD, Gid, Mode, OFlags, Uid, chown, mkdirat, open, stat, unlinkat};
use rustix::io::Errno;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;

const A: &str = "/tmp/t-devpts_a";
const B: &str = "/tmp/t-devpts_b";
const C: &str = "/tmp/t-devpts_c";
const R: &str = "/tmp/t-devpts_r";
const FILE: &str = "/tmp/t-devpts_r/file";

/// One `ioctl(2)`, the way `t-pty` asks: the request words are `libc`'s, and
/// the shape is the same three-word syscall.
fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: usize) -> (libc::c_long, i32) {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            fd as libc::c_long,
            request as libc::c_long,
            arg,
        )
    };
    (
        rc,
        if rc < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        } else {
            0
        },
    )
}

/// The master of a fresh pair, its number asked the way `ptsname`'s caller
/// asks it (`TIOCGPTN`), and the pair unlocked.
fn mint() -> Option<u32> {
    let master = open(c"/dev/ptmx", OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).ok()?;
    let mut number: u32 = 0;
    let (rc, _) = ioctl(
        master.as_raw_fd(),
        libc::TIOCGPTN as libc::c_ulong,
        &mut number as *mut u32 as usize,
    );
    if rc != 0 {
        return None;
    }
    let unlocked: i32 = 0;
    let (rc, _) = ioctl(
        master.as_raw_fd(),
        libc::TIOCSPTLCK as libc::c_ulong,
        &unlocked as *const i32 as usize,
    );
    if rc != 0 {
        return None;
    }
    Some(number)
}

/// `/dev/pts/<number>`, spelled as `ptsname` does.
fn slave(number: u32) -> CString {
    CString::new(format!("/dev/pts/{number}")).expect("a path with no NUL in it")
}

/// One claim: the slave answers `want` mode and owner, and reports the char
/// device it is. The numbers go into the line even when the claim holds, so
/// the log is the table this boot measured.
fn claim_slave(r: &mut Report, what: &str, number: u32, want: (u32, u32, u32)) {
    let path = slave(number);
    match stat(&path) {
        Ok(st) => {
            let (mode, uid, gid) = (
                (st.st_mode as u32) & 0o7777,
                st.st_uid as u32,
                st.st_gid as u32,
            );
            r.check(
                &format!(
                    "{what}: /dev/pts/{number} is {:04o} {}:{} (is {mode:04o} {uid}:{gid})",
                    want.0, want.1, want.2
                ),
                (mode, uid, gid) == want,
            );
            r.check(
                &format!("{what}: /dev/pts/{number} is still a character device"),
                (st.st_mode as u32) & 0o170_000 == 0o020_000,
            );
        }
        Err(e) => r.check(
            &format!("{what}: /dev/pts/{number} stats (stat: {e:?})"),
            false,
        ),
    }
}

/// `mount -t devpts devpts <dir> -o <options>`: what the image's init runs,
/// and what the payload's `/init` runs without options.
fn mount_devpts(r: &mut Report, what: &str, dir: &str, options: Option<&CStr>) -> bool {
    let ok = mount("devpts", dir, "devpts", MountFlags::empty(), options).is_ok();
    r.check(what, ok);
    ok
}

/// One refused mount's errno.
fn refused(r: &mut Report, what: &str, options: &CStr, want: Errno) {
    match mount("devpts", R, "devpts", MountFlags::empty(), Some(options)) {
        Ok(()) => {
            let _ = unmount(R, UnmountFlags::empty());
            r.check(&format!("{what} is refused (was accepted)"), false)
        }
        Err(e) => r.check(
            &format!("{what} is refused with {want:?} (is {e:?})"),
            e == want,
        ),
    }
}

fn main() {
    println!("t-devpts");
    let mut r = Report::new();

    // A leftover mount from an interrupted run would turn "the mount
    // succeeds" into EBUSY and hide everything after it.
    for dir in [A, B, C, R] {
        let _ = unmount(dir, UnmountFlags::empty());
    }
    let _ = unlinkat(CWD, FILE, AtFlags::empty());
    for dir in [A, B, C, R] {
        let _ = unlinkat(CWD, dir, AtFlags::REMOVEDIR);
    }
    for dir in [A, B, C, R] {
        let made = mkdirat(CWD, dir, Mode::from_raw_mode(0o755)).is_ok();
        r.check(&format!("{dir} is made"), made);
    }

    if host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: this host user cannot mount; the payload runs as root");
        r.finish()
    }

    // The option-less mount: Linux's own default is `0600`, owned by the
    // minting process (this payload's uid, root) in the mount's group.
    if !mount_devpts(&mut r, "mount -t devpts with no options succeeds", A, None) {
        r.finish()
    }
    let Some(p1) = mint() else {
        r.check("a pair can be minted through /dev/ptmx", false);
        r.finish()
    };
    r.check("a pair can be minted through /dev/ptmx", true);
    claim_slave(&mut r, "an option-less mount", p1, (0o600, 0, 0));
    // The same inode through the mount's own root: the option is the
    // mount's, not the node's, and the mount's view must agree.
    let through = CString::new(format!("{A}/{p1}")).expect("a path with no NUL in it");
    match (stat(&slave(p1)), stat(&through)) {
        (Ok(a), Ok(b)) => r.check(
            "the mount's own view reports the same numbers",
            (a.st_mode & 0o7777, a.st_uid, a.st_gid) == (b.st_mode & 0o7777, b.st_uid, b.st_gid),
        ),
        _ => r.check("the mount's own view reports the same numbers", false),
    }

    // `mode=` and `gid=` together: the image's own request.
    if !mount_devpts(
        &mut r,
        "mount -t devpts -o mode=0640,gid=5 succeeds",
        B,
        Some(c"mode=0640,gid=5"),
    ) {
        r.finish()
    }
    let Some(p2) = mint() else {
        r.check("a second pair can be minted", false);
        r.finish()
    };
    r.check("a second pair can be minted", true);
    claim_slave(&mut r, "a mount naming mode=0640,gid=5", p2, (0o640, 0, 5));

    // `mode=` alone.
    if !mount_devpts(
        &mut r,
        "mount -t devpts -o mode=0620 succeeds",
        C,
        Some(c"mode=0620"),
    ) {
        r.finish()
    }
    let Some(p3) = mint() else {
        r.check("a third pair can be minted", false);
        r.finish()
    };
    r.check("a third pair can be minted", true);
    claim_slave(&mut r, "a mount naming mode=0620", p3, (0o620, 0, 0));

    // The numbers are fixed at mint: the later mounts did not move the
    // pair born under the first one.
    claim_slave(
        &mut r,
        "the first pair, after two later mounts",
        p1,
        (0o600, 0, 0),
    );

    // `login`'s hand-over: `chown` the slave, and its mode stays.
    let path = slave(p3);
    match chown(&path, Some(Uid::from_raw(1000)), Some(Gid::from_raw(5))) {
        Ok(()) => r.check("chown on the slave succeeds", true),
        Err(e) => r.check(&format!("chown on the slave succeeds (is {e:?})"), false),
    }
    claim_slave(&mut r, "after the chown", p3, (0o620, 1000, 5));

    // Linux answers EINVAL for an option it does not know (measured).
    refused(&mut r, "nonsense=1", c"nonsense=1", Errno::INVAL);
    refused(&mut r, "mode=zz", c"mode=zz", Errno::INVAL);

    // Options this kernel cannot make true, named rather than ignored: there
    // is one `/dev/ptmx` for the machine and no `/dev/pts/ptmx` node, so a
    // fresh instance and a ptmx mode would each be a promise the tree cannot
    // keep, and `max=` bounds nothing it has a limit on.
    refused(&mut r, "newinstance", c"newinstance", Errno::NOTSUP);
    refused(&mut r, "ptmxmode=0666", c"ptmxmode=0666", Errno::NOTSUP);
    refused(&mut r, "max=2", c"max=2", Errno::NOTSUP);

    // A mount on a file is Linux's ENOTDIR, not a filesystem built anyway.
    r.check(
        "a regular file is made",
        open(
            FILE,
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o644),
        )
        .is_ok(),
    );
    r.check(
        "mounting devpts on a regular file is ENOTDIR",
        mount("devpts", FILE, "devpts", MountFlags::empty(), None) == Err(Errno::NOTDIR),
    );

    // Leave the tree as found: a mount left behind would be EBUSY for the
    // next run's setup.
    for dir in [A, B, C] {
        let _ = unmount(dir, UnmountFlags::empty());
    }
    let _ = unlinkat(CWD, FILE, AtFlags::empty());
    for dir in [A, B, C, R] {
        let _ = unlinkat(CWD, dir, AtFlags::REMOVEDIR);
    }

    r.finish()
}
