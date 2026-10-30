//! t-proc-modes - the word a procfs/sysfs node reports about *itself*.
//!
//! `/proc` and `/sys` here are filesystems that keep no modes: nothing on their
//! nodes was ever written down, so `lstat` fell back to the kind's default -
//! `0755` for a directory, `0644` for a file, `0777` for a link - for every node
//! in both trees. Linux has no such default. A proc file carries the mode it was
//! *created* with (`proc_create` with no mode means `S_IRUGO`, `0444`), a sysfs
//! attribute the one its macro names (`DEVICE_ATTR_RO` against
//! `DEVICE_ATTR_RW`), a directory the one its creator asked for, and a
//! descriptor's second name the direction its descriptor was opened in.
//!
//! The classification was measured on a current host first - every path this
//! kernel's two trees build - with `.workbuddy/probe_proc_node_modes.c`
//! (`lstat` through `syscall(2)`, so no libc's idea of a mode stands in for the
//! kernel's). What it says, and what this program pins:
//!
//!   * a proc directory is **`0555`** - `/proc`, `/proc/sys/...`, every
//!     `/proc/<pid>`, `/proc/<pid>/task`, `/proc/bus/pci/<bus>` - so the tree
//!     is readable and not writable;
//!   * a generated file nothing can write is **`0444`**, which is what Linux's
//!     own `proc_create` with no mode means (`/proc/version`, `/proc/meminfo`,
//!     `/proc/<pid>/status`, `/proc/net/tcp`), where this kernel reported the
//!     kind's `0644`;
//!   * a file that can *also* be written is **`0644`** - the sysctls, and a
//!     process's own `oom_score_adj` - and a process's
//!     `/proc/bus/pci/<bus>/<dev>.<func>` (the live configuration space) is the
//!     same word;
//!   * two directories are narrower than the rest, because what they hold is
//!     not for listing: `/proc/<pid>/fd` is `0500` (`t-fd-name`'s subject) and
//!     `/proc/<pid>/ns` is **`0511`** - a name inside it may be *reached*,
//!     which is what `setns` does, and the directory does not list what is in
//!     it;
//!   * `/proc/<pid>/environ` is **`0400`** where its neighbour `cmdline` is
//!     `0444`: an environment can carry secrets, and Linux makes the file the
//!     owner's alone;
//!   * `/proc/cmdline`'s own `st_size` is **the length of the line it
//!     answers**: the one generated file whose length is a fact (the command
//!     line cannot change after the boot), where every other one reports zero.
//!     The pair is claimed, because a size on its own could be a constant;
//!   * `/sys` itself is **`0555`** while every directory under it is `0755`
//!     (the kind's own word), a read-only attribute is `0444` (`dev`, `size`,
//!     a function's `vendor`) and an attribute that can be written - a device's
//!     `uevent`, which `udevadm trigger` writes - is `0644`.
//!
//! # What is named rather than claimed
//!
//! A word is a *report*, and procfs here polices nothing: it keeps no modes to
//! enforce, so an `0400` node is readable by any caller exactly as its `0444`
//! neighbours are. That is the mode-less filesystem's own limit, carried by
//! `docs/uapi/` §5.2, and it is not what this program is about - the claim is
//! the word a caller reads back from `lstat`, which is the whole of what a
//! listing tool sees.
//!
//! One thing the word cannot be talked out of, and it is claimed at the end of
//! this file: **the bits cannot be changed at all.** A node with no attributes
//! answers `EPERM` to `chmod`, `fchmod` and `fchmodat`, root included - measured
//! on a current host as both uid 1000 and root (`build/scratch/attrrefuse/p1.c`);
//! the same call on a tmpfs file is `0`, which is the control. The bit *report*
//! and the bit *mutation* are two different promises, and a filesystem can keep
//! the first and refuse the second (`docs/notes/` §3.17, item 138).
//!
//! Nor are the nodes Linux makes writable and this kernel did not claimed at
//! Linux's `0644`: `/proc/sys/kernel/pid_max`, `/proc/sys/kernel/threads-max`,
//! `/proc/sys/kernel/pty/max` and `/proc/<pid>/comm` are read-only files here,
//! so they report the read-only word. That is the file telling the truth about
//! itself - it refuses the write - and §5.2 names the difference from Linux's
//! node beside the measurement.
//!
//! `/proc/self`, `/proc/thread-self` and `/proc/mounts` were the third
//! exception, and are not one any more: the *kind* differed before the mode
//! could, because Linux answers a `0777` **link** for all three where this
//! kernel answered a directory (a mirror of the reading process, and a mount
//! table) - so `§5.2` carried the shape and these names were left unclaimed.
//! The shapes are Linux's now and the three are claimed below, target text and
//! `st_size` included (`docs/uapi/` §5.1 item 46).
//!
//! The two **mount names themselves** - `/proc` and `/sys` - are claimed
//! through either door, and used to be the one pair this program had to claim
//! through `stat` alone: the no-follow door answered the directory a mount
//! *covers* here rather than the filesystem mounted on it, which was a
//! divergence of its own, named in §5.2 beside its measurement. The mount
//! crossing is in the no-follow walk now, so `lstat` and `stat` agree about a
//! mount point, and `t-mountpoint` is the program that claims the rule with a
//! mount point of its own making.
//!
//! A host run makes every claim: the words above are Linux's, measured there
//! first, and the two fixtures a machine has to be asked for - which block
//! device exists, which PCI bus lists a function - are asked for by walking,
//! not named.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, FileType, statat};

