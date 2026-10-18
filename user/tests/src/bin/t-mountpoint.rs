//! t-mountpoint - what a name a mount sits on *is*, and which doors may take it.
//!
//! A mount covers its mount point, so the name leads to the filesystem mounted
//! on it and not to the directory underneath. That much this kernel has always
//! done for the *following* walk; the doors that resolve a path without
//! following its last name - `lstat`, `statx` with `AT_SYMLINK_NOFOLLOW`,
//! `open(..., O_NOFOLLOW)` - used to look the name up in its parent and stop
//! there, so they answered the covered directory instead. Measured on a current
//! host first (`.workbuddy/probe_mountpoint_doors.c`, Linux 7.2.9): a tmpfs over
//! a `0755` directory answers **its own root** - its mode, its `st_dev` - to
//! `lstat`, `stat`, `statx` with and without `AT_SYMLINK_NOFOLLOW`,
//! `newfstatat`, `open` and `access` alike. The crossing is not something
//! `LOOKUP_NOFOLLOW` turns off: the flag is about *links*, and a mount is
//! stepped into whatever the flag says.
//!
//! The second half is the one this kernel had nowhere at all. The doors that
//! make or remove a name ask about the **name**, and Linux refuses a name a
//! mount sits on: `rmdir`, `unlink` and `rename` are `EBUSY`, and `mkdir`,
//! `mknod` and `symlink` are `EEXIST` because the name is taken. Measured, in
//! this order:
//!
//! ```text
//!                                                                        Linux
//! rmdir(mount point) / unlinkat(AT_REMOVEDIR) / rename                  EBUSY
//! mkdir / mknod / symlink at the name                                   EEXIST
//! rename(file, mount point)                                             EISDIR
//! rename(mount point, file)                                             ENOTDIR
//! rename(dir, mount point) / rename(mount point, dir)                   EBUSY
//! ```
//!
//! So the *type* question comes before the mount question (`rename(file,
//! mounted dir)` is `EISDIR`, not `EBUSY`), and the *permission* question comes
//! before it too - the last section here forks a stranger and reads four
//! answers, because that ordering is invisible to root: a caller who may not
//! write the directory holding the mount point reads `EACCES` and not `EBUSY`.
//!
//! The refusal is keyed on the **node**, not on the view. Inside a plain bind of
//! the parent there is no mount at all - a non-recursive bind clones no
//! submounts - so the reading doors there answer the covered directory again,
//! while `rmdir` of the same name is still `EBUSY` (measured, §8 of the probe:
//! this is Linux's own `d_mountpoint`, which is a fact about the dentry rather
//! than about the mount you reached it through).
//!
//! Both machines run this: `mount -t tmpfs` is served everywhere, its root is
//! `01777`, and every claim below is a word or an errno a `t-` program can be
//! its own witness for - no device, no image, no network.

use mpos_user_tests::Report;
use rustix::fs::{
    AtFlags, CWD, Mode, OFlags, Stat, StatxFlags, Timespec, Timestamps, lstat, mkdirat, openat,
    renameat, stat, statat, statx, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};
use rustix::process::geteuid;

const BASE: &str = "/tmp/t-mountpoint";
const COVERED: &str = "/tmp/t-mountpoint/d";
const ALIAS: &str = "/tmp/t-mountpoint/alias";
const PLAIN_DIR: &str = "/tmp/t-mountpoint/dr";
const PLAIN_FILE: &str = "/tmp/t-mountpoint/fr";
const MOVED: &str = "/tmp/t-mountpoint/moved";
const GUARD: &str = "/tmp/t-mountpoint/p";
const GUARD_MP: &str = "/tmp/t-mountpoint/p/m";
const GUARD_OTHER: &str = "/tmp/t-mountpoint/p/other";

/// The covered directory's own mode, and deliberately not the tmpfs root's
/// `01777`: every reading claim below is one word against the other, so the two
/// have to differ for the claim to mean anything.
const COVERED_MODE: u32 = 0o755;
const TMPFS_ROOT_MODE: u32 = 0o1777;

