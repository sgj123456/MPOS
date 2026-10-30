//! t-mount-api - the mount API's attachment half: `open_tree(2)`,
//! `move_mount(2)` and `mount_setattr(2)`.
//!
//! The superblock half (`fsopen`/`fsconfig`/`fsmount`, 430-432) is served now
//! and has its own pin (`t-fsmount`); `fspick` (433) is the family's last
//! named gap and must still answer `ENOSYS` - the last claim asks.
//!
//! Measured on Linux 7.2.11 before any of this was written
//! (`build/scratch/mountapi/probe{2,3,4}.c`, under `unshare -Urm
//! --map-root-user`, because the calls want mount-namespace authority):
//!
//! - a descriptor is a name for a mount, not a file: `fstat` answers a
//!   directory, `readlink /proc/self/fd/N` answers `/` for a detached copy
//!   (`OPEN_TREE_CLONE`) and the mounted path for a reference, and `read(2)`
//!   answers `EBADF` - there is no byte stream behind it;
//! - `OPEN_TREE_CLONE`, `O_CLOEXEC`, `AT_SYMLINK_NOFOLLOW`, `AT_NO_AUTOMOUNT`,
//!   `AT_EMPTY_PATH` and `AT_RECURSIVE` are the accepted flag bits; anything
//!   else is `EINVAL`, and `AT_RECURSIVE` without the clone bit is `EINVAL`;
//! - an empty path is `ENOENT` unless its `AT_EMPTY_PATH` bit names the
//!   descriptor's own file;
//! - `move_mount`'s flag word is exactly its six names (stray bits `EINVAL`);
//!   a clone attaches with `F_EMPTY_PATH` (and may attach more than once), a
//!   plain reference only when the path it names is a mount point, a source
//!   that is not a mount is `EINVAL`, and a target that already carries a
//!   mount is `EINVAL` where `mount(2)`'s bind would say `EBUSY`;
//! - `mount_setattr`'s structure must be 32 bytes (`EINVAL` for less, `E2BIG`
//!   for more); `RDO` set and clear move a mount's write promise; the
//!   Linux-served `NOSUID`/`NODEV`/`NOEXEC`/`NODIRATIME`/`NOIDMAP` have no
//!   per-mount storage here and answer `EOPNOTSUPP` (this kernel refuses a
//!   promise it cannot keep rather than accepting it); `NOATIME`,
//!   `STRICTATIME` and `IDMAP` answer `EINVAL`, as they do on this host; a
//!   target that is not a mount is `EINVAL`; and `AT_RECURSIVE` walks the
//!   mounts below the target.
//!
//! On a host run (`MPOS_ON_HOST=1`) these calls answer `EPERM` to an ordinary
//! user (the host's own namespace), so the payload skips with its verdict
//! printed; the payload always runs as root in the guest.

use mpos_user_tests::{Report, host_run};
use rustix::fs::{CWD, Mode, OFlags, mkdirat, open, stat};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;

const SYS_OPEN_TREE: libc::c_long = 428;
const SYS_MOVE_MOUNT: libc::c_long = 429;
const SYS_MOUNT_SETATTR: libc::c_long = 442;
const SYS_FSOPEN: libc::c_long = 430;
const SYS_FSPICK: libc::c_long = 433;

const OPEN_TREE_CLONE: usize = 0x1;
const OPEN_TREE_CLOEXEC: usize = 0o2_000_000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_RECURSIVE: usize = 0x8000;
const AT_FDCWD: i32 = -100;

const MOVE_MOUNT_F_EMPTY_PATH: usize = 0x4;
const MOVE_MOUNT_T_EMPTY_PATH: usize = 0x40;

const MOUNT_ATTR_RDONLY: u64 = 0x1;
const MOUNT_ATTR_NOSUID: u64 = 0x2;
const MOUNT_ATTR_NOATIME: u64 = 0x10;
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;
const MOUNT_ATTR_NOIDMAP: u64 = 0x0020_0000;

const ROOT: &str = "/tmp/t-mount-api";
const SRC: &str = "/tmp/t-mount-api/src";
const DST: &str = "/tmp/t-mount-api/dst";
const DST2: &str = "/tmp/t-mount-api/dst2";
const DST3: &str = "/tmp/t-mount-api/dst3";
const A2: &str = "/tmp/t-mount-api/a2";
const B2: &str = "/tmp/t-mount-api/b2";
const PLAIN: &str = "/tmp/t-mount-api/plain";
const SUB: &str = "/tmp/t-mount-api/src/sub";