/// Claim that `path` names a node of `kind` whose own word is `mode`.
///
/// One claim per node, and the failure names what came back rather than only
/// that something did not match: the number *is* this program's subject.
fn claims(r: &mut Report, path: &str, kind: FileType, mode: u32) {
    claims_through(r, path, kind, mode, AtFlags::SYMLINK_NOFOLLOW, "");
}

/// The same claim for a name that is a **mount point** - `/proc` and `/sys` are
/// covered by the filesystem mounted on them - made through **both** doors.
///
/// `stat` follows the path into the mounted filesystem, and `lstat` *crosses*
/// the mount without following a link: a mount is stepped into whatever
/// `LOOKUP_NOFOLLOW` says about links (measured on a current host,
/// `.workbuddy/probe_mountpoint_doors.c`). This pair used to be the program's
/// one exception - the no-follow door answered the covered directory here, so
/// only `stat` was claimed (`docs/uapi/` §5.2 named the divergence) - and the
/// crossing is in the no-follow walk now, which is what makes two claims where
/// there was one.
fn claims_mounted(r: &mut Report, path: &str, kind: FileType, mode: u32) {
    claims_through(r, path, kind, mode, AtFlags::SYMLINK_NOFOLLOW, "");
    claims_through(
        r,
        path,
        kind,
        mode,
        AtFlags::empty(),
        " through the following door",
    );
}

/// One claim, through one door: `door` names which of the two it was, so the
/// pair above reads as two claims rather than one made twice.
fn claims_through(
    r: &mut Report,
    path: &str,
    kind: FileType,
    mode: u32,
    flags: AtFlags,
    door: &str,
) {
    let want = format!("{path}{door} reports {mode:04o} for a {}", name_of(kind));
    let got = statat(CWD, path, flags)
        .ok()
        .map(|st| (FileType::from_raw_mode(st.st_mode), st.st_mode & 0o7777));
    match got {
        Some((got_kind, got_mode)) if got_kind == kind && got_mode == mode => r.check(&want, true),
        Some((got_kind, got_mode)) => r.check(
            &format!("{want} (got {got_mode:04o} for a {})", name_of(got_kind)),
            false,
        ),
        None => r.check(&format!("{want} (no such name)"), false),
    }
}

/// A kind's name as the claim reads it.
fn name_of(kind: FileType) -> &'static str {
    match kind {
        FileType::Directory => "directory",
        FileType::RegularFile => "file",
        FileType::Symlink => "link",
        FileType::CharacterDevice => "character device",
        FileType::BlockDevice => "block device",
        FileType::Fifo => "fifo",
        FileType::Socket => "socket",
        _ => "node",
    }
}

/// The first name under `dir` - lowest first, so the choice is the machine's
/// and not the filesystem's listing order - for which `wanted` holds, as a full
/// path.
///
/// Both fixtures below are of this shape on purpose: which block device a
/// machine has, and which bus its first PCI function sits on, are facts about
/// the machine rather than constants a test may write down.
fn first_in(dir: &str, wanted: impl Fn(&str) -> bool) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .find(|name| wanted(&format!("{dir}/{name}")))
        .map(|name| format!("{dir}/{name}"))
}

