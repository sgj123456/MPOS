//! t-devtmpfs - a node under `/dev` keeps the clocks it is given.
//!
//! `utimensat`'s path-taking shapes all end in one place on the node:
//! `Inode::set_times`. A filesystem that does not implement it answers the
//! trait's default, `EOPNOTSUPP` - and that was every node under `/dev`,
//! because a devtmpfs node here kept a mode and an owner and nothing else.
//! Nothing in a boot *failed* over it. What it did was put one line on the wire
//! (`mpos: syscall 88 (fd -100) -> EOPNOTSUPP` on aarch64, printed once per
//! boot by the dispatcher's note table) and send its caller down a path Linux
//! never sends it down, which is the shape `docs/UAPI.md` §5.2 recorded:
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

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_OMIT, stat, utimensat};

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

    r.finish()
}
