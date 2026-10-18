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
//! Nor are the nodes Linux makes writable and this kernel did not claimed at
//! Linux's `0644`: `/proc/sys/kernel/pid_max`, `/proc/sys/kernel/threads-max`,
//! `/proc/sys/kernel/pty/max` and `/proc/<pid>/comm` are read-only files here,
//! so they report the read-only word. That is the file telling the truth about
//! itself - it refuses the write - and §5.2 names the difference from Linux's
//! node beside the measurement.
//!
//! `/proc/self` and `/proc/mounts` are not claimed either, and for a third
//! reason: the *kind* differs before the mode does. Linux answers a `0777` link
//! for both names, and this kernel answers a directory (a mirror of the
//! process, and a mount table) - so the mode follows from the shape, and §5.2
//! is where the shape is named.
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
    let size = statat(CWD, "/proc/cmdline", AtFlags::empty())
        .ok()
        .map(|st| st.st_size);
    let mut buf = [0u8; 512];
    let answered = std::fs::File::open("/proc/cmdline")
        .and_then(|mut file| std::io::Read::read(&mut file, &mut buf))
        .ok();
    r.check(
        "and /proc/cmdline's own size is the length of the line it answers",
        matches!((size, answered), (Some(size), Some(n)) if size > 0 && size as usize == n),
    );

    // ---- a process's own configuration space ------------------------------
    //
    // `/proc/bus/pci/<bus>/<dev>.<func>` is the live 256-byte window on a
    // device, readable and writable, and Linux makes it `0644` - the word the
    // file's own read-write shape is worth. Which bus the machine's first
    // function sits on is asked for rather than named.
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

    r.finish();
}

/// Whether a name under `/proc/bus/pci` is a bus: the directory of functions,
/// spelled in hex (`00`, and `0000:00` on a machine with domains).
fn path_is_a_bus(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    !name.is_empty() && name.chars().all(|c| c.is_ascii_hexdigit())
}