fn main() {
    let mut r = Report::new();

    // ---- the tree, and the directories in it ------------------------------
    //
    // The word is the *directory's* wherever Linux's creator named one, and
    // `0555` is what nearly every proc directory answers - where this kernel
    // reported the kind's `0755` for all of them. The two roots are claimed
    // through the door that crosses their mount point (`claims_mounted`), and
    // every other name is `/proc/self/...` rather than a numeric pid, so the
    // `ok:` lines are the same on every machine that runs this.
    claims_mounted(&mut r, "/proc", FileType::Directory, 0o555);
    claims(&mut r, "/proc/sys/kernel", FileType::Directory, 0o555);
    claims(&mut r, "/proc/self/task", FileType::Directory, 0o555);

    // ---- the two directories narrower than the rest -----------------------
    //
    // `/proc/<pid>/ns` is `0511`: the names in it may be opened - that is the
    // door `setns` walks - and it does not list them. (`/proc/<pid>/fd`'s
    // `0500` is the same shape and is pinned by `t-fd-name` with the links
    // inside it.)
    claims(&mut r, "/proc/self/ns", FileType::Directory, 0o511);

    // ---- the files --------------------------------------------------------
    //
    // Three pairs, and the pairing is the claim: a generated file nothing can
    // write against one that can, and the environment against the command line
    // beside it, so a kernel that answered one word for every file - or the
    // kind's default for both - fails whichever half it got wrong.
    claims(&mut r, "/proc/version", FileType::RegularFile, 0o444);
    claims(&mut r, "/proc/self/status", FileType::RegularFile, 0o444);
    claims(
        &mut r,
        "/proc/sys/kernel/hostname",
        FileType::RegularFile,
        0o644,
    );
    claims(
        &mut r,
        "/proc/self/oom_score_adj",
        FileType::RegularFile,
        0o644,
    );
    claims(&mut r, "/proc/self/environ", FileType::RegularFile, 0o400);
    claims(&mut r, "/proc/self/cmdline", FileType::RegularFile, 0o444);

    // ---- and `/proc/cmdline`'s own size ------------------------------------
    //
    // The length is the one the file answers: the claim is the *pair*, since a
    // size alone would be satisfied by a constant. Non-zero is claimed beside
    // it, because zero is the answer every other generated file gives.
    //
    // The read is a *loop*, not one 512-byte call. A test sweep hands each
    // guest its lane's program list on the command line (`docs/notes/` §15.94),
    // so the line is about a kilobyte; a single buffer answered its first 512
    // bytes and the pair then read as "the size is wrong" when it was the
    // reader that was short. The claim is the length of the *line*, which needs
    // the whole line - and the loop is bounded, because a generated file that
    // never reached the end would otherwise stall the sweep instead of failing
    // this claim.
    const CAP: usize = 64 * 1024;
    let size = statat(CWD, "/proc/cmdline", AtFlags::empty())
        .ok()
        .map(|st| st.st_size);
    let answered = (|| {
        let mut file = std::fs::File::open("/proc/cmdline").ok()?;
        let mut all: Vec<u8> = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            if all.len() >= CAP {
                break;
            }
            let n = std::io::Read::read(&mut file, &mut buf).ok()?;
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        Some(all.len())
    })();
    r.check(
        "and /proc/cmdline's own size is the length of the line it answers",
        matches!((size, answered), (Some(size), Some(n)) if size > 0 && size as usize == n),
    );

    // ---- a process's own configuration space ------------------------------
    //
    // `/proc/bus/pci/<bus>/<dev>.<func>` is the live configuration window on a
    // device (256 bytes, or 4096 when the device publishes extended space:
    // `t-pci` asks that), readable and writable, and Linux makes it `0644` -
    // the word the file's own read-write shape is worth. Which bus the
    // machine's first function sits on is asked for rather than named.
    let function = first_in("/proc/bus/pci", path_is_a_bus)
        .and_then(|bus| first_in(&bus, |path| path.contains('.')));
    match &function {
        Some(path) => claims(&mut r, path, FileType::RegularFile, 0o644),
        None => r.check("the pci bus lists a function's config file", false),
    }

    // ---- sysfs ------------------------------------------------------------
    //
    // The tree's own root is `0555` and every directory under it is the kind's
    // `0755`; the attributes split by whether they can be written.
    claims_mounted(&mut r, "/sys", FileType::Directory, 0o555);
    claims(&mut r, "/sys/class", FileType::Directory, 0o755);
    claims(&mut r, "/sys/class/block", FileType::Directory, 0o755);
    let device = first_in("/sys/class/block", |path| {
        std::path::Path::new(&format!("{path}/dev")).exists()
    });
    match &device {
        Some(device) => {
            claims(
                &mut r,
                &format!("{device}/dev"),
                FileType::RegularFile,
                0o444,
            );
            claims(
                &mut r,
                &format!("{device}/size"),
                FileType::RegularFile,
                0o444,
            );
            claims(
                &mut r,
                &format!("{device}/uevent"),
                FileType::RegularFile,
                0o644,
            );
        }
        None => r.check("a block device is listed under /sys/class/block", false),
    }

    // ---- the three names Linux answers as links ---------------------------
    //
    // A mount point is not the only place the *shape* decides the answer:
    // `/proc/self`, `/proc/thread-self` and `/proc/mounts` are `0777` links on
    // Linux, where this kernel answered a directory (a mirror of the reading
    // process, and a mount table) for all three. Measured whole on a current
    // host (`.workbuddy/probe_proc_self_links.c`): `lstat` is `0777` for each of
    // them, the target text is the reader's own number, `<pid>/task/<tid>` and
    // `self/mounts`, `stat` follows them to a directory, a directory and a
    // regular file, and a directory listing labels all three `DT_LNK`. The
    // *sizes* differ by how the name was made, which is the one thing here that
    // a table cannot guess: a live target was registered empty and reports
    // **0** however many bytes `readlink` answers, while the fixed one reports
    // the length of its text.
    const MOUNTS_TARGET: &str = "self/mounts";
    for (path, followed_kind, followed_mode) in [
        ("/proc/self", FileType::Directory, 0o555),
        ("/proc/thread-self", FileType::Directory, 0o555),
        ("/proc/mounts", FileType::RegularFile, 0o444),
    ] {
        claims(&mut r, path, FileType::Symlink, 0o777);
        claims_through(
            &mut r,
            path,
            followed_kind,
            followed_mode,
            AtFlags::empty(),
            " through the following door",
        );
    }
    let pid = std::process::id();
    let tid = unsafe { libc::gettid() } as u32;
    claims_text(
        &mut r,
        "/proc/self reads back the reader's own number",
        "/proc/self",
        &pid.to_string(),
    );
    claims_text(
        &mut r,
        "/proc/thread-self reads back the reader's own thread",
        "/proc/thread-self",
        &format!("{pid}/task/{tid}"),
    );
    claims_text(
        &mut r,
        "/proc/mounts reads back the file it points at",
        "/proc/mounts",
        MOUNTS_TARGET,
    );
    // The size, which is the length of the text the name was *made* with: an
    // empty one for the two live names, and this text's own for the fixed one.
    for (path, want) in [
        ("/proc/self", 0i64),
        ("/proc/thread-self", 0),
        ("/proc/mounts", MOUNTS_TARGET.len() as i64),
    ] {
        let got = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|st| st.st_size);
        r.check(
            &format!(
                "{path}'s own st_size is the registered target's length (got {got:?}, want {want})"
            ),
            got == Some(want),
        );
    }
    // And the kind a *listing* hands out, which procfs stores beside the node:
    // a directory that reports a link whose entry says `DT_DIR` is the mistake
    // this claim exists for.
    for name in ["self", "thread-self", "mounts"] {
        let listed = listed_kind("/proc", name);
        r.check(
            &format!(
                "/proc lists {name} as {} (got {listed:?})",
                dirent_kind_name(libc::DT_LNK as u8)
            ),
            listed == Some(libc::DT_LNK as u8),
        );
    }

    chmod_doors(&mut r);
    owner_and_time_doors(&mut r);
    sysfs_modes(&mut r);

    r.finish();
}

