//! t-fsmount - `fsopen(2)`, `fsconfig(2)`, `fsmount(2)`: a filesystem built
//! before it has a place to stand.
//!
//! Measured on Linux 7.2.11 first (`build/scratch/mntapi/p{1,2,3,4,5}.c`, run
//! through `sudo -n unshare -m --propagation private ./pN` because the calls
//! want mount-namespace authority). The facts the claims encode:
//!
//! - `fsopen` names a type and answers a descriptor whose `read(2)` is
//!   `ENODATA` (61 - nothing to read back), `write(2)` is `EINVAL` (22), and
//!   whose `fstat` is a 0-byte regular file; `FSOPEN_CLOEXEC` is the only bit
//!   its flag word carries, and an unknown type is `ENODEV` (19);
//! - `fsconfig` speaks one option: an unknown key is `EINVAL`, a command
//!   number past Linux's eight is `EOPNOTSUPP` (95), `FSCONFIG_CMD_CREATE`
//!   happens once (`EBUSY`, 16, afterwards), and every option after the
//!   creation is `EBUSY` too;
//! - `fsmount` before the creation is `EINVAL`, its own flag word carries
//!   `FSMOUNT_CLOEXEC`/`FSMOUNT_NAMESPACE` and nothing else (a stray bit is
//!   `EINVAL`), the *mount attributes* are the third argument
//!   (`MOUNT_ATTR_IDMAP` is `EINVAL` without idmapped mounts), and a second
//!   `fsmount` from one context is `EBUSY`;
//! - the descriptor `fsmount` answers is a mount, not a file: `read(2)` is
//!   `EBADF`, `fstat` reports the root directory, and `fsconfig` on it is
//!   `EBADF`;
//! - `move_mount` of that descriptor with `MOVE_MOUNT_F_EMPTY_PATH` attaches it,
//!   and the name it is attached at then answers the new filesystem's device.
//!
//! This kernel serves the trio for the types it can build; `fspick(2)` stays a
//! named gap (what it picks a mount *for* is reconfiguration, which no
//! filesystem here can do). One divergence is deliberate and named in
//! `docs/notes/` §3.16: an option this kernel does not read is refused rather
//! than recorded and ignored, so a tmpfs `size=` is `EINVAL` here where Linux
//! accepts it (`mount(2)`'s data argument ignores the same option silently).

use std::ffi::CString;

use mpos_user_tests::{Report, host_run};
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, mkdirat, open};

const SYS_MOVE_MOUNT: libc::c_long = 429;
const SYS_FSOPEN: libc::c_long = 430;
const SYS_FSCONFIG: libc::c_long = 431;
const SYS_FSMOUNT: libc::c_long = 432;

const FSOPEN_CLOEXEC: u32 = 0x1;
const FSMOUNT_CLOEXEC: u32 = 0x1;
const MOUNT_ATTR_RDONLY: u32 = 0x1;
const MOUNT_ATTR_IDMAP: u32 = 0x0010_0000;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;
const MOVE_MOUNT_F_EMPTY_PATH: usize = 0x4;
const MNT_DETACH: libc::c_int = 2;

const ROOT: &str = "/tmp/t-fsmount";
const MNT: &str = "/tmp/t-fsmount/m";