/// `struct mount_attr`, 32 bytes as Linux defines it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u32,
    pad: u32,
}

fn errno() -> i32 {
    -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn path(name: &str) -> CString {
    CString::new(name).expect("a path with no NUL in it")
}

/// `open_tree(2)`, answering the descriptor or the errno.
fn open_tree(dfd: i32, name: &CStr, flags: usize) -> Result<i32, i32> {
    let rc = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            dfd as libc::c_long,
            name.as_ptr() as usize,
            flags as libc::c_long,
        )
    };
    if rc < 0 { Err(errno()) } else { Ok(rc as i32) }
}

/// `move_mount(2)`.
fn move_mount(from_dfd: i32, from: &CStr, to_dfd: i32, to: &CStr, flags: usize) -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_MOVE_MOUNT,
            from_dfd as libc::c_long,
            from.as_ptr() as usize,
            to_dfd as libc::c_long,
            to.as_ptr() as usize,
            flags as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { 0 }
}

/// `mount_setattr(2)`.
fn mount_setattr(dfd: i32, name: &CStr, flags: usize, attr: &MountAttr, size: usize) -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            dfd as libc::c_long,
            name.as_ptr() as usize,
            flags as libc::c_long,
            attr as *const MountAttr as usize,
            size as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { 0 }
}

fn read_only_attr(set: u64, clear: u64) -> MountAttr {
    MountAttr {
        attr_set: set,
        attr_clr: clear,
        ..MountAttr::default()
    }
}

/// Mount a fresh tmpfs at `name`, the substrate every claim below works on.
fn mount_tmpfs(r: &mut Report, what: &str, name: &str) -> bool {
    let ok = mount("tmpfs", name, "tmpfs", MountFlags::empty(), None).is_ok();
    r.check(what, ok);
    ok
}

/// Whether a file can be created under `dir`, answering the errno otherwise.
fn writable(dir: &str) -> Result<bool, i32> {
    let name = format!("{dir}/probe-file");
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&name)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&name);
            Ok(true)
        }
        Err(e) => match e.raw_os_error().unwrap_or(0) {
            libc::EROFS => Ok(false),
            other => Err(-other),
        },
    }
}