/// The three `chmod` doors against a node that cannot carry attributes, and the
/// control that shows the refusal is the node's rather than the caller's.
///
/// Measured on a current host first (`build/scratch/attrrefuse/p1.c`, Linux
/// 7.2.11, uid 1000 *and* root): `chmod`, `fchmod` by descriptor and
/// `fchmodat(fd, "", AT_EMPTY_PATH)` answer **`EPERM` (1)** on
/// `/proc/self/status` and `/proc/sys/vm/max_map_count` for both callers -
/// procfs has no `setattr` operation, which is why root's answer is the same as
/// an ordinary user's - while the same call on a tmpfs file answers `0` and the
/// word does move. The claimed pair is the refusal *and* the word it leaves
/// behind: an answer of `0` that writes nothing is the shape this pins against.
fn chmod_doors(r: &mut Report) {
    const EPERM: i32 = 1;
    fn errno() -> i32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default()
    }
    fn cstring(path: &str) -> Option<std::ffi::CString> {
        std::ffi::CString::new(path).ok()
    }
    // One claim for `chmod` alone: the door's own answer, and the word after it.
    for path in ["/proc/self/status", "/proc/sys/vm/max_map_count"] {
        let Some(cpath) = cstring(path) else { continue };
        let rc = unsafe { libc::chmod(cpath.as_ptr(), 0o640) };
        let got = errno();
        r.check(
            &format!("chmod on {path} is EPERM for a caller who owns nothing there (got {got})"),
            rc != 0 && got == EPERM,
        );
        let word = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|st| st.st_mode & 0o7777);
        let printed = word.map(|m| format!("{m:04o}"));
        r.check(
            &format!(
                "and the word {path} reports is the one it had (got {printed:?}, want 0444 or 0644)"
            ),
            matches!(word, Some(0o444) | Some(0o644)),
        );
    }
    // The descriptor doors, which are what a program actually holds.
    let Some(status) = cstring("/proc/self/status") else {
        return;
    };
    let fd = unsafe { libc::open(status.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        r.check("a proc file opens for the descriptor doors", false);
    } else {
        let rc = unsafe { libc::fchmod(fd, 0o640) };
        let got = errno();
        r.check(
            &format!("fchmod on a proc file's descriptor is EPERM (got {got})"),
            rc != 0 && got == EPERM,
        );
        let empty = cstring("").expect("an empty name is a valid C string");
        let rc = unsafe { libc::fchmodat(fd, empty.as_ptr(), 0o640, libc::AT_EMPTY_PATH) };
        let got = errno();
        r.check(
            &format!("the AT_EMPTY_PATH spelling is EPERM too (got {got})"),
            rc != 0 && got == EPERM,
        );
        unsafe { libc::close(fd) };
    }
    // The control: a filesystem that *does* keep modes accepts the same call, so
    // a refusal everywhere would fail this claim rather than pass for a reason.
    let tmp = "/tmp/t-proc-modes-chmod";
    let _ = std::fs::remove_file(tmp);
    std::fs::write(tmp, b"x").ok();
    if let Some(cpath) = cstring(tmp) {
        let rc = unsafe { libc::chmod(cpath.as_ptr(), 0o600) };
        r.check(
            &format!("chmod on a tmpfs file is 0, so the refusal above is the node's (got {rc})"),
            rc == 0,
        );
    }
    let _ = std::fs::remove_file(tmp);
}

