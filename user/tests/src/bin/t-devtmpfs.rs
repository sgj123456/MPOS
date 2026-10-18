//! t-devtmpfs - what the device tree serves: a node under `/dev` keeps the
//! clocks it is given, and a directory made under `/dev` comes back out.
//!
//! `utimensat`'s path-taking shapes all end in one place on the node:
//! `Inode::set_times`. A filesystem that does not implement it answers the
//! trait's default, `EOPNOTSUPP` - and that was every node under `/dev`,
//! because a devtmpfs node here kept a mode and an owner and nothing else.
//! Nothing in a boot *failed* over it. What it did was put one line on the wire
//! (`mpos: syscall 88 (fd -100) -> EOPNOTSUPP` on aarch64, printed once per
//! boot by the dispatcher's note table) and send its caller down a path Linux
//! never sends it down, which is the shape `docs/uapi/` §5.2 recorded:
//! eudev's node path sets the timestamps of the device nodes it manages - the
//! ones a boot here touched were the disks - and `touch -d @1600000000
//! /dev/zero` answers `0` on a host whose `/dev` *is* devtmpfs, with `stat`
//! reading the stamp back.
//!
//! So the claims below are that a `/dev` node takes a stamp and reports it back
//! through `stat`, on each shape this filesystem carries: a node wrapped around
//! a driver (`/dev/null`), a node whose inode *is* the device (`/dev/tty0`, the
//! alias whose attributes are the one shared record every open of the name
//! reads), and a **block** node (`/dev/sda` or `/dev/vda1`, depending on what
//! the launcher wired up). The block kind is here because the boot that had only
//! the first two named *it*: the one refusal left once the character nodes kept
//! their clocks was `BlockDev, rdev 254:1`. The two clocks are pinned apart from
//! each other, because they are independent words in the ABI - `touch -a` sends
//! an atime with `UTIME_OMIT` in the other slot - and a node that folded them
//! into one would answer the other's number back.
//!
//! What is deliberately *not* claimed is a node's time of birth. Linux's
//! devtmpfs stamps a node when it is created; this filesystem has no mount-time
//! number to hand a node that arrives when its driver registers it, so a node
//! nobody has stamped reports zero - the trait's own answer for a filesystem
//! that keeps nothing, and what `/dev` already reported - rather than a
//! plausible time. The *directories* under `/dev` are claimed too, now: the
//! refusal a desktop still showed landed in the pass where udev lays
//! directories and names symlinks, so the node the filesystem *is* (`/dev`
//! itself) and a directory under it (`/dev/dri`, where a display driver
//! registered) take the same stamp a device node does (`NOTES.md` §15.35).
//! A symlink is not claimed here: `stat` follows one, so the path-taking door
//! this test uses would stamp the target and not the link - the host suite's
//! `path_named_nodes_keep_the_clocks_they_are_given` pins `DevSymlinkNode`
//! directly instead.
//!
//! The filesystem's other half is its *removal* pair, and the same program
//! claims it because the claims are about this filesystem rather than about a
//! syscall: `mkdir` of a name under `/dev` and `rmdir` of it back are the two
//! calls udev runs over `/dev/dri/by-path` at every boot, and the second half
//! answered `EOPNOTSUPP` here (`mpos: syscall 84 (path /dev/dri/by-path) ->
//! EOPNOTSUPP` on the wire) until the door existed - which left that directory
//! reachable only if the kernel itself had published it. The words the section
//! below claims are a host's, measured as root on a machine whose `/dev` *is* a
//! devtmpfs (`.workbuddy/probe_devfs_doors.c`, Linux 7.2.9): `mkdir` and
//! `rmdir` of one name both answer 0, a directory that still holds something is
//! `ENOTEMPTY`, a device node is `ENOTDIR` from this door and served by the
//! other, and `unlink` of a directory is `EISDIR`.

use mpos_user_tests::Report;
use rustix::fs::{
    AtFlags, CWD, Mode, Timespec, Timestamps, UTIME_OMIT, mkdirat, stat, unlinkat, utimensat,
};
use rustix::io::Errno;

/// The clock the test sets. Far from any boot time, so a node that answered
/// "now" - or the epoch - cannot pass as this value.
///
/// `MTIME` is deliberately *not* `ATIME + 1`: the `touch -a` half below sets
/// the atime to `ATIME + 1` while asking the mtime to stay, so if the two
/// numbers collided the "the mtime did not move" line would compare a value
/// against itself and pass on a node that moved the wrong clock.
const ATIME: i64 = 1_600_000_001;
const MTIME: i64 = 1_600_000_007;

