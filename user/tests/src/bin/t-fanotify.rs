//! `fanotify_init(2)` and `fanotify_mark(2)` — the notification class.
//!
//! The measured table this program pins was taken on a real-root Linux 7.2.11
//! (`build/scratch/fanokey/fanotify_probe.c`, booted as PID 1), because the
//! call asks `CAP_SYS_ADMIN` in the *initial* user namespace: an ordinary user
//! is refused `EPERM` there before a flag word is looked at, and so is uid 0
//! inside a user namespace. The gate's programs run as root in the payload, so
//! the served shape is reachable here; a host run as an unprivileged user
//! currently measures only the `EPERM` half, and says so.
//!
//! What the measurements fixed:
//!
//! - `fanotify_init(0, 0)` answers a descriptor whose `F_GETFL` is `O_RDWR`
//!   (0x2), unlike an inotify queue's `O_RDONLY`.
//! - `FAN_CLOEXEC` and `FAN_NONBLOCK` are the two descriptor words; the
//!   `event_f_flags` word accepts the open-status bits (`O_RDWR`, `O_APPEND`,
//!   `O_NOATIME`, `O_CLOEXEC`, `O_NONBLOCK`) and this kernel does not use them
//!   (a notification record has no event description to configure).
//! - Every `FAN_REPORT_*` bit, the content classes and `FAN_ENABLE_AUDIT` are
//!   served by Linux's *notification* group only for some of them: the report
//!   shapes and the content classes are not served here and are refused
//!   `EOPNOTSUPP` by name — Linux answers `EINVAL` for `FAN_REPORT_NAME` and
//!   accepts the content classes, which is the named divergence this program
//!   records rather than hides.
//! - A mark's object is the `dirfd` descriptor when the name is NULL (`EBADF`
//!   for a bad one), otherwise the name resolved from `dirfd`.
//! - A mask of 0, an unknown bit, `FAN_MARK_ADD|FAN_MARK_REMOVE` together, and
//!   a permission-event mask on a notification group are each `EINVAL`.
//! - The mount and filesystem scopes, and the two `IGNORED_*` shapes, are
//!   `EOPNOTSUPP` here (Linux serves them; this tree has no inode-to-mount
//!   ancestry to answer them against, so refusal is named).
//! - A record is 24 bytes: `event_len`, `vers` 3, `metadata_len`, `mask`, an
//!   event `fd`, and the causing `pid` — the child's pid when a child opened
//!   the object, measured.
//! - A short buffer (`< 24`) and `read(NULL, 0)` are `EINVAL`, and the event
//!   **survives** a short read — it is the reader's mistake, not the queue's
//!   loss.
//! - Removing a mark stops its events.

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use mpos_user_tests::{Report, host_run};

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_CLASS_CONTENT: u32 = 0x0000_0004;
const FAN_REPORT_NAME: u32 = 0x0000_0800;

const FAN_ACCESS: u64 = 0x0000_0001;
const FAN_MODIFY: u64 = 0x0000_0002;
const FAN_ATTRIB: u64 = 0x0000_0004;
const FAN_CLOSE_WRITE: u64 = 0x0000_0008;
const FAN_CLOSE_NOWRITE: u64 = 0x0000_0010;
const FAN_OPEN: u64 = 0x0000_0020;
const FAN_OPEN_PERM: u64 = 0x0000_0000_0001_0000;

const FAN_MARK_ADD: u32 = 0x0000_0001;
const FAN_MARK_REMOVE: u32 = 0x0000_0002;
const FAN_MARK_MOUNT: u32 = 0x0000_0010;
const FAN_MARK_FLUSH: u32 = 0x0000_0080;
const FAN_MARK_FILESYSTEM: u32 = 0x0000_0100;

const AT_FDCWD: i32 = -100;
const EVENT_LEN: usize = 24;
const VERSION: u8 = 3;
const EPERM: i32 = -1;
const EBADF: i32 = -9;
const ENOENT: i32 = -2;
const EAGAIN: i32 = -11;
const EINVAL: i32 = -22;
const EOPNOTSUPP: i32 = -95;

fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(0)
        .saturating_neg()
}

fn fanotify_init(flags: u32, event_f_flags: u32) -> Result<OwnedFd, i32> {
    let r = unsafe {
        libc::syscall(
            libc::SYS_fanotify_init,
            flags as libc::c_long,
            event_f_flags as libc::c_long,
        ) as libc::c_int
    };
    if r < 0 {
        Err(errno())
    } else {
        // SAFETY: the call just handed this process a descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(r) })
    }
}

fn mark(fd: i32, flags: u32, mask: u64, dirfd: i32, path: Option<&CStr>) -> i32 {
    let path = path.map_or(std::ptr::null(), |p| p.as_ptr());
    let r = unsafe {
        libc::syscall(
            libc::SYS_fanotify_mark,
            fd as libc::c_long,
            flags as libc::c_long,
            mask as libc::c_long,
            dirfd as libc::c_long,
            path,
        ) as libc::c_int
    };
    if r < 0 { errno() } else { r }
}

fn readable(fd: i32, millis: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut p, 1, millis) };
    r > 0 && p.revents & libc::POLLIN != 0
}

fn read_record(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if r < 0 { Err(errno()) } else { Ok(r as usize) }
}

/// Every record available now, oldest first. A read may answer several
/// records packed into one buffer, so the caller walks 24-byte steps.
fn collect(fd: i32) -> Vec<Record> {
    let mut buf = vec![0u8; 8 * EVENT_LEN];
    let mut out = Vec::new();
    loop {
        match read_record(fd, &mut buf) {
            Ok(n) => {
                let mut at = 0;
                while at + EVENT_LEN <= n {
                    out.push(parse(&buf[at..at + EVENT_LEN]));
                    at += EVENT_LEN;
                }
            }
            Err(_) => return out,
        }
    }
}

fn drain(fd: i32) {
    let mut buf = vec![0u8; 8 * EVENT_LEN];
    loop {
        match read_record(fd, &mut buf) {
            Ok(_) => {}
            Err(e) if e == EAGAIN => return,
            Err(_) => return,
        }
    }
}

/// The record's fields, little-endian as the wire spells them.
struct Record {
    event_len: u32,
    vers: u8,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
}

fn parse(buf: &[u8]) -> Record {
    let u32_at = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_ne_bytes(buf[at..at + 8].try_into().unwrap());
    let i32_at = |at: usize| i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
    Record {
        event_len: u32_at(0),
        vers: buf[4],
        metadata_len: u16::from_ne_bytes(buf[6..8].try_into().unwrap()),
        mask: u64_at(8),
        fd: i32_at(16),
        pid: i32_at(20),
    }
}

/// Open a file through a forked child, so the event's `pid` is a real other
/// task's, and wait for it - the measured shape of the child-pid claim.
fn child_opens(path: &Path) -> i32 {
    let path = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    pid
}

fn open_read(path: &Path) -> std::fs::File {
    std::fs::File::open(path).expect("open for read")
}

/// The scene: one directory with one file per claim family, so marks on one
/// object cannot colour another's answers.
struct Scene {
    dir: PathBuf,
    open_file: PathBuf,
    news_file: PathBuf,
    close_file: PathBuf,
    attrib_file: PathBuf,
    exec_file: PathBuf,
}

impl Scene {
    fn make() -> Scene {
        let dir = PathBuf::from("/tmp/t-fanotify");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scene = Scene {
            open_file: dir.join("open"),
            news_file: dir.join("news"),
            close_file: dir.join("close"),
            attrib_file: dir.join("attrib"),
            exec_file: dir.join("exec"),
            dir,
        };
        for p in [
            &scene.open_file,
            &scene.news_file,
            &scene.close_file,
            &scene.attrib_file,
            &scene.exec_file,
        ] {
            std::fs::write(p, b"x").unwrap();
        }
        scene
    }