/// The other two attribute doors (`chown`/`utimensat`) against a node that
/// keeps no owner and no timestamps.
///
/// Measured on a current host first (`build/scratch/attr2/p1.c` and `p2.c`,
/// Linux 7.2.11, uid 1000 *and* root), and the shape is not one rule: a
/// `/proc/sys` file **drops** the timestamps (all three `utimensat` shapes
/// answer `0` to root) but **refuses** the owner (`chown` is `EPERM` even for
/// root, because its inode has no `setattr`); a `/proc/<pid>` directory refuses
/// both, whoever asks, because it mirrors a process; and a sysfs directory
/// drops them like the sysctl. Who may ask is the ordinary question on top of
/// that: an ordinary user's `utimensat` on a `0444` proc file is `EACCES` for
/// the "now" shapes and `EPERM` for an explicit pair, and on `/dev/null` (mode
/// `0666`) the "now" shape is `0` because write permission stands in for
/// ownership.
fn owner_and_time_doors(r: &mut Report) {
    const EPERM: i32 = 1;
    const EACCES: i32 = 13;
    const ENOENT: i32 = 2;
    fn errno() -> i32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default()
    }
    fn name(path: &str) -> Option<std::ffi::CString> {
        std::ffi::CString::new(path).ok()
    }
    /// `utimensat` in one of its three shapes: 0 = both clocks "now", 1 = an
    /// explicit pair, 2 = no pair at all.
    fn touch(path: &str, shape: i32) -> i32 {
        let Some(cpath) = name(path) else {
            return ENOENT;
        };
        let now = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            },
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            },
        ];
        let set = [
            libc::timespec {
                tv_sec: 1_000_000,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: 1_000_000,
                tv_nsec: 0,
            },
        ];
        let times = match shape {
            0 => now.as_ptr(),
            1 => set.as_ptr(),
            _ => std::ptr::null(),
        };
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times, 0) };
        if rc == 0 { 0 } else { errno() }
    }
    fn chown_self(path: &str) -> i32 {
        let Some(cpath) = name(path) else {
            return ENOENT;
        };
        let Ok(st) = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) else {
            return ENOENT;
        };
        let rc = unsafe { libc::chown(cpath.as_ptr(), st.st_uid, st.st_gid) };
        if rc == 0 { 0 } else { errno() }
    }

    // ---- root: the sysctl drops the times and refuses the owner ---------------
    let sysctl = "/proc/sys/vm/max_map_count";
    let now = touch(sysctl, 0);
    let set = touch(sysctl, 1);
    let null = touch(sysctl, 2);
    r.check(
        &format!("a /proc/sys file accepts the timestamps and drops them (now/null {now}/{null})"),
        now == 0 && null == 0,
    );
    r.check(
        &format!("an explicit pair of times on it is accepted for root too (got {set})"),
        set == 0,
    );
    let sysctl_owner = chown_self(sysctl);
    r.check(
        &format!("chown on a /proc/sys file is EPERM even for root (got {sysctl_owner})"),
        sysctl_owner == EPERM,
    );

    // ---- root: the pid directory refuses both, whoever asks ------------------
    let pid_dir = "/proc/self";
    let m = (touch(pid_dir, 0), touch(pid_dir, 1), touch(pid_dir, 2));
    r.check(
        &format!(
            "the pid directory refuses the three timestamps shapes with EPERM (got {}/{}/{})",
            m.0, m.1, m.2
        ),
        m == (EPERM, EPERM, EPERM),
    );
    let pid_owner = chown_self(pid_dir);
    r.check(
        &format!("and it refuses chown with EPERM for root too (got {pid_owner})"),
        pid_owner == EPERM,
    );
    // A proc file accepts the owner question: root may name the owner, and the
    // call answers 0 even though the tree keeps what it named nowhere.
    let status_owner = chown_self("/proc/self/status");
    r.check(
        &format!("chown on a proc file answers 0 (accepted, dropped; got {status_owner})"),
        status_owner == 0,
    );

    // ---- the unprivileged half, in a child that dropped its ids --------------
    const WRITABLE_NOW: u32 = 1;
    const WRITABLE_EXPLICIT: u32 = 2;
    const WRITABLE_NULL: u32 = 4;
    const CHOWN_REFUSED: u32 = 8;
    const DEVICE_NOW: u32 = 16;
    const DEVICE_EXPLICIT: u32 = 32;
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::setuid(65534);
        }
        let mut bits = 0;
        if touch(sysctl, 0) == EACCES {
            bits |= WRITABLE_NOW;
        }
        if touch(sysctl, 1) == EPERM {
            bits |= WRITABLE_EXPLICIT;
        }
        if touch(sysctl, 2) == EACCES {
            bits |= WRITABLE_NULL;
        }
        if chown_self(sysctl) == EPERM {
            bits |= CHOWN_REFUSED;
        }
        if touch("/dev/null", 0) == 0 {
            bits |= DEVICE_NOW;
        }
        if touch("/dev/null", 1) == EPERM {
            bits |= DEVICE_EXPLICIT;
        }
        std::process::exit(bits as i32);
    }
    let mut status = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
    }
    let bits = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else {
        0
    };
    if bits == 0 && !libc::WIFEXITED(status) {
        r.check(
            "the unprivileged child could run the attribute doors",
            false,
        );
        return;
    }
    r.check(
        &format!(
            "an ordinary user's \"now\" timestamps on a 0444 proc file are EACCES (bits {bits:02x})"
        ),
        bits & WRITABLE_NOW != 0,
    );
    r.check(
        &format!("an explicit pair from that user is EPERM (bits {bits:02x})"),
        bits & WRITABLE_EXPLICIT != 0,
    );
    r.check(
        &format!("the null-pointer form from that user is EACCES (bits {bits:02x})"),
        bits & WRITABLE_NULL != 0,
    );
    r.check(
        &format!("and that user's chown on it is EPERM (bits {bits:02x})"),
        bits & CHOWN_REFUSED != 0,
    );
    r.check(
        &format!("a 0666 device node takes the \"now\" form from that user (bits {bits:02x})"),
        bits & DEVICE_NOW != 0,
    );
    r.check(
        &format!("and refuses the explicit pair, since it does not own it (bits {bits:02x})"),
        bits & DEVICE_EXPLICIT != 0,
    );
}