/// The guarded directory is `0555` - searchable, so a stranger can reach the
/// name, and unwritable, so the permission half refuses before the mount half.
const GUARD_MODE: u32 = 0o555;

/// Two stamps, for the same reason as the two modes: the covered directory is
/// given one before the mount, the mounted root another through the mount point,
/// and after the umount the covered directory still answers its own.
const COVERED_STAMP: i64 = 1_600_000_007;
const MOUNTED_STAMP: i64 = 1_700_000_007;

/// The uid the last section drops to: nobody, which owns none of this tree.
const STRANGER: u32 = 65534;

/// A node's permission bits - the low twelve of the machine's own `st_mode`,
/// which the two machines' `struct stat` carry in different places.
fn permissions(st: &Stat) -> u32 {
    Mode::from_raw_mode(st.st_mode).bits() & 0o7777
}

/// One stamp, both clocks, so the literal lives in one place.
fn stamped(sec: i64) -> Timestamps {
    Timestamps {
        last_access: Timespec {
            tv_sec: sec,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: sec,
            tv_nsec: 0,
        },
    }
}

/// How a call answered, as a word a red claim can print - so a failure says what
/// came back rather than only that the expected thing did not.
fn answer<T>(result: &Result<T, Errno>) -> String {
    match result {
        Ok(_) => String::from("ok"),
        Err(e) => format!("{e:?}"),
    }
}

/// Claim that a call refused with the errno Linux refuses with.
fn claims_refusal<T>(r: &mut Report, what: &str, got: Result<T, Errno>, want: Errno) {
    r.check(
        &format!("{what} ({})", answer(&got)),
        got.err() == Some(want),
    );
}

/// Claim that a name reports `mode`'s permission bits through the reading door
/// the claim names.
fn claims_mode(r: &mut Report, what: &str, got: Result<Stat, Errno>, mode: u32) {
    match got {
        Ok(st) => r.check(
            &format!("{what} (got {:04o})", permissions(&st)),
            permissions(&st) == mode,
        ),
        Err(e) => r.check(&format!("{what} ({e:?})"), false),
    }
}

/// Run `f` as a stranger and bring its four answers back - the same
/// fork-and-one-word-back shape `in_user_namespace` uses, with `setuid` where
/// that has `unshare`.
///
/// A *user namespace* would not do: entering one hands the caller the whole
/// capability set inside it, so the permission half would pass and the ordering
/// this exists to observe would disappear. Dropping to an ordinary uid is what
/// leaves the caller privileged over nothing.
fn as_a_stranger(f: impl FnOnce() -> [i32; 4]) -> Option<[i32; 4]> {
    let mut pipe = [0i32; 2];
    // SAFETY: a two-element array this call owns.
    if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `fork` is what this helper exists to make.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        // SAFETY: both ends are open and neither is used again.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        return None;
    }
    if pid == 0 {
        // SAFETY: the child owns the write end; the parent reads the read end
        // and nothing else touches either.
        unsafe {
            libc::close(pipe[0]);
            if libc::setgid(STRANGER) == 0 && libc::setuid(STRANGER) == 0 {
                let words: [u8; 4] = f().map(|e| e as u8);
                libc::write(pipe[1], words.as_ptr().cast(), words.len());
            }
            libc::_exit(0);
        }
    }
    // SAFETY: the parent owns the read end, and reads exactly the four words the
    // child writes; a short read means the child never dropped.
    let answer = unsafe {
        libc::close(pipe[1]);
        let mut bytes = [0u8; 4];
        let n = libc::read(pipe[0], bytes.as_mut_ptr().cast(), bytes.len());
        libc::close(pipe[0]);
        (n == 4).then(|| bytes.map(|b| b as i32))
    };
    let mut status = 0;
    // SAFETY: a child this call made, whose status nothing needs.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    answer
}

/// The raw errno a call answered, or 0 - what the stranger's child reports
/// through the pipe.
fn raw_errno<T>(result: Result<T, Errno>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(e) => e.raw_os_error(),
    }
}