/// The block nodes a boot of either machine publishes, in the order this test
/// asks for them: the SATA disk the sweep attaches on both (major 8), then this
/// machine's own virtio disk and its partitions (major 254). Which of them QEMU
/// wired up is the launcher's business and not this claim's - the *kind* is what
/// is pinned - so the first that `stat`s is the one claimed, and its name is
/// printed with the result.
const BLOCK_NODES: [&str; 4] = ["/dev/sda", "/dev/vda2", "/dev/vda1", "/dev/vda"];

/// The names the removal section makes for itself, and one nothing ever makes.
///
/// Under `/dev` rather than under `/tmp`, which is where the rest of the suite
/// keeps its scratch: what these claims are about is the *filesystem* - a tmpfs
/// has no `create` for a `rmdir` to be the other half of, and no device nodes
/// to be `ENOTDIR` about.
const DIR: &str = "/dev/t-devtmpfs_d";
const INNER: &str = "/dev/t-devtmpfs_d/inner";
/// The same test's directory inside one a display driver published - the nested
/// door udev's by-path pass lands on.
const NESTED: &str = "/dev/dri/t-devtmpfs_d";
/// A name nothing has made.
const GONE: &str = "/dev/t-devtmpfs_gone";
/// One user, no permission bits to exercise: the mode is a formality here, as
/// it is in `t-mkdirat`.
const DIR_MODE: Mode = Mode::from_bits_truncate(0o755);

/// Both clocks, as a caller that means them both sends them.
fn both(atime: i64, mtime: i64) -> Timestamps {
    Timestamps {
        last_access: Timespec {
            tv_sec: atime,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: mtime,
            tv_nsec: 0,
        },
    }
}

