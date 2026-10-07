//! t-mount - one test for mount(2)'s MS_BIND and umount2(2)'s side of it:
//! the second name on an already-mounted tree.
//!
//! This is the call OpenRC's boot cleanup makes (`mount --bind / /tmp/tmp.X`,
//! to test whether `/run` sits on the root), and the one whose absence was a
//! "No such device" on every boot. Pinned here: binding a directory makes its
//! contents visible under the target name, the same files are *the same*
//! storage (a write through one name reads back through the other, and both
//! stat with one `st_dev` - a bind shares the superblock, it does not mint a
//! new one), a second bind on a taken mountpoint answers EBUSY, binding a
//! file answers the EINVAL the kernel's refusal spells rather than succeeding,
//! and the umount peels the bind off the target: after it, the target's own
//! (empty) directory shows through again and the source is untouched.
//!
//! # The migration
//!
//! `rustix::mount::{mount, unmount}` is the whole door: the wrapper passes
//! exactly the five words Linux defines, and `MountFlags::BIND` names the one
//! bit this test sends. The fstype word stays `"none"` because busybox spells
//! it that way and the kernel ignores it for binds. Two host facts are named
//! where they bite. Mounting needs `CAP_SYS_ADMIN`: measured on this host as
//! an ordinary user, the bind answers EPERM (errno 1), so the host run skips
//! the section with its verdict printed rather than reporting the machine's
//! privilege as the kernel's answer - the payload always runs as root. And an
//! interrupted earlier run can leave the bind mounted; the setup peels any
//! leftover before making the directories, so "the source directory is made"
//! cannot fail as EBUSY through no fault of this boot.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, Stat, mkdirat, openat, stat, unlinkat};
use rustix::io::{Errno, read, write};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

const SRC: &str = "/tmp/t-mount_a";
const SRC_MARKER: &str = "/tmp/t-mount_a/marker";
const DST: &str = "/tmp/t-mount_b";
const DST_MARKER: &str = "/tmp/t-mount_b/marker";
const DST_THROUGH: &str = "/tmp/t-mount_b/second";
const SRC_THROUGH: &str = "/tmp/t-mount_a/second";

/// Create `path` with `contents`, failing the test if either step does not
/// answer 0 or the buffer's length.
fn touch(r: &mut Report, what: &str, path: &str, contents: &[u8]) {
    let fd = openat(
        CWD,
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
        Mode::from_raw_mode(0o644),
    );
    r.check(what, fd.is_ok());
    if let Ok(fd) = &fd {
        let n = write(fd, contents);
        r.check("the write lands in full", n == Ok(contents.len()));
    }
}

fn main() {
    println!("t-mount");
    let mut r = Report::new();

    // Start from a clean slate: a leftover bind from an earlier run would
    // turn "the bind succeeds" into EBUSY and hide everything after it.
    let _ = unmount(DST, UnmountFlags::empty());
    let _ = unlinkat(CWD, DST_MARKER, AtFlags::empty());
    let _ = unlinkat(CWD, SRC_THROUGH, AtFlags::empty());
    let _ = unlinkat(CWD, SRC_MARKER, AtFlags::empty());
    let _ = unlinkat(CWD, SRC, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, DST, AtFlags::REMOVEDIR);

    r.check(
        "the source directory is made",
        mkdirat(CWD, SRC, Mode::from_raw_mode(0o755)).is_ok(),
    );
    r.check(
        "the target directory is made",
        mkdirat(CWD, DST, Mode::from_raw_mode(0o755)).is_ok(),
    );
    touch(&mut r, "the marker file is created", SRC_MARKER, b"shared");

    // The bind itself. busybox spells the fstype `none` and the kernel
    // ignores the word - MS_BIND never consults the type table.
    if mpos_user_tests::host_run() && unsafe { libc::geteuid() } != 0 {
        println!("skip: this host user cannot mount; the payload runs as root");
        r.finish()
    }
    let bound = mount(SRC, DST, "none", MountFlags::BIND, None).is_ok();
    r.check("mount --bind succeeds", bound);
    if !bound {
        r.finish()
    }

    // The source's contents are visible under the target name.
    let fd = openat(CWD, DST_MARKER, OFlags::RDONLY, Mode::empty());
    r.check("the marker reads through the bind", fd.is_ok());
    if let Ok(fd) = &fd {
        let mut buf = [0u8; 8];
        let n = read(fd, &mut buf);
        r.check("with the bytes it carries", n == Ok(6));
        if let Ok(6) = n {
            r.check("the marker says \"shared\"", &buf[..6] == b"shared");
        }
    }
    drop(fd);

    // One storage, two names: a file created through the bind is the
    // source's, and both names answer the same st_dev - the bind shares
    // the filesystem, it does not mint a new instance.
    touch(
        &mut r,
        "a file created through the bind opens",
        DST_THROUGH,
        b"x",
    );
    let fd = openat(CWD, SRC_THROUGH, OFlags::RDONLY, Mode::empty());
    r.check("and is visible under the source name", fd.is_ok());
    drop(fd);
    let sa: Result<Stat, Errno> = stat(SRC);
    let sb: Result<Stat, Errno> = stat(DST);
    r.check("both names stat", sa.is_ok() && sb.is_ok());
    if let (Ok(a), Ok(b)) = (&sa, &sb) {
        r.check(
            "both names answer one st_dev (a bind shares the superblock)",
            a.st_dev == b.st_dev,
        );
    }

    // A second bind on the taken mountpoint: the table carries one mount
    // per point, and the answer is the same EBUSY a second mount is.
    r.check(
        "a second bind on one point answers EBUSY",
        mount(SRC, DST, "none", MountFlags::BIND, None) == Err(Errno::BUSY),
    );

    // Binding a file is refused with EINVAL, not quietly accepted: the
    // resolver's second name is for directories.
    r.check(
        "binding a file answers EINVAL",
        mount(SRC_MARKER, DST, "none", MountFlags::BIND, None) == Err(Errno::INVAL),
    );

    // The peel. After the umount the target's own - now empty - directory
    // shows through, and the source keeps its files. The unmount happens
    // before the checks that can fail below, so a red run never leaves the
    // bind mounted over the scratch tree.
    r.check(
        "umount2 of the bind succeeds",
        unmount(DST, UnmountFlags::empty()).is_ok(),
    );
    r.check(
        "the marker is gone from the target after the umount",
        matches!(
            openat(CWD, DST_MARKER, OFlags::RDONLY, Mode::empty()),
            Err(Errno::NOENT)
        ),
    );
    let fd = openat(CWD, SRC_THROUGH, OFlags::RDONLY, Mode::empty());
    r.check("and the source kept its files", fd.is_ok());
    drop(fd);

    // Cleanup: the test leaves the scratch tree as it found it.
    let _ = unlinkat(CWD, SRC_THROUGH, AtFlags::empty());
    let _ = unlinkat(CWD, SRC_MARKER, AtFlags::empty());
    let _ = unlinkat(CWD, SRC, AtFlags::REMOVEDIR);
    let _ = unlinkat(CWD, DST, AtFlags::REMOVEDIR);

    r.finish()
}