fn main() {
    println!("t-mountpoint");
    let mut r = Report::new();

    // Start from a clean slate: a leftover mount from an interrupted run would
    // turn "the tmpfs mounts" into EBUSY and hide every claim after it.
    let _ = unmount(GUARD_MP, UnmountFlags::empty());
    let _ = unmount(ALIAS, UnmountFlags::empty());
    let _ = unmount(COVERED, UnmountFlags::empty());
    for path in [
        MOVED,
        GUARD_OTHER,
        PLAIN_FILE,
        ALIAS,
        GUARD_MP,
        PLAIN_DIR,
        COVERED,
    ] {
        let _ = unlinkat(CWD, path, AtFlags::empty());
        let _ = unlinkat(CWD, path, AtFlags::REMOVEDIR);
    }
    let _ = unlinkat(CWD, GUARD, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, BASE, AtFlags::REMOVEDIR);

    let made = mkdirat(CWD, BASE, Mode::from_raw_mode(0o755)).is_ok()
        && mkdirat(CWD, COVERED, Mode::from_raw_mode(COVERED_MODE)).is_ok()
        && mkdirat(CWD, ALIAS, Mode::from_raw_mode(0o755)).is_ok()
        && mkdirat(CWD, PLAIN_DIR, Mode::from_raw_mode(0o755)).is_ok()
        && openat(
            CWD,
            PLAIN_FILE,
            OFlags::CREATE | OFlags::WRONLY,
            Mode::from_raw_mode(0o644),
        )
        .is_ok();
    r.check("the scratch tree is made", made);
    if !made {
        r.finish()
    }

    // The two nodes the whole program is about, before anything covers one.
    claims_mode(
        &mut r,
        "the covered directory reports its own mode",
        lstat(COVERED),
        COVERED_MODE,
    );
    let outer_dev = match lstat(BASE) {
        Ok(st) => st.st_dev,
        Err(e) => {
            r.check(&format!("the scratch root stats ({e:?})"), false);
            r.finish()
        }
    };
    r.check(
        "the covered directory takes its own stamp",
        utimensat(CWD, COVERED, &stamped(COVERED_STAMP), AtFlags::empty()).is_ok(),
    );

    // Mounting needs `CAP_SYS_ADMIN`. A host run as an ordinary user cannot, and
    // the payload always runs as root; the verdict is printed rather than the
    // host's privilege being reported as the kernel's answer.
    if mpos_user_tests::host_run() && geteuid().as_raw() != 0 {
        println!("skip: this host user cannot mount; the payload runs as root");
        r.finish()
    }

    let mounted = mount("none", COVERED, "tmpfs", MountFlags::empty(), None).is_ok();
    r.check("a tmpfs mounts over the covered directory", mounted);
    if !mounted {
        r.finish()
    }

    // ---- the reading doors cross the mount, whatever they say about links ---
    claims_mode(
        &mut r,
        "lstat of the mount point answers the mounted root",
        lstat(COVERED),
        TMPFS_ROOT_MODE,
    );
    claims_mode(
        &mut r,
        "stat agrees with lstat about the mount point",
        stat(COVERED),
        TMPFS_ROOT_MODE,
    );
    claims_mode(
        &mut r,
        "fstatat with AT_SYMLINK_NOFOLLOW answers the mounted root",
        statat(CWD, COVERED, AtFlags::SYMLINK_NOFOLLOW),
        TMPFS_ROOT_MODE,
    );
    match statx(
        CWD,
        COVERED,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS,
    ) {
        Ok(x) => r.check(
            &format!(
                "statx with AT_SYMLINK_NOFOLLOW answers it too (got {:04o})",
                u32::from(x.stx_mode) & 0o7777
            ),
            u32::from(x.stx_mode) & 0o7777 == TMPFS_ROOT_MODE,
        ),
        Err(e) => r.check(
            &format!("statx with AT_SYMLINK_NOFOLLOW answers it too ({e:?})"),
            false,
        ),
    }
    r.check(
        "open of the mount point with O_NOFOLLOW succeeds",
        openat(
            CWD,
            COVERED,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .is_ok(),
    );
    let inner_dev = lstat(COVERED).map(|st| st.st_dev).ok();
    r.check(
        "the mount point answers the mounted filesystem's own st_dev",
        inner_dev.is_some() && inner_dev != Some(outer_dev),
    );

    // ---- the name may not be taken away ------------------------------------
    claims_refusal(
        &mut r,
        "rmdir of the mount point",
        unlinkat(CWD, COVERED, AtFlags::REMOVEDIR),
        Errno::BUSY,
    );
    claims_refusal(
        &mut r,
        "rename of the mount point",
        renameat(CWD, COVERED, CWD, MOVED),
        Errno::BUSY,
    );
    claims_refusal(
        &mut r,
        "mkdir of the mount point",
        mkdirat(CWD, COVERED, Mode::from_raw_mode(0o755)),
        Errno::EXIST,
    );
    claims_refusal(
        &mut r,
        "symlink onto the mount point",
        symlinkat("nothing", CWD, COVERED),
        Errno::EXIST,
    );

    // ---- the type question comes before the mount question -----------------
    //
    // The pair is what pins the *order*, and each half is claimed twice: over a
    // plain directory and over a mount point, so the errno cannot be the
    // mount question answering either time.
    claims_refusal(
        &mut r,
        "rename of a file onto a plain directory",
        renameat(CWD, PLAIN_FILE, CWD, PLAIN_DIR),
        Errno::ISDIR,
    );
    claims_refusal(
        &mut r,
        "rename of a plain directory onto a file",
        renameat(CWD, PLAIN_DIR, CWD, PLAIN_FILE),
        Errno::NOTDIR,
    );
    claims_refusal(
        &mut r,
        "rename of a file onto the *mounted* directory",
        renameat(CWD, PLAIN_FILE, CWD, COVERED),
        Errno::ISDIR,
    );
    claims_refusal(
        &mut r,
        "rename of a plain directory onto the *mounted* directory",
        renameat(CWD, PLAIN_DIR, CWD, COVERED),
        Errno::BUSY,
    );

    // ---- the doors that alter a mount point alter the mounted root ---------
    r.check(
        "utimensat with AT_SYMLINK_NOFOLLOW stamps through the mount point",
        utimensat(
            CWD,
            COVERED,
            &stamped(MOUNTED_STAMP),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .is_ok(),
    );
    match lstat(COVERED) {
        Ok(st) => r.check(
            &format!(
                "and lstat of the mount point reads the new stamp (got {}, want {MOUNTED_STAMP})",
                st.st_mtime
            ),
            st.st_mtime == MOUNTED_STAMP,
        ),
        Err(e) => r.check(
            &format!("and lstat of the mount point reads it ({e:?})"),
            false,
        ),
    }

    // ---- a plain bind of the parent covers the crossing up -----------------
    let bound = mount(BASE, ALIAS, "none", MountFlags::BIND, None).is_ok();
    r.check("a plain bind of the parent succeeds", bound);
    if bound {
        let aliased = format!("{ALIAS}/d");
        // The alias's own root is the *bound* directory, so the mount point's
        // name inside it is not a mount point there: `lstat` answers the covered
        // directory - its mode, and the outer filesystem's device.
        claims_mode(
            &mut r,
            "lstat through the alias answers the covered directory",
            lstat(aliased.as_str()),
            COVERED_MODE,
        );
        r.check(
            "and that answer carries the outer st_dev, not the mounted one",
            lstat(aliased.as_str()).map(|st| st.st_dev).ok() == Some(outer_dev),
        );
        claims_mode(
            &mut r,
            "while the outer spelling still answers the mounted root",
            lstat(COVERED),
            TMPFS_ROOT_MODE,
        );
        // The refusal is about the node and not the view: there is no mount in
        // this alias and the name still may not go.
        claims_refusal(
            &mut r,
            "rmdir through the alias",
            unlinkat(CWD, aliased.as_str(), AtFlags::REMOVEDIR),
            Errno::BUSY,
        );
        r.check(
            "the alias peels off",
            unmount(ALIAS, UnmountFlags::empty()).is_ok(),
        );
    }

    // ---- and the covered directory was its own node all along --------------
    r.check(
        "the umount of the mount point succeeds",
        unmount(COVERED, UnmountFlags::empty()).is_ok(),
    );
    claims_mode(
        &mut r,
        "the covered directory shows through again with its own mode",
        lstat(COVERED),
        COVERED_MODE,
    );
    r.check(
        "and its own st_dev",
        lstat(COVERED).map(|st| st.st_dev).ok() == Some(outer_dev),
    );
    match lstat(COVERED) {
        Ok(st) => r.check(
            &format!(
                "and its own stamp, untouched by the mount that covered it (got {}, want {COVERED_STAMP})",
                st.st_mtime
            ),
            st.st_mtime == COVERED_STAMP,
        ),
        Err(e) => r.check(&format!("and its own stamp ({e:?})"), false),
    }

    // ---- the permission half answers before the refusal --------------------
    //
    // The one thing root cannot observe, so a stranger does: a `0555` directory
    // holding a mount point refuses on the *permission* first, and three of the
    // four answers below are `EACCES` where root reads `EBUSY`. `mkdir` is the
    // exception that shows the order is per-door: a name that is taken answers
    // `EEXIST` before any permission is asked, for root and stranger alike.
    let guard = mkdirat(CWD, GUARD, Mode::from_raw_mode(GUARD_MODE)).is_ok()
        && mkdirat(CWD, GUARD_MP, Mode::from_raw_mode(0o755)).is_ok()
        && mount("none", GUARD_MP, "tmpfs", MountFlags::empty(), None).is_ok();
    r.check("the guarded tree is made and covered", guard);
    if guard {
        // Root reached inside a `0555` directory to make and cover that name,
        // which is the permission half's own point: the bits are the stranger's
        // obstacle and not the kernel's.
        claims_mode(
            &mut r,
            "the guarded directory is shut to strangers",
            lstat(GUARD),
            GUARD_MODE,
        );
        let answers = as_a_stranger(|| {
            [
                raw_errno(unlinkat(CWD, GUARD_MP, AtFlags::REMOVEDIR)),
                raw_errno(unlinkat(CWD, GUARD_MP, AtFlags::empty())),
                raw_errno(renameat(CWD, GUARD_MP, CWD, GUARD_OTHER)),
                raw_errno(mkdirat(CWD, GUARD_MP, Mode::from_raw_mode(0o755))),
            ]
        });
        match answers {
            Some([rmdir, unlink, rename, mkdir]) => {
                let denied = Errno::ACCESS.raw_os_error();
                let taken = Errno::EXIST.raw_os_error();
                r.check(
                    &format!(
                        "a stranger's rmdir of the mount point is EACCES, not EBUSY ({rmdir})"
                    ),
                    rmdir == denied,
                );
                r.check(
                    &format!("a stranger's unlink of it is EACCES too ({unlink})"),
                    unlink == denied,
                );
                r.check(
                    &format!("a stranger's rename of it is EACCES too ({rename})"),
                    rename == denied,
                );
                r.check(
                    &format!("while a taken name is EEXIST for a stranger as well ({mkdir})"),
                    mkdir == taken,
                );
            }
            None => r.check("a stranger can be forked and served", false),
        }
        r.check(
            "the guarded mount peels off",
            unmount(GUARD_MP, UnmountFlags::empty()).is_ok(),
        );
    }

    // Cleanup: the test leaves the scratch tree as it found it.
    let _ = unlinkat(CWD, GUARD_OTHER, AtFlags::empty());
    let _ = unlinkat(CWD, GUARD_MP, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, GUARD, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, PLAIN_FILE, AtFlags::empty());
    let _ = unlinkat(CWD, MOVED, AtFlags::empty());
    let _ = unlinkat(CWD, PLAIN_DIR, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, COVERED, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, ALIAS, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, BASE, AtFlags::REMOVEDIR);

    r.finish()
}
