//! t-fd-name - a descriptor's *second name* denotes the file behind it.
//!
//! `/proc/<pid>/fd/<N>` is a "magic link". Its `readlink` answers a report - the
//! path the descriptor was opened by, or `pipe:[<ino>]` for a channel with no
//! name - and every *path* operation on it is really an operation on the file
//! the descriptor holds. That is not a small corner of the ABI: it is how a
//! program names a file it has open but cannot reopen (a deleted or `O_TMPFILE`
//! one), how `> /dev/stdout` reaches wherever standard output goes, and how a
//! directory descriptor is walked into.
//!
//! Pinned here, all of it measured on a current host through `syscall(2)`
//! (`.workbuddy/probe_fdlink.c`, Linux 7.2.9 — through the raw call because
//! glibc's `stat` reaches for `statx` and its `open` rewrites, either of which
//! would measure the libc rather than the kernel):
//!
//!   * `stat` of the name reports the **file** — kind, size and inode are the
//!     ones the file's own path reports — while `lstat` reports the **link**.
//!     The pair is the whole point, and both halves are claimed: a kernel that
//!     followed the link in the no-follow walk would pass the first alone.
//!   * `statx` answers the same way, including under `AT_SYMLINK_NOFOLLOW`.
//!   * the size-changing call reaches the file: `truncate(name, 4)` moves the
//!     size the *descriptor* reads back, a directory is `EISDIR` and a pipe's
//!     two ends and a character device are `EINVAL` — the path spelling's own
//!     answers, not `ftruncate`'s (§5.1 item 22) — and a file whose *path* no
//!     longer resolves is still nameable, because the name is not the path.
//!   * the mutators reach the file too: `fchmodat`, `utimensat` and `fchownat`
//!     through the name change the mode and the clock the descriptor reads
//!     back, and `access` serves; `chdir` into a *directory's* name works.
//!   * the name works **inside** a path: `/proc/self/fd/<dirfd>/<name>` is the
//!     spelling glibc's `*at` fallbacks build, and it resolves to the child of
//!     the directory the descriptor holds.
//!   * `open` of the name is a *fresh* description of the file - its own
//!     position and its own flags - and **not** a `dup` of the descriptor:
//!     measured, `open(name, O_RDWR)` reads from offset 0 while the descriptor
//!     stands where it was, and `open(name, O_WRONLY)` on an `O_RDWR|O_APPEND`
//!     descriptor reports `F_GETFL = 0x8001`, the word the *call* asked for.
//!   * a name for a descriptor nothing is open on is `ENOENT` - not `EBADF`,
//!     which is the answer the bare number would earn.
//!   * `/dev/fd/N` names the same file, being devfs's link into the same place.
//!
//! # What is named rather than claimed
//!
//! Three things this test does *not* assert, each measured, each owned by
//! `docs/uapi/` §5.2 rather than by this file:
//!
//!   * **another process's descriptor.** Linux names it: measured, a child's
//!     `pipe:[…]` entry under `/proc/<child>/fd/N` answers `stat` with the pipe
//!     and `truncate` refuses `EINVAL` like any other. This kernel's link lends
//!     a descriptor only to the caller's own thread group, and the link of a
//!     foreign process falls back to splicing its *report* as a path — so a
//!     named file is reached by the path it reports, and a *channel* answers
//!     `ENOENT` where Linux hands over the pipe.
//!   * **`st_dev`.** Linux reports the *file's* device through the name;
//!     this kernel reports the device of the *name's* filesystem, which is the
//!     same limitation `fstat` and the path spelling have already named (§5.1
//!     item 5: an inode does not say which filesystem it belongs to). The
//!     claims below therefore compare kind, size and inode, and the two `st_dev`
//!     values are printed so the difference is visible rather than asserted.
//!   * **`..` after the name.** Linux jumps the walk onto the file, so
//!     `/proc/self/fd/<dir>/..` is the file's own parent (measured: the same
//!     inode as `/tmp/..`). Here the walk's chain answers the *name's* directory
//!     instead — `/proc/self/fd` — because the parent a jumped-to node has is
//!     not a fact this kernel's inodes carry.
//!   * **what `getcwd` reports afterwards.** `chdir` through the name is
//!     claimed — it is the walk's own half — and the path read back is claimed
//!     too: it canonicalises to `/tmp`, because Linux's `getcwd` walks the
//!     dentry chain and this kernel's cwd now stores the name *derived from the
//!     directory* rather than the argument `chdir` was handed (`adopt_cwd`). A
//!     later section pins the shape a symlinked `chdir` has, and the `..` Linux
//!     takes from the target's parent.
//!
//! # Why `rustix` beside one raw door
//!
//! Every call here is a `rustix` binding with the shape the ABI promises, with
//! one exception: `truncate(path, len)` by name. `rustix` names no path-taking
//! `truncate` at all in this version, so the path spelling is `libc::truncate` —
//! the same door `t-truncate` uses, for the same reason.
//!
//! # What the kernel's first run of this program found
//!
//! Two claims failed, and both were the kernel's: `truncate` by a *pipe's* name
//! answered `EOPNOTSUPP`, because the path spelling's kind test let a pipe
//! through (a pipe reports `NodeKind::File`, the vocabulary's only word for an
//! unnamed channel) and reached the node's default `truncate` — the very test
//! §5.1 item 22 added to `ftruncate` and not to this door, which the denotation
//! is what made reachable; and the `getcwd` claim above, which the measurement
//! turned into a named divergence (`docs/uapi/` §5.2) instead of an assertion —
//! and which is an assertion again now that the cwd stores the directory's own
//! name.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{
    Access, AtFlags, CWD, FileType, Mode, OFlags, Stat, StatxFlags, Timespec, Timestamps, accessat,
    chownat, fstat, mkdirat, openat, statat, statx, unlinkat, utimensat,
};
use rustix::io::{Errno, read, write};
use rustix::pipe::{PipeFlags, pipe_with};

