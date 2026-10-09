//! t-9p - the host share: the kernel's own mount at `/mnt/share`, and the
//! `mount -t 9p` a program makes for itself.
//!
//! The boot probes the host share and mounts it (`steps::mount_share`). Until
//! §13.29 the first machine printed "no share on the bus" on *every* boot,
//! because nothing filled the device registry the mount reads - the PCI probe
//! had no caller - and the device was never reachable. What this pins is that
//! it is: the file xtask lays in the exported directory (`utils::lay_p9_share`)
//! reads back through the boot's mount, and a fresh `mount -t 9p hostshare`
//! made here reaches the same bytes over the syscall path the mount dispatch
//! serves. An unexported tag answers ENOENT rather than mounting something.
//!
//! The line the probe carries is written in three places - here, the kernel's
//! `check_p9_dev` and `xtask::utils::lay_p9_share` - and that is deliberate: a
//! fixture both sides have to agree on has copies, and the check's job is to
//! notice when they drift.
//!
//! # The migration
//!
//! `rustix::mount::mount` spells the call; the tag, fstype `"9p"` and options
//! word are exactly what the no-std harness passed. The one thing that cannot
//! travel is the expectation itself: outside the sweep's initramfs there is no
//! QEMU virtio-9p export and no boot-time attach, so on a host run every claim
//! here would be measuring the host's absence rather than this kernel's
//! presence. `host_run()` therefore skips with its verdict printed - the
//! payload boot is the only machine that can answer these lines.

use mpos_user_tests::Report;
use rustix::fs::{AtFlags, CWD, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::{Errno, read};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

/// The file xtask writes into every exported share, under the boot's own mount
/// name and under the one this test makes.
const PROBE: &str = "/mnt/share/mpos-9p-probe";
const PROBE_UNDER_MOUNT: &str = "/mnt/t-9p/mpos-9p-probe";
/// The bytes it carries. 39 of them: see `xtask::utils::P9_PROBE`.
const PROBE_LINE: &[u8] = b"mpos reads from the host share over 9p\n";
const MOUNT_POINT: &str = "/mnt/t-9p";
const TAG: &str = "hostshare";
const MISSING_TAG: &str = "no-such-9p";
const FSTYPE: &str = "9p";

/// Read `path` whole and compare it with the line the host laid, reporting two
/// checks: the read happened, and it carried the right bytes.
fn read_probe(r: &mut Report, what: &str, path: &str) {
    let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty());
    r.check(what, fd.is_ok());
    if let Ok(fd) = &fd {
        let mut buf = [0u8; 64];
        let n = read(fd, &mut buf);
        r.check(
            "the probe file reads whole",
            n.is_ok_and(|n| n == PROBE_LINE.len()),
        );
        if let Ok(n) = n
            && n == PROBE_LINE.len()
        {
            r.check(
                "and carries the line the host laid in the share",
                &buf[..n] == PROBE_LINE,
            );
        }
    }
}

fn main() {
    println!("t-9p");
    let mut r = Report::new();

    // The share is the sweep's QEMU wiring, not a host device: outside the
    // test boot there is no virtio-9p export and no `steps::mount_share`, so a
    // host run would measure the host's absence rather than this kernel's
    // presence. Skip with the verdict printed; the payload boot answers here.
    if mpos_user_tests::host_run() {
        println!("skip: the 9p share exists only in the test boot");
        r.finish()
    }

    // The boot's own mount, at the name `steps::mount_share` gives it. This is
    // the half that fails when the probe has no caller. On the second machine
    // the name is served by the init script's own mount of the same export (the
    // kernel's hangs on the ramfs the switch deletes), so what this pins is
    // that *the boot* served the share there - either half is the boot's.
    read_probe(&mut r, "the boot's share serves its probe file", PROBE);

    // And the syscall path: a program's own `mount -t 9p hostshare <dir>`. A
    // second attach to the same export is the server's business, and it must
    // work - the boot's mount is one mount, not the only one.
    let made = mkdirat(CWD, MOUNT_POINT, Mode::from_raw_mode(0o755));
    r.check(
        "the mount point is made",
        made.is_ok() || made == Err(Errno::EXIST),
    );
    let mounted = mount(TAG, MOUNT_POINT, FSTYPE, MountFlags::empty(), None);
    r.check(
        "mount -t 9p hostshare succeeds through the dispatch",
        mounted.is_ok(),
    );
    if mounted.is_ok() {
        read_probe(
            &mut r,
            "the mounted share serves the same file",
            PROBE_UNDER_MOUNT,
        );
        let _ = unmount(MOUNT_POINT, UnmountFlags::empty());
    }

    // An unexported tag is refused rather than mounting something: the source
    // names a device the registry does not hold, which is ENOENT.
    r.check(
        "an unexported tag answers ENOENT",
        mount(MISSING_TAG, MOUNT_POINT, FSTYPE, MountFlags::empty(), None) == Err(Errno::NOENT),
    );

    // The test leaves the scratch tree as it found it.
    let _ = unlinkat(CWD, MOUNT_POINT, AtFlags::REMOVEDIR);

    r.finish()
}