    fn path(&self, p: &Path) -> CString {
        CString::new(p.to_string_lossy().as_bytes()).unwrap()
    }

    fn mark_add(&self, fd: i32, mask: u64, p: &Path) -> i32 {
        let path = self.path(p);
        mark(fd, FAN_MARK_ADD, mask, AT_FDCWD, Some(&path))
    }
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        // The host run is the measured creditor for the privilege half: an
        // ordinary user is refused EPERM before any flag word is read.
        let refused = fanotify_init(0, 0).err();
        r.check(
            "a plain user's fanotify_init is EPERM (measured)",
            refused == Some(EPERM),
        );
        r.finish();
    }

    let scene = Scene::make();

    // ---- init -----------------------------------------------------------------
    let plain = fanotify_init(FAN_NONBLOCK, 0).expect("init(0)");
    let fd = &plain;
    r.check("init(0) answers a descriptor", plain.as_raw_fd() >= 0);
    let status = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    r.check(
        "the group descriptor's F_GETFL is O_RDWR (0x2), as measured",
        status & libc::O_ACCMODE == libc::O_RDWR,
    );
    r.check(
        "an empty queue on a nonblocking group is EAGAIN",
        read_record(fd.as_raw_fd(), &mut [0u8; EVENT_LEN]) == Err(EAGAIN),
    );

    let cloexec = fanotify_init(FAN_NONBLOCK | FAN_CLOEXEC, 0).expect("init(CLOEXEC)");
    let fds = unsafe { libc::fcntl(cloexec.as_raw_fd(), libc::F_GETFD) };
    r.check(
        "FAN_CLOEXEC sets FD_CLOEXEC on the group descriptor",
        fds & libc::FD_CLOEXEC != 0,
    );

    let nonblock = fanotify_init(FAN_NONBLOCK, 0).expect("init(NONBLOCK)");
    let st = unsafe { libc::fcntl(nonblock.as_raw_fd(), libc::F_GETFL) };
    r.check(
        "FAN_NONBLOCK sets O_NONBLOCK in the status word",
        st & libc::O_NONBLOCK != 0,
    );

    r.check(
        "the event_f_flags word takes the open-status bits",
        fanotify_init(FAN_NONBLOCK, libc::O_RDWR as u32 | libc::O_CLOEXEC as u32).is_ok(),
    );
    r.check(
        "a word outside the uapi's flags is EINVAL",
        fanotify_init(0x2000, 0).err() == Some(EINVAL),
    );
    // Divergence, named: Linux accepts the content classes and answers EINVAL
    // for FAN_REPORT_NAME; this kernel serves the notification class alone and
    // names both refusals EOPNOTSUPP.
    r.check(
        "FAN_REPORT_NAME is EOPNOTSUPP here (Linux: EINVAL)",
        fanotify_init(FAN_REPORT_NAME, 0).err() == Some(EOPNOTSUPP),
    );
    r.check(
        "the content class is EOPNOTSUPP here (Linux serves it)",
        fanotify_init(FAN_CLASS_CONTENT, 0).err() == Some(EOPNOTSUPP),
    );

    // ---- mark validation ------------------------------------------------------
    let g = plain.as_raw_fd();
    let open_path = scene.path(&scene.open_file);
    r.check(
        "a mark is added by path",
        mark(g, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, Some(&open_path)) == 0,
    );
    r.check(
        "adding the same mark again is 0",
        mark(g, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, Some(&open_path)) == 0,
    );
    r.check(
        "a mask of 0 is EINVAL",
        mark(g, FAN_MARK_ADD, 0, AT_FDCWD, Some(&open_path)) == EINVAL,
    );
    r.check(
        "an unknown mask bit is EINVAL",
        mark(g, FAN_MARK_ADD, 0x8000_0000, AT_FDCWD, Some(&open_path)) == EINVAL,
    );
    r.check(
        "ADD and REMOVE together are EINVAL",
        mark(
            g,
            FAN_MARK_ADD | FAN_MARK_REMOVE,
            FAN_OPEN,
            AT_FDCWD,
            Some(&open_path),
        ) == EINVAL,
    );
    r.check(
        "a permission-event mask on a notification group is EINVAL",
        mark(g, FAN_MARK_ADD, FAN_OPEN_PERM, AT_FDCWD, Some(&open_path)) == EINVAL,
    );
    r.check(
        "a NULL name with a bad dirfd is EBADF",
        mark(g, FAN_MARK_ADD, FAN_OPEN, -1, None) == EBADF,
    );
    r.check(
        "a mark on a descriptor that is not a fanotify group is EBADF",
        {
            let f = open_read(&scene.news_file);
            let bad = f.into_raw_fd();
            let ret = mark(bad, FAN_MARK_ADD, FAN_OPEN, AT_FDCWD, Some(&open_path));
            unsafe { libc::close(bad) };
            ret == EBADF
        },
    );
    r.check(
        "the mount scope is EOPNOTSUPP here (Linux serves it)",
        mark(
            g,
            FAN_MARK_ADD | FAN_MARK_MOUNT,
            FAN_OPEN,
            AT_FDCWD,
            Some(&open_path),
        ) == EOPNOTSUPP,
    );
    r.check(
        "the filesystem scope is EOPNOTSUPP here (Linux serves it)",
        mark(
            g,
            FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
            FAN_OPEN,
            AT_FDCWD,
            Some(&open_path),
        ) == EOPNOTSUPP,
    );
    r.check(
        "flushing a group's marks is 0",
        mark(g, FAN_MARK_FLUSH, 0, AT_FDCWD, None) == 0,
    );

    // ---- open events, records, short reads, removal ----------------------------
    let mut buf = [0u8; 8 * EVENT_LEN];
    let got = scene.mark_add(g, FAN_OPEN, &scene.open_file);
    r.check("the open mark is back after the flush", got == 0);
    drain(g);
    let child = child_opens(&scene.open_file);
    r.check(
        "the child's open makes the group readable",
        readable(g, 2000),
    );
    let n = read_record(g, &mut buf).expect("read a record");
    let rec = parse(&buf[..n]);
    r.check("one record is read", n == EVENT_LEN);
    r.check(
        "the record's event_len and metadata_len are 24",
        rec.event_len as usize == EVENT_LEN && rec.metadata_len as usize == EVENT_LEN,
    );
    r.check("the record's vers is 3", rec.vers == VERSION);
    r.check("the record's mask is FAN_OPEN", rec.mask == FAN_OPEN);
    r.check(
        "the record's pid is the child that opened it",
        rec.pid == child,
    );
    r.check(
        "the record carries an event descriptor (FAN_NOFD is -1)",
        rec.fd >= 0,
    );

    let child = child_opens(&scene.open_file);
    let _ = child;
    r.check("a second open queues a record", readable(g, 2000));
    r.check(
        "a short buffer is EINVAL",
        read_record(g, &mut buf[..4]) == Err(EINVAL),
    );
    r.check(
        "read(NULL, 0) is EINVAL",
        read_record(g, &mut []) == Err(EINVAL),
    );
    r.check("the event survives a short read", readable(g, 200));
    let n = read_record(g, &mut buf).expect("read after short");
    r.check(
        "the surviving record is the open",
        n == EVENT_LEN && parse(&buf[..n]).mask == FAN_OPEN,
    );

    drain(g);
    r.check(
        "removing the open mark is 0",
        mark(g, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, Some(&open_path)) == 0,
    );
    r.check(
        "removing a mark that is not there is ENOENT",
        mark(g, FAN_MARK_REMOVE, FAN_OPEN, AT_FDCWD, Some(&open_path)) == ENOENT,
    );
    let child = child_opens(&scene.open_file);
    let _ = child;
    r.check(
        "no event arrives after the mark is removed",
        !readable(g, 300),
    );

    // ---- modify, access, attrib, close ----------------------------------------
    let news = nonblock.as_raw_fd();
    let news_path = scene.path(&scene.news_file);
    r.check(
        "a MODIFY|CLOSE_WRITE mark is added",
        mark(
            news,
            FAN_MARK_ADD,
            FAN_MODIFY | FAN_CLOSE_WRITE,
            AT_FDCWD,
            Some(&news_path),
        ) == 0,
    );
    drain(news);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&scene.news_file)
            .expect("open for append");
        f.write_all(b"y").expect("write");
    }
    r.check("a write makes the group readable", readable(news, 2000));
    let masks: Vec<u64> = collect(news).into_iter().map(|rec| rec.mask).collect();
    r.check("the write reports FAN_MODIFY", masks.contains(&FAN_MODIFY));
    r.check(
        "the write's close reports FAN_CLOSE_WRITE",
        masks.contains(&FAN_CLOSE_WRITE),
    );

    let close = fanotify_init(FAN_NONBLOCK, 0).expect("init(close)");
    let close_path = scene.path(&scene.close_file);
    r.check(
        "a CLOSE_NOWRITE|ACCESS mark is added",
        mark(
            close.as_raw_fd(),
            FAN_MARK_ADD,
            FAN_CLOSE_NOWRITE | FAN_ACCESS,
            AT_FDCWD,
            Some(&close_path),
        ) == 0,
    );
    drain(close.as_raw_fd());
    {
        use std::io::Read;
        let mut f = open_read(&scene.close_file);
        let mut b = [0u8; 1];
        let _ = f.read(&mut b);
    }
    r.check(
        "a read-open makes the group readable",
        readable(close.as_raw_fd(), 2000),
    );
    let masks: Vec<u64> = collect(close.as_raw_fd())
        .into_iter()
        .map(|rec| rec.mask)
        .collect();
    r.check("the read is FAN_ACCESS", masks.contains(&FAN_ACCESS));
    r.check(
        "the read-open's close is FAN_CLOSE_NOWRITE",
        masks.contains(&FAN_CLOSE_NOWRITE),
    );

    let attrib = fanotify_init(FAN_NONBLOCK, 0).expect("init(attrib)");
    let attrib_path = scene.path(&scene.attrib_file);
    r.check(
        "an ATTRIB mark is added",
        mark(
            attrib.as_raw_fd(),
            FAN_MARK_ADD,
            FAN_ATTRIB,
            AT_FDCWD,
            Some(&attrib_path),
        ) == 0,
    );
    drain(attrib.as_raw_fd());
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scene.attrib_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
    }
    r.check(
        "a chmod makes the group readable",
        readable(attrib.as_raw_fd(), 2000),
    );
    let n = read_record(attrib.as_raw_fd(), &mut buf).expect("read attrib");
    r.check(
        "the chmod is FAN_ATTRIB",
        n == EVENT_LEN && parse(&buf[..n]).mask == FAN_ATTRIB,
    );

    // ---- exec -----------------------------------------------------------------
    let ex = fanotify_init(FAN_NONBLOCK, 0).expect("init(exec)");
    let ex_path = scene.path(&scene.exec_file);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&scene.exec_file, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x");
    }
    r.check(
        "an OPEN_EXEC mark is added",
        mark(
            ex.as_raw_fd(),
            FAN_MARK_ADD,
            0x0000_1000,
            AT_FDCWD,
            Some(&ex_path),
        ) == 0,
    );
    drain(ex.as_raw_fd());
    let _ = std::process::Command::new(&scene.exec_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    r.check(
        "an exec of the marked file makes the group readable",
        readable(ex.as_raw_fd(), 2000),
    );
    let n = read_record(ex.as_raw_fd(), &mut buf).expect("read exec");
    r.check(
        "the exec is FAN_OPEN_EXEC (0x1000)",
        n == EVENT_LEN && parse(&buf[..n]).mask == 0x0000_1000,
    );

    let _ = std::fs::remove_dir_all(&scene.dir);
    r.finish();
}