/// sysfs's word is the node's too: root's `chmod` is stored, an ordinary
/// user's is `EPERM`, and a read-only attribute refuses a write descriptor.
///
/// Measured on a current host first (`build/scratch/sysfsattr/p.c`, `p2.c`,
/// Linux 7.2.11, uid 1000 *and* root): `chmod 0700 /sys/kernel` answers `0` for
/// root and `stat` then reports `0700` - kernfs keeps the word on the inode, so
/// the *same* word the next process sees - while an ordinary user's call is
/// `EPERM` (1) and leaves the word alone; `fchmod` of a sysfs attribute's
/// descriptor takes the same path; and `open("/sys/kernel/ostype",
/// O_WRONLY)` is `EACCES` (13), because that attribute is `0444` and has no
/// write method. The same two calls run here against `/sys/kernel/ostype`, the
/// attribute this tree publishes (`uevent_seqnum` is Linux's, not ours). Every word this function moves is put back, so the tree is the
/// same when the run ends.
fn sysfs_modes(r: &mut Report) {
    const EPERM: i32 = 1;
    const EACCES: i32 = 13;
    fn errno() -> i32 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default()
    }
    fn cstring(path: &str) -> Option<std::ffi::CString> {
        std::ffi::CString::new(path).ok()
    }
    fn word(path: &str) -> Option<u32> {
        statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
            .ok()
            .map(|st| st.st_mode & 0o7777)
    }
    fn chmod(path: &str, mode: u32) -> i32 {
        match cstring(path) {
            Some(name) => unsafe { libc::chmod(name.as_ptr(), mode as libc::mode_t) },
            None => -1,
        }
    }

    let dir = "/sys/kernel";
    // The read-only attribute each kernel publishes: this tree has `ostype`,
    // Linux has `uevent_seqnum`, and the claims are about the shape rather than
    // the name - so the pin walks the candidates it knows and uses the one this
    // machine has.
    let Some(file) = ["/sys/kernel/ostype", "/sys/kernel/uevent_seqnum"]
        .into_iter()
        .find(|path| word(path).is_some())
    else {
        r.check("the tree publishes a read-only attribute to test", false);
        return;
    };

    // Root asks with the word's permission and is still refused: an open for
    // writing asks whether the node *has* a write method, and this one does
    // not. Linux answers `EACCES` for root on a `0444` attribute and hands a
    // descriptor for a *writable* one (`net/lo/mtu`) and for a `0444` regular
    // file - measured, `build/scratch/wopen/q.c`, `docs/notes/` §3.25.
    if let Some(name) = cstring(file) {
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY) };
        let got = errno();
        r.check(
            &format!(
                "root's write-open of a read-only attribute is EACCES (got {fd}, errno {got})"
            ),
            fd < 0 && got == EACCES,
        );
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR) };
        let got = errno();
        r.check(
            &format!("and O_RDWR is refused the same way (got {fd}, errno {got})"),
            fd < 0 && got == EACCES,
        );
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY) };
        r.check(
            &format!("while the read open is a descriptor (got {fd})"),
            fd >= 0,
        );
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }
    // The word is not the rule: a `0444` *regular* file has a write method, and
    // root's write-open of it is a descriptor on Linux too.
    let contrast = "/tmp/t-proc-modes-0444";
    if let Some(name) = cstring(contrast) {
        let _ = std::fs::write(contrast, b"x");
        let _ = chmod(contrast, 0o444);
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY) };
        r.check(
            &format!(
                "another 0444 file does take a write descriptor, so the word is not the rule (got {fd})"
            ),
            fd >= 0,
        );
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        let _ = std::fs::remove_file(contrast);
    }

    // Root's chmod is accepted *and stored*: the next reader sees the new word.
    let rc = chmod(dir, 0o700);
    let got = errno();
    r.check(
        &format!("root's chmod on a sysfs directory is 0 (got {rc}, errno {got})"),
        rc == 0,
    );
    r.check(
        &format!(
            "and the word it wrote is the one stat reports (got {:?})",
            word(dir).map(|m| format!("{m:04o}"))
        ),
        word(dir) == Some(0o700),
    );
    let rc = chmod(dir, 0o755);
    r.check(
        &format!("root may put the word back (got {rc})"),
        rc == 0 && word(dir) == Some(0o755),
    );

    // The descriptor door on an attribute, and its restore.
    if let Some(name) = cstring(file) {
        let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY) };
        if fd >= 0 {
            let rc = unsafe { libc::fchmod(fd, 0o600) };
            let got = errno();
            r.check(
                &format!("fchmod of a sysfs attribute's descriptor is 0 (got {rc}, errno {got})"),
                rc == 0,
            );
            r.check(
                &format!(
                    "and the word moves with it (got {:?})",
                    word(file).map(|m| format!("{m:04o}"))
                ),
                word(file) == Some(0o600),
            );
            unsafe { libc::close(fd) };
            let rc = chmod(file, 0o444);
            r.check(
                &format!("the attribute's word is put back (got {rc})"),
                rc == 0 && word(file) == Some(0o444),
            );
        } else {
            r.check("a sysfs attribute opens for its descriptor", false);
        }
    }

    // ---- the unprivileged half: the word decides ---------------------------
    const CHMOD_REFUSED: u32 = 1;
    const WRITE_OPEN_REFUSED: u32 = 2;
    const READ_OPEN_ALLOWED: u32 = 4;
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::setuid(65534);
        }
        let mut bits = 0;
        if chmod(dir, 0o700) != 0 && errno() == EPERM {
            bits |= CHMOD_REFUSED;
        }
        if let Some(name) = cstring(file) {
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_WRONLY) };
            if fd < 0 && errno() == EACCES {
                bits |= WRITE_OPEN_REFUSED;
            }
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
            let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY) };
            if fd >= 0 {
                bits |= READ_OPEN_ALLOWED;
                unsafe { libc::close(fd) };
            }
        }
        std::process::exit(bits as i32);
    }
    let mut status = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
    }
    let bits = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else {
        0
    };
    r.check(
        &format!("an ordinary user's chmod on sysfs is EPERM (bits {bits:02x})"),
        bits & CHMOD_REFUSED != 0,
    );
    r.check(
        &format!("opening a 0444 attribute for write is EACCES (bits {bits:02x})"),
        bits & WRITE_OPEN_REFUSED != 0,
    );
    r.check(
        &format!("while the same user reads it (bits {bits:02x})"),
        bits & READ_OPEN_ALLOWED != 0,
    );
    r.check(
        &format!(
            "and the directory's word is the one root left (got {:?})",
            word(dir).map(|m| format!("{m:04o}"))
        ),
        word(dir) == Some(0o755),
    );
}