fn errno() -> i32 {
    -std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `fsopen(2)`: a descriptor, or the negative errno.
fn fsopen(fstype: &str, flags: u32) -> i32 {
    let name = CString::new(fstype).unwrap();
    let rc = unsafe { libc::syscall(SYS_FSOPEN, name.as_ptr() as usize, flags as libc::c_long) };
    if rc < 0 { errno() } else { rc as i32 }
}

/// `fsconfig(2)`: 0, or the negative errno. A `None` key or value is the null
/// pointer Linux distinguishes.
fn fsconfig(fd: i32, command: u32, key: Option<&str>, value: Option<&str>) -> i32 {
    let key = key.map(|k| CString::new(k).unwrap());
    let value = value.map(|v| CString::new(v).unwrap());
    let rc = unsafe {
        libc::syscall(
            SYS_FSCONFIG,
            fd as libc::c_long,
            command as libc::c_long,
            key.as_ref().map_or(0, |k| k.as_ptr() as usize),
            value.as_ref().map_or(0, |v| v.as_ptr() as usize),
            0usize,
        )
    };
    if rc < 0 { errno() } else { rc as i32 }
}

/// `fsmount(2)`: a mount descriptor, or the negative errno.
fn fsmount(fd: i32, flags: u32, attributes: u32) -> i32 {
    let rc = unsafe {
        libc::syscall(
            SYS_FSMOUNT,
            fd as libc::c_long,
            flags as libc::c_long,
            attributes as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { rc as i32 }
}

/// `move_mount(2)` of a descriptor into a path.
fn move_mount(from_fd: i32, path: &str) -> i32 {
    let to = CString::new(path).unwrap();
    let rc = unsafe {
        libc::syscall(
            SYS_MOVE_MOUNT,
            from_fd as libc::c_long,
            b"\0".as_ptr() as usize,
            libc::AT_FDCWD as libc::c_long,
            to.as_ptr() as usize,
            MOVE_MOUNT_F_EMPTY_PATH as libc::c_long,
        )
    };
    if rc < 0 { errno() } else { rc as i32 }
}

/// A fresh context that has already created its filesystem.
fn created_context(fstype: &str) -> Option<i32> {
    let fd = fsopen(fstype, 0);
    if fd < 0 {
        return None;
    }
    let created = fsconfig(fd, FSCONFIG_CMD_CREATE, None, None);
    if created != 0 {
        return None;
    }
    Some(fd)
}

fn main() {
    println!("t-fsmount");
    let mut r = Report::new();

    let _ = unsafe { libc::umount2(CString::new(MNT).unwrap().as_ptr(), MNT_DETACH) };
    let _ = std::fs::remove_dir_all(ROOT);
    let made = mkdirat(CWD, ROOT, Mode::from_raw_mode(0o755)).is_ok();
    r.check("the scratch directory is made", made);
    let made = mkdirat(CWD, MNT, Mode::from_raw_mode(0o755)).is_ok();
    r.check("a directory to attach a filesystem to is made", made);

    if host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: the host's ordinary user is refused these calls with EPERM");
        r.finish()
    }

    // ---- fsopen: the descriptor and its shape
    let fd = fsopen("tmpfs", 0);
    r.check("fsopen names a type and answers a descriptor", fd >= 0);
    let Some(fd) = (fd >= 0).then_some(fd) else {
        r.finish()
    };
    let unknown = fsopen("nosuchfilesystem", 0);
    r.check(
        &format!("an unknown type is ENODEV (is {unknown})"),
        unknown == -libc::ENODEV,
    );
    let stray = fsopen("tmpfs", 0x8000_0000);
    r.check(
        &format!("a stray flag bit is EINVAL (is {stray})"),
        stray == -libc::EINVAL,
    );

    // `FSOPEN_CLOEXEC` is the one bit `fsopen`'s word carries; the descriptor
    // it makes is close-on-exec when asked.
    let cloexec = fsopen("tmpfs", FSOPEN_CLOEXEC);
    let flags_of = if cloexec >= 0 {
        unsafe { libc::fcntl(cloexec, libc::F_GETFD) }
    } else {
        -1
    };
    r.check(
        &format!("FSOPEN_CLOEXEC sets FD_CLOEXEC (is {flags_of})"),
        flags_of >= 0 && flags_of & libc::FD_CLOEXEC != 0,
    );

    let mut byte = [0u8; 1];
    let rc = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    let read_errno = if rc < 0 { errno() } else { 0 };
    r.check(
        &format!("read(2) on a context is ENODATA (is {read_errno})"),
        read_errno == -libc::ENODATA,
    );
    let rc = unsafe { libc::write(fd, byte.as_ptr() as *const libc::c_void, 1) };
    let write_errno = if rc < 0 { errno() } else { 0 };
    r.check(
        &format!("write(2) on a context is EINVAL (is {write_errno})"),
        write_errno == -libc::EINVAL,
    );

    // ---- fsconfig: the option door and the one-way creation
    let unknown_key = fsconfig(fd, FSCONFIG_SET_STRING, Some("nosuchkey"), Some("1"));
    r.check(
        &format!("an option this type does not read is EINVAL (is {unknown_key})"),
        unknown_key == -libc::EINVAL,
    );
    let unknown_command = fsconfig(fd, 9, None, None);
    r.check(
        &format!("a command number past Linux's eight is EOPNOTSUPP (is {unknown_command})"),
        unknown_command == -libc::EOPNOTSUPP,
    );
    // Deliberate divergence, named above: Linux's tmpfs reads `size=`, this
    // kernel's does not, so the option is refused rather than ignored.
    let size = fsconfig(fd, FSCONFIG_SET_STRING, Some("size"), Some("1M"));
    r.check(
        &format!("an option no filesystem here reads is refused (is {size})"),
        size == -libc::EINVAL,
    );
    let early = fsmount(fd, 0, 0);
    r.check(
        &format!("fsmount before the creation is EINVAL (is {early})"),
        early == -libc::EINVAL,
    );
    let made_fs = fsconfig(fd, FSCONFIG_CMD_CREATE, None, None);
    r.check("FSCONFIG_CMD_CREATE builds the filesystem", made_fs == 0);
    let twice = fsconfig(fd, FSCONFIG_CMD_CREATE, None, None);
    r.check(
        &format!("a second creation is EBUSY (is {twice})"),
        twice == -libc::EBUSY,
    );
    let late = fsconfig(fd, FSCONFIG_SET_STRING, Some("source"), Some("none"));
    r.check(
        &format!("an option after the creation is EBUSY (is {late})"),
        late == -libc::EBUSY,
    );

    // ---- fsmount: the descriptor, its refusals, and its shape
    let stray_flag = fsmount(fd, 0x4, 0);
    r.check(
        &format!("a stray fsmount flag is EINVAL (is {stray_flag})"),
        stray_flag == -libc::EINVAL,
    );
    let idmap = match created_context("tmpfs") {
        Some(context) => fsmount(context, 0, MOUNT_ATTR_IDMAP),
        None => -1,
    };
    r.check(
        &format!("MOUNT_ATTR_IDMAP is EINVAL (no idmapped mounts, is {idmap})"),
        idmap == -libc::EINVAL,
    );
    let read_only = fsmount(fd, 0, MOUNT_ATTR_RDONLY);
    r.check(
        &format!("fsmount answers a mount descriptor (is {read_only})"),
        read_only >= 0,
    );
    let Some(read_only) = (read_only >= 0).then_some(read_only) else {
        r.finish()
    };
    // The read-only descriptor is the shape check above; the mount this test
    // attaches is a fresh read-write one, because a read-only tmpfs is a
    // *refused* mount in this kernel (item 122: a filesystem that cannot honour
    // read-only is `EOPNOTSUPP`) and the claims below are about the attach.
    let Some(context) = created_context("tmpfs") else {
        r.check("a read-write context is created", false);
        r.finish()
    };
    let mount = fsmount(context, FSMOUNT_CLOEXEC, 0);
    r.check("fsmount answers a read-write mount descriptor", mount >= 0);
    let Some(mount) = (mount >= 0).then_some(mount) else {
        r.finish()
    };
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let rc = unsafe { libc::fstat(read_only, &mut stat) };
    r.check(
        "the mount descriptor reports the filesystem's root directory",
        rc == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFDIR,
    );
    let rc = unsafe { libc::read(read_only, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    let mount_read = if rc < 0 { errno() } else { 0 };
    r.check(
        &format!("read(2) on a mount descriptor is EBADF (is {mount_read})"),
        mount_read == -libc::EBADF,
    );
    let on_mount = fsconfig(read_only, FSCONFIG_CMD_CREATE, None, None);
    r.check(
        &format!("fsconfig on a mount descriptor is EBADF (is {on_mount})"),
        on_mount == -libc::EBADF,
    );
    let second = fsmount(context, 0, 0);
    r.check(
        &format!("a second fsmount from one context is EBUSY (is {second})"),
        second == -libc::EBUSY,
    );

    // ---- move_mount: the attach, and the name that carries the filesystem
    let attached = move_mount(mount, MNT);
    r.check(
        &format!("move_mount attaches the descriptor (is {attached})"),
        attached == 0,
    );
    let parent_dev = std::fs::metadata(ROOT).map(|m| dev_of(&m)).unwrap_or(0);
    let mount_dev = std::fs::metadata(MNT).map(|m| dev_of(&m)).unwrap_or(0);
    r.check(
        "the name answers the new filesystem's own device",
        parent_dev != 0 && mount_dev != 0 && mount_dev != parent_dev,
    );
    // A *file inside* the attached filesystem: the mount point itself is a
    // directory, so `O_CREAT` on it is `EISDIR` (measured the hard way).
    let inside = format!("{MNT}/f");
    let file = open(
        inside.as_str(),
        OFlags::CREATE | OFlags::WRONLY,
        Mode::from_raw_mode(0o644),
    );
    match &file {
        Ok(file) => {
            let rc =
                unsafe { libc::write(file.as_raw_fd(), b"hi".as_ptr() as *const libc::c_void, 2) };
            let write_errno = if rc < 0 { errno() } else { 0 };
            r.check(
                &format!(
                    "a file can be written through the attached name (rc {rc}, errno {write_errno})"
                ),
                rc == 2,
            );
        }
        Err(e) => r.check(
            &format!("a file can be written through the attached name (open failed: {e})"),
            false,
        ),
    }
    let _ = unsafe { libc::umount2(CString::new(MNT).unwrap().as_ptr(), MNT_DETACH) };
    r.check(
        "the mount detaches again",
        std::fs::metadata(MNT).map(|m| dev_of(&m)).unwrap_or(0) == parent_dev,
    );

    r.finish()
}

fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}