/// One node's whole claim: it takes a stamp, `stat` reads both words back, and
/// the two words move independently.
fn claim(r: &mut Report, path: &str) {
    let set = utimensat(CWD, path, &both(ATIME, MTIME), AtFlags::empty());
    r.check(&format!("{path} takes a stamp ({set:?})"), set.is_ok());

    match stat(path) {
        Ok(st) => {
            r.check(
                &format!(
                    "... and stat reads the atime back ({}, set to {ATIME})",
                    st.st_atime
                ),
                st.st_atime == ATIME,
            );
            r.check(
                &format!("... and the mtime ({}, set to {MTIME})", st.st_mtime),
                st.st_mtime == MTIME,
            );
        }
        Err(e) => r.check(&format!("{path} stats ({e})"), false),
    }

    // `touch -a`: the atime moves, the mtime is `UTIME_OMIT`. The syscall
    // resolves that sentinel against what the node reports, so a node that
    // kept one clock and handed it to both slots fails here rather than
    // passing quietly on a stamp the caller never asked to change.
    let atime_only = Timestamps {
        last_access: Timespec {
            tv_sec: ATIME + 1,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
    };
    let set = utimensat(CWD, path, &atime_only, AtFlags::empty());
    r.check(
        &format!("{path} takes an atime with the mtime omitted ({set:?})"),
        set.is_ok(),
    );
    match stat(path) {
        Ok(st) => {
            r.check(
                &format!(
                    "... the atime moved ({}, set to {})",
                    st.st_atime,
                    ATIME + 1
                ),
                st.st_atime == ATIME + 1,
            );
            r.check(
                &format!("... and the mtime did not ({}, still {MTIME})", st.st_mtime),
                st.st_mtime == MTIME,
            );
        }
        Err(e) => r.check(&format!("{path} stats again ({e})"), false),
    }
}

/// The two removal doors, on names this test makes and removes itself.
///
/// The pair is one idiom - `mkdir` a name under `/dev`, then `rmdir` it back -
/// and it is what udev's by-path pass runs at every boot. `rmdir` answered
/// `EOPNOTSUPP` here until the filesystem's directory node gained the door, and
/// so did a `cat` on a directory's earlier gap; the words around it are a
/// host's, measured as root on a machine whose `/dev` *is* a devtmpfs
/// (`.workbuddy/probe_devfs_doors.c`, Linux 7.2.9).
fn removal(r: &mut Report) {
    // A first-run claim should be about the call rather than about the last
    // run's leftovers, and this program may be run twice on one machine.
    let _ = unlinkat(CWD, DIR, AtFlags::REMOVEDIR);

    let made = mkdirat(CWD, DIR, DIR_MODE);
    r.check(
        &format!("mkdir of a name under /dev is served ({made:?})"),
        made.is_ok(),
    );
    r.check(
        "mkdir of the same name again is EEXIST",
        mkdirat(CWD, DIR, DIR_MODE) == Err(Errno::EXIST),
    );

    // The door that is *not* for directories refuses the kind, and the refusal
    // takes nothing away - which is what makes the two doors a split rather
    // than two spellings of one removal.
    r.check(
        "unlink of a directory is EISDIR",
        unlinkat(CWD, DIR, AtFlags::empty()) == Err(Errno::ISDIR),
    );
    r.check("... and the refused name is still there", stat(DIR).is_ok());

    // A directory that still holds something: ENOTEMPTY, and again nothing
    // moves - which is the whole reason `rmdir` is not a recursive delete.
    r.check(
        "mkdir of a child inside it is served",
        mkdirat(CWD, INNER, DIR_MODE).is_ok(),
    );
    r.check(
        "rmdir of a directory that still holds something is ENOTEMPTY",
        unlinkat(CWD, DIR, AtFlags::REMOVEDIR) == Err(Errno::NOTEMPTY),
    );
    r.check(
        "... and that refusal left it where it was",
        stat(DIR).is_ok(),
    );

    // The idiom's second half, from the inside out.
    r.check(
        "rmdir of the empty child is served",
        unlinkat(CWD, INNER, AtFlags::REMOVEDIR).is_ok(),
    );
    r.check(
        "and then rmdir of the directory itself is served",
        unlinkat(CWD, DIR, AtFlags::REMOVEDIR).is_ok(),
    );
    r.check("so the name it removed is gone", stat(DIR).is_err());

    // The kinds that are not directories, and a name nothing ever made.
    r.check(
        "rmdir of a device node is ENOTDIR",
        unlinkat(CWD, "/dev/null", AtFlags::REMOVEDIR) == Err(Errno::NOTDIR),
    );
    r.check(
        "rmdir of a name nothing made is ENOENT",
        unlinkat(CWD, GONE, AtFlags::REMOVEDIR) == Err(Errno::NOENT),
    );

    // The nested door, which is where the pass that found this actually lands:
    // `/dev/dri` is a directory the kernel published, and udev removes
    // `by-path` inside it. Present only where a display driver registered, so
    // its absence is noted rather than failed - the root's own door above is
    // `DevRoot`'s, and this one is `DevDirNode`'s.
    if stat("/dev/dri").is_ok() {
        let made = mkdirat(CWD, NESTED, DIR_MODE);
        r.check(
            &format!("mkdir inside a directory the kernel published ({made:?})"),
            made.is_ok(),
        );
        let taken = unlinkat(CWD, NESTED, AtFlags::REMOVEDIR);
        r.check(
            &format!("and rmdir of it, the pass udev runs ({taken:?})"),
            taken.is_ok(),
        );
    } else {
        println!("  (no /dev/dri on this machine, so the nested door is not claimed)");
    }
}

fn main() {
    println!("t-devtmpfs");
    let mut r = Report::new();

    // A node the filesystem made around a driver: `DevNode`, whose mode and
    // owner already stuck, which is what made the clocks' absence a
    // disagreement between two attributes of one node.
    claim(&mut r, "/dev/null");
    // A node that is its own inode, with the shared record every open of the
    // name reads - the alias path, and a second implementation of the trait.
    claim(&mut r, "/dev/tty0");
    // The block kind, which the boot that had only the two above named: the
    // disk nodes are what udev's node path set the timestamps of.
    let mut block: Option<&str> = None;
    for path in BLOCK_NODES {
        if stat(path).is_ok() {
            block = Some(path);
            break;
        }
    }
    match block {
        Some(path) => claim(&mut r, path),
        None => r.check(&format!("one of {BLOCK_NODES:?} exists to claim"), false),
    }

    // The node the filesystem *is*: `/dev` itself, a directory whose inode is
    // `DevRoot`. Always here, and the inode the refusal a desktop still showed
    // reached once the block nodes kept their clocks (`NOTES.md` §15.35).
    claim(&mut r, "/dev");
    // A directory udev/mdev lays under `/dev` for the GPU. Present only where a
    // display driver registered, so its absence is noted rather than failed -
    // the *kind* is what is claimed, and `/dev` above already pins `DevDirNode`
    // on a machine that has no `/dev/dri`.
    match stat("/dev/dri") {
        Ok(_) => claim(&mut r, "/dev/dri"),
        Err(_) => println!("  (no /dev/dri on this machine)"),
    }

    // The filesystem's other half: the two removal doors, on names this test
    // makes and removes itself.
    removal(&mut r);

    r.finish()
}
