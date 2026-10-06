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

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The file xtask writes into every exported share, under the boot's own mount
/// name and under the one this test makes.
static PROBE: &[u8] = b"/mnt/share/mpos-9p-probe\0";
static PROBE_UNDER_MOUNT: &[u8] = b"/mnt/t-9p/mpos-9p-probe\0";
/// The bytes it carries. 39 of them: see `xtask::utils::P9_PROBE`.
static PROBE_LINE: &[u8] = b"mpos reads from the host share over 9p\n";
static MOUNT_POINT: &[u8] = b"/mnt/t-9p\0";
static TAG: &[u8] = b"hostshare\0";
static MISSING_TAG: &[u8] = b"no-such-9p\0";
static FSTYPE: &[u8] = b"9p\0";

/// Read `path` whole and compare it with the line the host laid, reporting two
/// checks: the read happened, and it carried the right bytes.
fn read_probe(failed: &mut bool, what: &[u8], path: &[u8]) {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0);
    lin::check_true(failed, what, fd >= 0);
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 64];
    let n = lin::read(fd as i32, &mut buf);
    lin::close(fd as i32);
    lin::check_eq(
        failed,
        b"the probe file reads whole",
        n,
        PROBE_LINE.len() as isize,
    );
    if n != PROBE_LINE.len() as isize {
        return;
    }
    lin::check_true(
        failed,
        b"and carries the line the host laid in the share",
        &buf[..n as usize] == PROBE_LINE,
    );
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-9p\n");
    let mut failed = false;

    // The boot's own mount, at the name `steps::mount_share` gives it. This is
    // the half that fails when the probe has no caller.
    read_probe(
        &mut failed,
        b"the boot's share serves its probe file",
        PROBE,
    );

    // And the syscall path: a program's own `mount -t 9p hostshare <dir>`. A
    // second attach to the same export is the server's business, and it must
    // work - the boot's mount is one mount, not the only one.
    lin::check_eq(
        &mut failed,
        b"the mount point is made",
        lin::mkdirat(lin::AT_FDCWD, MOUNT_POINT, 0o755),
        0,
    );
    let r = lin::mount(TAG, MOUNT_POINT, FSTYPE, 0);
    lin::check_eq(
        &mut failed,
        b"mount -t 9p hostshare succeeds through the dispatch",
        r,
        0,
    );
    if r == 0 {
        read_probe(
            &mut failed,
            b"the mounted share serves the same file",
            PROBE_UNDER_MOUNT,
        );
        let _ = lin::umount2(MOUNT_POINT, 0);
    }

    // An unexported tag is refused rather than mounting something: the source
    // names a device the registry does not hold, which is ENOENT.
    let r = lin::mount(MISSING_TAG, MOUNT_POINT, FSTYPE, 0);
    lin::check_eq(
        &mut failed,
        b"an unexported tag answers ENOENT",
        r,
        lin::ENOENT,
    );

    // The test leaves the scratch tree as it found it.
    let _ = lin::unlinkat(lin::AT_FDCWD, MOUNT_POINT, lin::AT_REMOVEDIR);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-9p: panicked\n")
}