/// Claim that `path`'s link reads back `want`.
///
/// The *passing* label has to be the same on every machine that runs this - the
/// harness diffs the two machines' `ok:` lines - and the reader's own number is
/// not the same on two boots, let alone two machines. So the two values go into
/// the label only when they disagree, where a diagnosis is worth more than a
/// diffable line.
fn claims_text(r: &mut Report, what: &str, path: &str, want: &str) {
    let got = link_target(path);
    if got.as_deref() == Some(want) {
        r.check(what, true);
    } else {
        r.check(&format!("{what} (got {got:?}, want {want:?})"), false);
    }
}

/// What `readlink` answers, as text.
fn link_target(path: &str) -> Option<String> {
    std::fs::read_link(path)
        .ok()
        .map(|target| target.to_string_lossy().into_owned())
}

/// The `d_type` a listing gives `name` under `dir` - the filesystem's own label
/// for the entry, which is a different answer from what `lstat` says.
fn listed_kind(dir: &str, name: &str) -> Option<u8> {
    let dir = std::ffi::CString::new(dir).ok()?;
    let want = std::ffi::CString::new(name).ok()?;
    // SAFETY: `opendir`'s handle is closed on every path out, and each
    // `readdir` result is read before the next call.
    unsafe {
        let stream = libc::opendir(dir.as_ptr());
        if stream.is_null() {
            return None;
        }
        let mut found = None;
        loop {
            let entry = libc::readdir(stream);
            if entry.is_null() {
                break;
            }
            if libc::strcmp((*entry).d_name.as_ptr(), want.as_ptr()) == 0 {
                found = Some((*entry).d_type);
                break;
            }
        }
        libc::closedir(stream);
        found
    }
}

/// A `d_type`'s name, for a failure that says which label came back.
fn dirent_kind_name(kind: u8) -> &'static str {
    match kind {
        libc::DT_DIR => "DT_DIR",
        libc::DT_REG => "DT_REG",
        libc::DT_LNK => "DT_LNK",
        libc::DT_CHR => "DT_CHR",
        _ => "another kind",
    }
}

/// Whether a name under `/proc/bus/pci` is a bus: the directory of functions,
/// spelled in hex (`00`, and `0000:00` on a machine with domains).
fn path_is_a_bus(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    !name.is_empty() && name.chars().all(|c| c.is_ascii_hexdigit())
}
