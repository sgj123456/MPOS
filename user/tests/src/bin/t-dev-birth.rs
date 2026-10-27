//! t-dev-birth - the mode and owner a `/dev` node is *born* with.
//!
//! A `stat` of any `/dev` node in an Alpine boot shows the number that udev's
//! rules wrote, not the number devtmpfs created the node with - and those are
//! two different things. Linux's devtmpfs births a node `0600 root:root` and
//! leaves the widening to udev; the memory class's stream nodes and the two
//! tty-aux aliases are the classes devtmpfs itself writes wide. This kernel
//! birthed *every* class `0666 root:root` until the table in
//! `src/vfs/devfs.rs` (`birth_mode`) existed, which meant a boot with no udev
//! running - a payload sweep's, an initramfs's before its rules - served
//! callers a wider tree than Linux ever does. The claims below are that table,
//! node by node, on the arm that has no udev at all; they are exactly the
//! program that fails against the old number.
//!
//! The measured table these numbers come from: a Linux 7.2.11 boot with no
//! udev (a static init that mounted devtmpfs and stat'ed every node), plus two
//! probes on a host whose udev exec queue was frozen while `null_blk`, `vkms`,
//! `vgem` and `uinput` loaded. Both are written down in `docs/notes/
//! 03-filesystem.md` §3.4, with the image's after-udev table beside them.
//! `0666` = `/dev/null` `zero` `full` `random` `urandom`, `/dev/tty` (5:0),
//! `/dev/ptmx` (5:2); `0644` = `/dev/kmsg`; `0600` = everything else, the DRM
//! render node included (a rule widens it; devtmpfs does not birth it wide).
//!
//! Two deliberate silences. `/dev/pts/N` is not claimed: that is devpts's
//! number, which comes from devpts's mount options (Linux's default
//! `mode=0620`) and not from devtmpfs's birth - this kernel does not implement
//! devpts options yet, and folding that gap into a birth claim would hide it.
//! And a node the machine does not have is not a claim either: `dri`,
//! `fb0`, `input`, `uinput`, `hwrng`, `hvc`, `dsp` and the disks exist only on
//! the machines whose drivers came up, so each is claimed when present and
//! named absent otherwise.
//!
//! On a host run (`MPOS_ON_HOST=1`) the numbers read back are udev's, so only
//! the classes devtmpfs births wide are claimed - everything else is printed
//! as the host's own answer, because udev's output depends on the udev version
//! and measuring *that* is the image boot's job, not this program's.

use mpos_user_tests::{Report, host_run};
use rustix::fs::stat;
use rustix::io::Errno;

/// One node's claim: it is born `want` and owned by root.
///
/// The actual numbers go into the line even when the claim holds - the log then
/// *is* the table this boot measured, which is what a reader comparing it with
/// the note needs.
fn claim(r: &mut Report, path: &str, want: u32) {
    match stat(path) {
        Ok(st) => {
            let got = st.st_mode & 0o7777;
            r.check(
                &format!(
                    "{path} is born {want:04o} root:root (is {got:04o} {}:{})",
                    st.st_uid, st.st_gid
                ),
                got == want && st.st_uid == 0 && st.st_gid == 0,
            );
        }
        Err(e) => r.check(
            &format!("{path} is born {want:04o} root:root (stat: {e:?})"),
            false,
        ),
    }
}

/// The same, for a class only some machines carry: absent is a named skip, not
/// a failed claim - but an absent node that should be there still has to look
/// like absence rather than a permission error, so a stat failure other than
/// `ENOENT` is a failure.
fn claim_if_present(r: &mut Report, path: &str, want: u32) {
    match stat(path) {
        Ok(st) => {
            let got = st.st_mode & 0o7777;
            r.check(
                &format!(
                    "{path} is born {want:04o} root:root (is {got:04o} {}:{})",
                    st.st_uid, st.st_gid
                ),
                got == want && st.st_uid == 0 && st.st_gid == 0,
            );
        }
        Err(Errno::NOENT) => println!("skip: {path} is not on this machine"),
        Err(e) => r.check(&format!("{path} stats when present ({e:?})"), false),
    }
}

/// Claim the first of `paths` this machine has - the serial port's name is the
/// machine's (`ttyS0` on x86_64, `ttyAMA0` on aarch64), and the disks' names
/// come from whichever block driver came up.
fn claim_first(r: &mut Report, paths: &[&str], want: u32) {
    let Some(path) = paths.iter().find(|p| stat(**p).is_ok()) else {
        println!("skip: none of {paths:?} is on this machine");
        return;
    };
    claim(r, path, want);
}

fn main() {
    let mut r = Report::new();

    if host_run() {
        // udev's numbers, not devtmpfs's birth: the wide classes are the ones
        // that still say something here, because devtmpfs writes those itself
        // and no rule narrows them back.
        for path in [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty",
            "/dev/ptmx",
        ] {
            match stat(path) {
                Ok(st) => r.check(
                    &format!("{path} is 0666 on the host too"),
                    st.st_mode & 0o7777 == 0o666,
                ),
                Err(e) => r.check(&format!("{path} stats on the host ({e:?})"), false),
            }
        }
        // The rest is printed, not claimed: it is the host's udev answering.
        for path in [
            "/dev/kmsg",
            "/dev/console",
            "/dev/tty0",
            "/dev/tty1",
            "/dev/dri/card0",
            "/dev/dri/renderD128",
            "/dev/fb0",
            "/dev/input/event0",
            "/dev/uinput",
            "/dev/rtc0",
            "/dev/hwrng",
        ] {
            if let Ok(st) = stat(path) {
                println!(
                    "host: {path} {:04o} {}:{} (post-udev; not a birth claim)",
                    st.st_mode & 0o7777,
                    st.st_uid,
                    st.st_gid
                );
            }
        }
        r.finish();
    }

    // The wide births: the classes a boot with no udev must still serve, which
    // is the half that has to survive a missing rule.
    for path in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
    ] {
        claim(&mut r, path, 0o666);
    }

    // The one measured class that is neither wide nor strict.
    claim(&mut r, "/dev/kmsg", 0o644);

    // The strict births - the half this kernel used to hand out at 0666.
    for path in ["/dev/console", "/dev/tty0", "/dev/tty1", "/dev/rtc0"] {
        claim(&mut r, path, 0o600);
    }
    claim_first(&mut r, &["/dev/ttyS0", "/dev/ttyAMA0"], 0o600);

    // Classes only some machines carry: a display driver, an input bus, the
    // hardware RNG, a virtio console, a sound device, a disk. Absence is named
    // rather than claimed, because a class a machine has no driver for is not
    // a birth this boot can measure.
    for path in [
        "/dev/dri/card0",
        "/dev/dri/renderD128",
        "/dev/fb0",
        "/dev/input/event0",
        "/dev/uinput",
        "/dev/hwrng",
        "/dev/hvc0",
        "/dev/hvc",
        "/dev/dsp",
        "/dev/rtc",
    ] {
        claim_if_present(&mut r, path, 0o600);
    }

    // The block kind was `0660` here, and the old comment claimed that was
    // devtmpfs's number. The frozen-queue probe's `/dev/nullb0` refutes it: a
    // block node is born `0600` like the rest, and udev's `GROUP=disk` rule
    // (which names no `MODE`) is what hands it on.
    claim_first(
        &mut r,
        &[
            "/dev/vda",
            "/dev/vda1",
            "/dev/vda2",
            "/dev/sda",
            "/dev/sda1",
        ],
        0o600,
    );

    r.finish();
}