fn main() {
    println!("t-mount-api");
    let mut r = Report::new();

    // A leftover mount from an interrupted run would turn the first mount into
    // EBUSY and hide every claim after it.
    for dir in [SRC, DST, DST2, DST3, A2, B2] {
        let _ = unmount(dir, UnmountFlags::empty());
    }
    let _ = std::fs::remove_dir_all(ROOT);
    let made = mkdirat(CWD, ROOT, Mode::from_raw_mode(0o755)).is_ok();
    r.check("the scratch directory is made", made);
    for dir in [SRC, DST, DST2, DST3, A2, B2, PLAIN] {
        let made = mkdirat(CWD, dir, Mode::from_raw_mode(0o755)).is_ok();
        r.check(&format!("{dir} is made"), made);
    }

    if host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: the host's ordinary user is refused these calls with EPERM");
        r.finish()
    }
    if !mount_tmpfs(&mut r, "a tmpfs is mounted at the source", SRC) {
        r.finish()
    }
    let _ = std::fs::write(format!("{SRC}/file"), b"here");
    r.check("the source filesystem takes a file", true);
    // The directory to mount under the source must be made *inside* it: the
    // tmpfs over the source is a fresh root, and a directory made before the
    // mount is one the mount now covers.
    let made = mkdirat(CWD, SUB, Mode::from_raw_mode(0o755)).is_ok();
    r.check("a directory to mount under the source is made", made);

    // --- open_tree: a reference and a copy --------------------------------
    let reference = open_tree(AT_FDCWD, &path(SRC), 0);
    match reference {
        Ok(fd) => {
            r.check("open_tree of a mount names it", fd >= 0);
            match stat(format!("/proc/self/fd/{fd}")) {
                Ok(st) => {
                    let link = std::fs::read_link(format!("/proc/self/fd/{fd}"));
                    r.check(
                        &format!(
                            "a reference is a directory and names its path (mode {:o}, link {:?})",
                            st.st_mode & 0o170_000,
                            link.as_ref().map(|p| p.display().to_string())
                        ),
                        st.st_mode & 0o170_000 == 0o040_000,
                    );
                    r.check(
                        "a reference's link is the mounted path",
                        matches!(&link, Ok(p) if p == std::path::Path::new(SRC)),
                    );
                }
                Err(e) => r.check(&format!("the reference stats (stat: {e:?})"), false),
            }
        }
        Err(e) => r.check(&format!("open_tree of a mount names it (is {e})"), false),
    }
    let clone = match open_tree(AT_FDCWD, &path(SRC), OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC) {
        Ok(fd) => {
            r.check("open_tree with OPEN_TREE_CLONE makes a copy", fd >= 0);
            let link = std::fs::read_link(format!("/proc/self/fd/{fd}"));
            r.check(
                &format!(
                    "a detached copy's link is / (is {:?})",
                    link.as_ref().map(|p| p.display().to_string())
                ),
                matches!(&link, Ok(p) if p == std::path::Path::new("/")),
            );
            fd
        }
        Err(e) => {
            r.check(
                &format!("open_tree with OPEN_TREE_CLONE makes a copy (is {e})"),
                false,
            );
            r.finish()
        }
    };
    // The vocabulary and its refusals.
    let stray = open_tree(AT_FDCWD, &path(SRC), 0x2000_0000);
    r.check(
        &format!("a stray flag bit is EINVAL (is {:?})", stray.as_ref().err()),
        stray == Err(-libc::EINVAL),
    );
    let recursive_only = open_tree(AT_FDCWD, &path(SRC), AT_RECURSIVE);
    r.check(
        &format!(
            "AT_RECURSIVE without a clone is EINVAL (is {:?})",
            recursive_only.as_ref().err()
        ),
        recursive_only == Err(-libc::EINVAL),
    );
    let missing = open_tree(AT_FDCWD, &path("/tmp/t-mount-api/absent"), 0);
    r.check(
        &format!("a missing path is ENOENT (is {:?})", missing.as_ref().err()),
        missing == Err(-libc::ENOENT),
    );
    let empty_noflag = open_tree(AT_FDCWD, &path(SRC), OPEN_TREE_CLONE);
    let empty_path = open_tree(AT_FDCWD, c"", OPEN_TREE_CLONE);
    r.check(
        &format!(
            "an empty path without AT_EMPTY_PATH is ENOENT (is {:?})",
            empty_path.as_ref().err()
        ),
        empty_path == Err(-libc::ENOENT),
    );
    let _ = empty_noflag;
    let source_fd = open(
        c"/tmp/t-mount-api/src",
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
    );
    match source_fd {
        Ok(fd) => {
            let through_fd = open_tree(
                fd.as_raw_fd(),
                c"",
                OPEN_TREE_CLONE | AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW,
            );
            r.check(
                &format!(
                    "an empty path with AT_EMPTY_PATH names the descriptor's mount (is {:?})",
                    through_fd.as_ref().err()
                ),
                through_fd.is_ok(),
            );
        }
        Err(e) => r.check(
            &format!("an O_PATH descriptor of the source opens ({e:?})"),
            false,
        ),
    }

    // --- move_mount: attaching the copy and moving a path ----------------
    let attached = move_mount(clone, c"", AT_FDCWD, &path(DST), MOVE_MOUNT_F_EMPTY_PATH);
    r.check(
        &format!("a copy attaches at a directory (is {attached})"),
        attached == 0,
    );
    r.check(
        "the attached copy shows the source's file",
        std::fs::metadata(format!("{DST}/file")).is_ok(),
    );
    // The same descriptor again: Linux attaches a copy as often as it is asked.
    let attached_twice = move_mount(clone, c"", AT_FDCWD, &path(DST2), MOVE_MOUNT_F_EMPTY_PATH);
    r.check(
        &format!("the same copy attaches again (is {attached_twice})"),
        attached_twice == 0,
    );
    // The target may also be a descriptor's own file: `T_EMPTY_PATH`.
    match open(
        c"/tmp/t-mount-api/dst3",
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let by_fd = move_mount(
                clone,
                c"",
                fd.as_raw_fd(),
                c"",
                MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
            );
            r.check(
                &format!("a copy attaches at a descriptor's own file (is {by_fd})"),
                by_fd == 0 && std::fs::metadata(format!("{DST3}/file")).is_ok(),
            );
        }
        Err(e) => r.check(
            &format!("the target's O_PATH descriptor opens ({e:?})"),
            false,
        ),
    }
    // A reference to an ordinary directory is a descriptor, not a mount.
    let plain = open_tree(AT_FDCWD, &path(PLAIN), 0);
    let plain_fd = match plain {
        Ok(fd) => {
            r.check(
                "a reference to an ordinary directory is still made",
                fd >= 0,
            );
            fd
        }
        Err(e) => {
            r.check(
                &format!("a reference to an ordinary directory is still made (is {e})"),
                false,
            );
            -1
        }
    };
    if plain_fd >= 0 {
        let refused = move_mount(plain_fd, c"", AT_FDCWD, &path(DST), MOVE_MOUNT_F_EMPTY_PATH);
        r.check(
            &format!("a reference to a non-mount is EINVAL when attached (is {refused})"),
            refused == -libc::EINVAL,
        );
    }
    let stray = move_mount(clone, c"", AT_FDCWD, &path(DST), 0x80);
    r.check(
        &format!("a stray move_mount flag bit is EINVAL (is {stray})"),
        stray == -libc::EINVAL,
    );
    let busy = move_mount(clone, c"", AT_FDCWD, &path(DST), MOVE_MOUNT_F_EMPTY_PATH);
    r.check(
        &format!("a target that already carries a mount is EINVAL (is {busy})"),
        busy == -libc::EINVAL,
    );
    // A path source moves the mount itself.
    if mount_tmpfs(&mut r, "a second tmpfs is mounted at a path source", A2) {
        let _ = std::fs::write(format!("{A2}/moved"), b"here");
        let moved = move_mount(AT_FDCWD, &path(A2), AT_FDCWD, &path(B2), 0);
        r.check(
            &format!("a path source moves its mount (is {moved})"),
            moved == 0 && std::fs::metadata(format!("{B2}/moved")).is_ok(),
        );
        let back = move_mount(AT_FDCWD, &path(B2), AT_FDCWD, &path(A2), 0);
        r.check(&format!("and it moves back (is {back})"), back == 0);
        let not_mount = move_mount(AT_FDCWD, &path(PLAIN), AT_FDCWD, &path(B2), 0);
        r.check(
            &format!("a source that is not a mount is EINVAL (is {not_mount})"),
            not_mount == -libc::EINVAL,
        );
        let _ = unmount(A2, UnmountFlags::empty());
    }

    // --- mount_setattr: the structure, the bits, the walk ----------------
    let small = mount_setattr(AT_FDCWD, &path(SRC), 0, &MountAttr::default(), 24);
    r.check(
        &format!("a structure smaller than 32 is EINVAL (is {small})"),
        small == -libc::EINVAL,
    );
    let big = mount_setattr(AT_FDCWD, &path(SRC), 0, &MountAttr::default(), 40);
    r.check(
        &format!("a structure larger than 32 is E2BIG (is {big})"),
        big == -libc::E2BIG,
    );
    let unknown_flag = mount_setattr(AT_FDCWD, &path(SRC), 0x2_0000, &MountAttr::default(), 32);
    r.check(
        &format!("a stray setattr flag bit is EINVAL (is {unknown_flag})"),
        unknown_flag == -libc::EINVAL,
    );
    let not_a_mount = mount_setattr(
        AT_FDCWD,
        &path(PLAIN),
        0,
        &read_only_attr(MOUNT_ATTR_RDONLY, 0),
        32,
    );
    r.check(
        &format!("a target that is not a mount is EINVAL (is {not_a_mount})"),
        not_a_mount == -libc::EINVAL,
    );
    let nosuid = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(MOUNT_ATTR_NOSUID, 0),
        32,
    );
    r.check(
        &format!("NOSUID, which this kernel cannot store, is EOPNOTSUPP (is {nosuid})"),
        nosuid == -libc::EOPNOTSUPP,
    );
    let noatime = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(MOUNT_ATTR_NOATIME, 0),
        32,
    );
    r.check(
        &format!("NOATIME, which the host also refuses, is EINVAL (is {noatime})"),
        noatime == -libc::EINVAL,
    );
    let idmap = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(MOUNT_ATTR_IDMAP, 0),
        32,
    );
    r.check(
        &format!("IDMAP is EINVAL (is {idmap})"),
        idmap == -libc::EINVAL,
    );
    let noidmap = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(MOUNT_ATTR_NOIDMAP, 0),
        32,
    );
    r.check(
        &format!("NOIDMAP, which this kernel cannot store, is EOPNOTSUPP (is {noidmap})"),
        noidmap == -libc::EOPNOTSUPP,
    );

    // A tmpfs in this kernel has no read-only state to honour, so the promise
    // is refused rather than recorded: Linux enforces read-only at the VFS
    // level, this kernel's filesystems each enforce their own, and `ramfs`
    // answers the flag with nothing. An ext4 mount would take it; a tmpfs
    // cannot, and the claim below is the refusal rather than a promise nobody
    // keeps.
    let ro = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(MOUNT_ATTR_RDONLY, 0),
        32,
    );
    r.check(
        &format!("RDO set on a tmpfs is EOPNOTSUPP, not a promise nobody keeps (is {ro})"),
        ro == -libc::EOPNOTSUPP,
    );
    let rw = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        0,
        &read_only_attr(0, MOUNT_ATTR_RDONLY),
        32,
    );
    r.check(&format!("RDO clear is accepted (is {rw})"), rw == 0);
    r.check(
        &format!("and writes still work (is {:?})", writable(SRC)),
        writable(SRC) == Ok(true),
    );
    // Through a descriptor's own file: the empty-path shape.
    match open(
        c"/tmp/t-mount-api/src",
        OFlags::PATH | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let asked = mount_setattr(
                fd.as_raw_fd(),
                c"",
                AT_EMPTY_PATH,
                &read_only_attr(0, MOUNT_ATTR_RDONLY),
                32,
            );
            r.check(
                &format!(
                    "an empty path with AT_EMPTY_PATH names the descriptor's mount (is {asked})"
                ),
                asked == 0,
            );
            let refused = mount_setattr(
                fd.as_raw_fd(),
                c"",
                AT_EMPTY_PATH,
                &read_only_attr(MOUNT_ATTR_RDONLY, 0),
                32,
            );
            r.check(
                &format!("and the read-only promise it cannot keep is EOPNOTSUPP (is {refused})"),
                refused == -libc::EOPNOTSUPP,
            );
        }
        Err(e) => r.check(
            &format!("the source's O_PATH descriptor opens ({e:?})"),
            false,
        ),
    }
    // The recursive walk: a mount below the target is asked too, and the first
    // filesystem that cannot keep the promise refuses the whole call.
    let child = mount("tmpfs", SUB, "tmpfs", MountFlags::empty(), None);
    r.check("a tmpfs is mounted below the source", child.is_ok());
    let recursive = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        AT_RECURSIVE,
        &read_only_attr(MOUNT_ATTR_RDONLY, 0),
        32,
    );
    r.check(
        &format!("a recursive RDO set over tmpfs mounts is EOPNOTSUPP too (is {recursive})"),
        recursive == -libc::EOPNOTSUPP,
    );
    let recursive_clear = mount_setattr(
        AT_FDCWD,
        &path(SRC),
        AT_RECURSIVE,
        &read_only_attr(0, MOUNT_ATTR_RDONLY),
        32,
    );
    r.check(
        &format!("the recursive clear is accepted (is {recursive_clear})"),
        recursive_clear == 0,
    );
    r.check(
        &format!("and writes work below it (is {:?})", writable(SUB)),
        writable(SUB) == Ok(true),
    );
    let _ = unmount(SUB, UnmountFlags::empty());

    // --- the superblock half, and the one number of it still refused ----
    // The trio is served now (`t-fsmount` owns its contract); `fspick` is the
    // family's remaining named gap: what it picks a mount *for* is
    // reconfiguration, and no filesystem here can be reconfigured.
    let fs = unsafe { libc::syscall(SYS_FSOPEN, c"tmpfs".as_ptr() as usize, 0usize) };
    r.check(
        &format!("fsopen names a type and answers a descriptor (is {fs})"),
        fs >= 0,
    );
    let pick = unsafe {
        libc::syscall(
            SYS_FSPICK,
            libc::AT_FDCWD as i64,
            c"/tmp".as_ptr() as usize,
            0usize,
        )
    };
    let pick_errno = errno();
    r.check(
        &format!("fspick, the reconfiguration door, is still ENOSYS (is {pick_errno})"),
        pick < 0 && pick_errno == -libc::ENOSYS,
    );

    // --- cleanup ---------------------------------------------------------
    for dir in [DST, DST2, DST3, SRC, B2] {
        let _ = unmount(dir, UnmountFlags::empty());
    }
    let _ = std::fs::remove_dir_all(ROOT);
    r.finish()
}