/// The descriptor's own file, and the second fixture whose *path* is removed
/// while the descriptor stays open.
const FILE: &str = "/tmp/t-fd-name-file";
const GONE: &str = "/tmp/t-fd-name-gone";
/// A child of a mounted directory, for the name-inside-a-path spelling.
const CHILD: &str = "/tmp/t-fd-name-child";
/// A directory to reach through its descriptor, and a device.
const DIR: &str = "/tmp";
const NULL_DEV: &str = "/dev/null";
/// The mountpoint this test owns when the payload has no `/proc`.
const OWN_PROC: &str = "/t-fd-name-proc";

/// `S_IFMT`'s answer as `rustix`'s enum, for a `Stat` taken either way.
fn kind_of(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode as _)
}

/// The path spelling of `truncate`, through `libc` - see the header.
fn truncate_path(path: &str, len: i64) -> Result<(), Errno> {
    let c = std::ffi::CString::new(path).unwrap();
    if unsafe { libc::truncate(c.as_ptr(), len) } == 0 {
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

/// `fchmodat` with no flags, which reaches the file a name denotes.
fn chmodat_path(path: &str, mode: u32) -> Result<(), Errno> {
    rustix::fs::chmodat(CWD, path, Mode::from_bits_retain(mode), AtFlags::empty())
}

/// Where a descriptor's listing can be read: the payload's own `/proc` when it
/// has one, otherwise a procfs this test mounts and owns. The same shape
/// `t-fd-list` uses, and for the same reason - a payload with no `/proc` is a
/// harness gap rather than a kernel failure, and saying which one was used lets
/// a reader tell the two apart.
fn procfs_root(r: &mut Report) -> Option<&'static str> {
    if std::path::Path::new("/proc/self").exists() {
        println!("     the payload's own /proc");
        return Some("/proc");
    }
    let _ = std::fs::create_dir(OWN_PROC);
    if let Err(e) = rustix::mount::mount(
        "proc",
        OWN_PROC,
        "proc",
        rustix::mount::MountFlags::empty(),
        None::<&std::ffi::CStr>,
    ) {
        r.check(&format!("a procfs can be mounted (got {e})"), false);
        return None;
    }
    println!("     a procfs mounted by this test ({OWN_PROC}): the payload has none");
    Some(OWN_PROC)
}

/// The name of a descriptor: `{root}/self/fd/{raw}`.
fn name_of(root: &str, fd: &OwnedFd) -> String {
    format!("{root}/self/fd/{}", fd.as_raw_fd())
}

/// The cwd as a `String`, or `None` when `getcwd` itself fails.
fn cwd_string() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

fn main() {
    println!("t-fd-name");
    let mut r = Report::new();
    let start = std::env::current_dir().ok();

    for path in [FILE, GONE, CHILD] {
        let _ = unlinkat(CWD, path, AtFlags::empty());
    }
    let Some(root) = procfs_root(&mut r) else {
        r.finish()
    };

    // The fixture: eleven bytes, owned by whoever is running, so every mutator
    // below is one the caller is allowed to make on either machine.
    let _ = unlinkat(CWD, FILE, AtFlags::empty());
    let fd = openat(
        CWD,
        FILE,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from_bits_retain(0o644),
    );
    let fd = match fd {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("the fixture opens (got {e})"), false);
            r.finish()
        }
    };
    r.check(
        "eleven bytes are written",
        write(&fd, b"hello world") == Ok(11),
    );

    // The file's own answer, to compare the name's against.
    let by_path = match statat(CWD, FILE, AtFlags::empty()) {
        Ok(st) => st,
        Err(e) => {
            r.check(&format!("the fixture stats by path (got {e})"), false);
            r.finish()
        }
    };
    let name = name_of(root, &fd);

    // ---- stat names the file, lstat names the link -------------------------
    match statat(CWD, &name, AtFlags::empty()) {
        Ok(st) => {
            r.check("stat by the name succeeds", true);
            r.check(
                "and names the file, not the link",
                kind_of(&st) == FileType::RegularFile,
            );
            r.check("with the file's own size", st.st_size == by_path.st_size);
            r.check("and the file's own inode", st.st_ino == by_path.st_ino);
            println!(
                "     st_dev: {} by path, {} by the name (Linux reports the file's both times)",
                by_path.st_dev, st.st_dev
            );
        }
        Err(e) => {
            r.check(&format!("stat by the name succeeds (got {e})"), false);
            r.check("and names the file, not the link", false);
            r.check("with the file's own size", false);
            r.check("and the file's own inode", false);
        }
    }
    match statat(CWD, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            r.check(
                "lstat by the name names the link",
                kind_of(&st) == FileType::Symlink,
            );
        }
        Err(e) => r.check(&format!("lstat by the name answers (got {e})"), false),
    }

    // ---- statx agrees, both ways ------------------------------------------
    match statx(CWD, &name, AtFlags::empty(), StatxFlags::BASIC_STATS) {
        Ok(stx) => {
            r.check(
                "statx by the name names the file",
                stx.stx_mode as u32 & 0o170000 == 0o100000
                    && stx.stx_size == by_path.st_size as u64
                    && stx.stx_ino == by_path.st_ino as u64,
            );
        }
        Err(e) => r.check(&format!("statx by the name answers (got {e})"), false),
    }
    match statx(
        CWD,
        &name,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS,
    ) {
        Ok(stx) => {
            r.check(
                "statx with AT_SYMLINK_NOFOLLOW names the link",
                stx.stx_mode as u32 & 0o170000 == 0o120000,
            );
        }
        Err(e) => r.check(
            &format!("statx by the name without following answers (got {e})"),
            false,
        ),
    }

    // ---- the size-changing call reaches the file ---------------------------
    r.check(
        "truncate by the name succeeds",
        truncate_path(&name, 4).is_ok(),
    );
    r.check(
        "and the size the descriptor reads back follows",
        fstat(&fd).ok().map(|st| st.st_size) == Some(4),
    );

    let dir = openat(CWD, DIR, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty());
    match &dir {
        Ok(d) => {
            let dir_name = name_of(root, d);
            r.check(
                "truncate by a directory's name answers EISDIR",
                truncate_path(&dir_name, 4).err() == Some(Errno::ISDIR),
            );
        }
        Err(e) => r.check(&format!("the directory opens (got {e})"), false),
    }

    let pipes = pipe_with(PipeFlags::empty());
    match &pipes {
        Ok(p) => {
            r.check(
                "truncate by a pipe's write end name answers EINVAL",
                truncate_path(&name_of(root, &p.1), 4).err() == Some(Errno::INVAL),
            );
            r.check(
                "and by its read end name too",
                truncate_path(&name_of(root, &p.0), 4).err() == Some(Errno::INVAL),
            );
        }
        Err(_) => {
            r.check("truncate by a pipe's write end name answers EINVAL", false);
            r.check("and by its read end name too", false);
        }
    }
    drop(pipes);

    let dev = openat(CWD, NULL_DEV, OFlags::RDWR, Mode::empty());
    match &dev {
        Ok(d) => {
            r.check(
                "truncate by a character device's name answers EINVAL",
                truncate_path(&name_of(root, d), 4).err() == Some(Errno::INVAL),
            );
        }
        Err(e) => r.check(&format!("{NULL_DEV} opens (got {e})"), false),
    }

    // ---- the mutators reach the file ---------------------------------------
    r.check(
        "fchmodat by the name succeeds",
        chmodat_path(&name, 0o600).is_ok(),
    );
    r.check(
        "and the mode the descriptor reads back is the one it set",
        fstat(&fd).ok().map(|st| st.st_mode & 0o7777) == Some(0o600),
    );
    r.check("the mode is put back", chmodat_path(&name, 0o644).is_ok());

    let epoch = Timestamps {
        last_access: Timespec {
            tv_sec: 1_600_000_000,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: 1_600_000_000,
            tv_nsec: 0,
        },
    };
    r.check(
        "utimensat by the name succeeds",
        utimensat(CWD, &name, &epoch, AtFlags::empty()).is_ok(),
    );
    r.check(
        "and the clock the descriptor reads back is the one it set",
        fstat(&fd).ok().map(|st| st.st_mtime) == Some(1_600_000_000),
    );
    match (u32::try_from(by_path.st_uid), u32::try_from(by_path.st_gid)) {
        (Ok(uid), Ok(gid)) => {
            r.check(
                "fchownat by the name succeeds, to the owner it already has",
                chownat(
                    CWD,
                    &name,
                    Some(rustix::process::Uid::from_raw(uid)),
                    Some(rustix::process::Gid::from_raw(gid)),
                    AtFlags::empty(),
                )
                .is_ok(),
            );
        }
        _ => r.check(
            "fchownat by the name succeeds, to the owner it already has",
            false,
        ),
    }
    r.check(
        "access by the name succeeds",
        accessat(CWD, &name, Access::EXISTS, AtFlags::empty()).is_ok(),
    );

    // ---- the name works inside a path --------------------------------------
    let child = openat(
        CWD,
        CHILD,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from_bits_retain(0o644),
    );
    match (&dir, &child) {
        (Ok(d), Ok(c)) => {
            let _ = write(c, b"abc");
            // The directory's descriptor *inside* the path, the shape glibc's
            // `*at` fallbacks build from a dirfd and a name.
            let inside = format!("{}/{}", name_of(root, d), "t-fd-name-child");
            match statat(CWD, &inside, AtFlags::empty()) {
                Ok(st) => r.check(
                    "a name inside the directory's name names the child",
                    kind_of(&st) == FileType::RegularFile && st.st_size == 3,
                ),
                Err(e) => r.check(
                    &format!("a name inside the directory's name stats (got {e})"),
                    false,
                ),
            }
            r.check(
                "and truncate by it succeeds",
                truncate_path(&inside, 1).is_ok(),
            );
            r.check(
                "with the child's own size following",
                fstat(c).ok().map(|st| st.st_size) == Some(1),
            );
        }
        _ => {
            r.check("a name inside the directory's name names the child", false);
            r.check("and truncate by it succeeds", false);
            r.check("with the child's own size following", false);
        }
    }
    drop(child);

    // ---- chdir into a directory's name -------------------------------------
    //
    // The *walk* half is the claim: before the walk denoted a descriptor's
    // second name, this was `ENOTDIR`, because the name was a link and a link
    // is not a directory to descend into. What `getcwd` then reports is a claim
    // too: Linux canonicalises it (measured on a current host, this same call
    // answers `/tmp`), because its `getcwd` walks the dentry chain, and this
    // kernel's cwd stores the name derived from the directory rather than the
    // `/proc/self/fd/N` spelling it was handed.
    match &dir {
        Ok(d) => {
            let dir_name = name_of(root, d);
            r.check(
                "chdir into a directory's name succeeds",
                rustix::process::chdir(&dir_name).is_ok(),
            );
            r.check(
                &format!("getcwd after it is the directory's own name ({DIR}), not the name used"),
                cwd_string() == Some(String::from(DIR)),
            );
            r.check(
                "and the directory it entered is a directory",
                std::fs::metadata(".").ok().map(|m| m.is_dir()) == Some(true),
            );
        }
        Err(_) => {
            r.check("chdir into a directory's name succeeds", false);
            r.check(
                &format!("getcwd after it is the directory's own name ({DIR}), not the name used"),
                false,
            );
            r.check("and the directory it entered is a directory", false);
        }
    }

    // ---- the cwd is the *directory's* name, not the argument's -------------
    //
    // Linux's `getcwd` walks the dentry chain, so a `chdir` through a symlink
    // reads back the target's canonical path, and a `..` taken afterwards climbs
    // from the *target's* parent - not the link's. Measured whole on the host
    // (`.workbuddy/probe_getcwd.c`, Linux 7.2.9) before any of it was made. The
    // shapes below are the two that separate the models: a symlink at the end of
    // the argument, and one that crosses to a *different* parent (where a cwd
    // that kept the argument lands a later `..` in the wrong directory).
    {
        const REAL: &str = "/tmp/t-fd-name-real";
        const LINK: &str = "/tmp/t-fd-name-link";
        const CROSS: &str = "/tmp/t-fd-name-cross";
        const CROSS_LINK: &str = "/tmp/t-fd-name-cross/x";
        for link in [LINK, CROSS_LINK] {
            let _ = unlinkat(CWD, link, AtFlags::empty());
        }
        for dir in [REAL, CROSS] {
            let _ = unlinkat(CWD, dir, AtFlags::REMOVEDIR);
        }
        let made = mkdirat(CWD, REAL, Mode::from(0o755)).is_ok()
            && mkdirat(CWD, CROSS, Mode::from(0o755)).is_ok()
            && std::os::unix::fs::symlink(REAL, LINK).is_ok()
            && std::os::unix::fs::symlink(REAL, CROSS_LINK).is_ok();
        if made {
            let _ = rustix::process::chdir(LINK);
            r.check(
                "chdir through a symlink reads back the target's own name",
                cwd_string() == Some(String::from(REAL)),
            );
            // The link now crosses to a different parent: Linux takes `..` from
            // `/tmp/t-fd-name-real`'s parent, `/tmp`, where a cwd that kept the
            // argument would climb to the link's parent, `/tmp/t-fd-name-cross`.
            let _ = rustix::process::chdir(CROSS_LINK);
            r.check(
                "chdir through a cross-parent link reads back the target too",
                cwd_string() == Some(String::from(REAL)),
            );
            let _ = rustix::process::chdir("..");
            r.check(
                "and `..` then climbs from the target's parent, not the link's",
                cwd_string() == Some(String::from("/tmp")),
            );
        } else {
            r.check("the symlink fixtures are made", false);
        }
        for link in [LINK, CROSS_LINK] {
            let _ = unlinkat(CWD, link, AtFlags::empty());
        }
        for dir in [REAL, CROSS] {
            let _ = unlinkat(CWD, dir, AtFlags::REMOVEDIR);
        }
    }

    let _ = match &start {
        Some(dir) => rustix::process::chdir(dir.as_path()),
        None => rustix::process::chdir("/"),
    };

    // ---- open of the name is a fresh description ---------------------------
    r.check(
        "the descriptor is rewound",
        rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0)) == Ok(0),
    );
    let fresh = openat(CWD, &name, OFlags::RDONLY, Mode::empty());
    match &fresh {
        Ok(f) => {
            let mut two = [0u8; 2];
            let n = read(f, &mut two);
            r.check(
                "a read through the opened name answers the file's first bytes",
                n == Ok(2) && two == *b"he",
            );
            r.check(
                "and the descriptor it names stands where it was",
                rustix::fs::seek(&fd, rustix::fs::SeekFrom::Current(0)) == Ok(0),
            );
        }
        Err(e) => {
            r.check(&format!("open by the name succeeds (got {e})"), false);
            r.check("and the descriptor it names stands where it was", false);
        }
    }
    drop(fresh);
    // The flags are the *call's*, not the descriptor's: the descriptor is
    // O_RDWR, and a name opened O_WRONLY reports the word the open asked for.
    let wo = openat(CWD, &name, OFlags::WRONLY, Mode::empty());
    match &wo {
        Ok(f) => {
            let flags = rustix::fs::fcntl_getfl(f);
            r.check(
                "the name's own flags are the ones the call asked for",
                flags.map(|f| (f.bits() as u32) & 0o3) == Ok(1),
            );
        }
        Err(e) => r.check(&format!("the name opens O_WRONLY (got {e})"), false),
    }
    drop(wo);

    // ---- a file whose path is gone is still nameable -----------------------
    let gone = openat(
        CWD,
        GONE,
        OFlags::CREATE | OFlags::RDWR | OFlags::TRUNC,
        Mode::from_bits_retain(0o644),
    );
    match gone {
        Ok(g) => {
            let _ = write(&g, b"still here");
            r.check(
                "the fixture's path is removed",
                unlinkat(CWD, GONE, AtFlags::empty()).is_ok(),
            );
            let gone_name = name_of(root, &g);
            match statat(CWD, &gone_name, AtFlags::empty()) {
                Ok(st) => r.check(
                    "a deleted file's name still stats, as the file it is",
                    kind_of(&st) == FileType::RegularFile && st.st_size == 10,
                ),
                Err(e) => r.check(&format!("a deleted file's name stats (got {e})"), false),
            }
            r.check(
                "and truncate by it succeeds",
                truncate_path(&gone_name, 3).is_ok(),
            );
            r.check(
                "with the size the descriptor reads back following",
                fstat(&g).ok().map(|st| st.st_size) == Some(3),
            );
            drop(g);
            // Once the descriptor is gone the name names nothing, and that is
            // ENOENT rather than the EBADF the bare number would earn.
            r.check(
                "and is ENOENT once the descriptor is closed",
                statat(CWD, &gone_name, AtFlags::empty()).err() == Some(Errno::NOENT),
            );
            r.check(
                "as is truncating it",
                truncate_path(&gone_name, 3).err() == Some(Errno::NOENT),
            );
        }
        Err(e) => {
            r.check(&format!("the second fixture opens (got {e})"), false);
            r.check(
                "a deleted file's name still stats, as the file it is",
                false,
            );
            r.check("and truncate by it succeeds", false);
            r.check("with the size the descriptor reads back following", false);
            r.check("and is ENOENT once the descriptor is closed", false);
            r.check("as is truncating it", false);
        }
    }

    // ---- the devfs spelling names the same place ---------------------------
    let devfd = openat(
        CWD,
        "/dev/fd",
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    );
    match &devfd {
        Ok(_) => {
            let dev_name = format!("/dev/fd/{}", fd.as_raw_fd());
            match statat(CWD, &dev_name, AtFlags::empty()) {
                Ok(st) => r.check(
                    "/dev/fd/N names the same file as /proc/self/fd/N",
                    st.st_ino == by_path.st_ino && kind_of(&st) == FileType::RegularFile,
                ),
                Err(e) => r.check(&format!("/dev/fd/N stats (got {e})"), false),
            }
        }
        Err(_) => {
            // A payload with no devfs: the name is the test's own gap, and it
            // says so rather than claiming a pass.
            println!("     no /dev/fd to reach the same descriptor by (payload has no devfs)");
        }
    }
    drop(devfd);

    drop(fd);
    drop(dir);
    drop(dev);
    for path in [FILE, CHILD] {
        let _ = unlinkat(CWD, path, AtFlags::empty());
    }
    let _ = start;
    r.finish()
}
