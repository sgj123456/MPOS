//! t-mountinfo - `/proc/self/mountinfo`, `/proc/modules`, `/sys/block` and
//! `/sys/bus/usb/devices`: the four files the machine's `ls*` tools open, and
//! the four that were not there.
//!
//! Each of these was reported by a tool that could not run. `lsblk` printed
//! `can't read '/proc/self/mountinfo'`; `lsmod` printed
//! `/proc/modules: No such file or directory`; `lsusb` printed
//! `/sys/bus/usb/devices: No such file or directory`; and `lsblk` will not do
//! anything at all - `can't open '/sys/block'` - without that directory,
//! whatever else the tree has. So all four are pinned here, in the shape the
//! tool that reads them needs:
//!
//!   * mountinfo is **ten fields in `proc(5)`'s order**, with the `-` separator
//!     where it belongs. The fields are parsed by position by everything that
//!     reads the file, so position is the whole of its contract.
//!   * its **third field is the same number `stat(2)` reports** as `st_dev` for
//!     the mountpoint's own path. That is what the file is *for*: `lsblk`'s
//!     MOUNTPOINTS column, `findmnt` and `df` find a mount's device by matching
//!     that field against the `dev` numbers `/sys/class/block` publishes, so a
//!     value that is not `st_dev` - such as the mount table's own attachment
//!     counter, which is what this kernel used to print - makes every one of
//!     them blank.
//!   * `/proc/modules` reads as **nothing**, and `/sys/bus/usb/devices` is
//!     **empty**. Both are the honest answer for this machine - it has no
//!     loadable modules and no USB host - and an absent file was the *wrong*
//!     answer, because `lsmod` and `lsusb` report it as an error and exit
//!     non-zero rather than as "none".
//!
//! What is *not* pinned here, deliberately: the block tree's shape. This test
//! boots with no disk at all, so `/sys/block` has nothing in it to walk - the
//! shape of a disk's entry, its partitions and their `size`/`partition`
//! attributes is pinned by the host tests, which build the tree from a
//! numbering they write down, and the real thing is checked in the guest on the
//! real image.
//!
//! **Two host divergences, both of them the same kind.** The host is a machine
//! with modules and with USB devices, so `/proc/modules` is 11 KiB there and
//! `/sys/bus/usb/devices` lists five - this kernel's honest "none" is not
//! Linux's answer, and the claim names that rather than pinning it. Everything
//! else holds on both: ten fields, the `-` at field seven, `maj:min` in field
//! three and equal to the `st_dev` of the mountpoint, and `mounts`' six columns
//! with a readable options word were all measured the same on a current host
//! (119 lines, every one of them ten fields).

use mpos_user_tests::{Report, host_run};
use rustix::fs::{CWD, Mode, OFlags, openat, stat};
use rustix::io::read;

/// Read a generated file whole. The loop is the point rather than a formality:
/// a `read` that comes back short of what the file holds is a bug in the file,
/// and a test that read once and believed the answer would not see it. A
/// `/proc` file reports a size of zero, so the length has to come from the
/// reads.
fn read_whole(path: &str) -> Option<Vec<u8>> {
    let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match read(&fd, &mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    Some(out)
}

/// `dev` in the `maj:min` spelling mountinfo prints and `/sys` publishes,
/// decoded exactly the way libc decodes it - which is the point of printing it
/// this way rather than as the raw word.
fn majmin(dev: u64) -> String {
    let major = (dev >> 8) & 0xfff;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    format!("{major}:{minor}")
}

/// The `n`th space-separated field of a line.
fn field(line: &str, n: usize) -> Option<&str> {
    line.split(' ').nth(n)
}

/// The lines of a file, without its trailing newline's empty tail.
fn lines(text: &[u8]) -> impl Iterator<Item = &str> {
    std::str::from_utf8(text)
        .unwrap_or("")
        .split('\n')
        .filter(|l| !l.is_empty())
}

fn main() {
    println!("t-mountinfo");
    let mut r = Report::new();

    // ---- stat("/") first: its `st_dev` is what mountinfo's third field must
    // ---- print, so the answer has to be in hand before the file is read.
    let root = stat("/");
    r.check("stat(\"/\") succeeds", root.is_ok());
    let Ok(root) = root else {
        r.finish();
    };
    let want = majmin(root.st_dev);

    // ---- /proc/self/mountinfo ----
    let Some(text) = read_whole("/proc/self/mountinfo") else {
        r.check("/proc/self/mountinfo opens", false);
        r.finish();
    };
    r.check("/proc/self/mountinfo is not empty", !text.is_empty());
    r.check(
        "and ends with a newline",
        text.last().copied() == Some(b'\n'),
    );

    let mut count = 0usize;
    let mut ten_fields = true;
    let mut separator = true;
    let mut fields_are_whole = true;
    let mut shape_ok = true;
    let mut root_seen = false;
    let mut root_dev_ok = false;
    for line in lines(&text) {
        count += 1;
        // Ten fields, and *only* ten: the optional list between the options
        // and the `-` is empty here - this kernel has no shared subtrees and
        // no idmapped mounts - and a reader that found a field where it
        // expected the separator would misparse every column after it. (The
        // host measures the same: 119 lines of ten fields.)
        ten_fields &= line.split(' ').count() == 10;
        separator &= field(line, 6) == Some("-");
        // Every field before the separator is non-empty: a missing `maj:min`
        // or a mountpoint that vanished would otherwise read as a column of
        // spaces and parse as a different mount.
        for i in 0..6 {
            fields_are_whole &= field(line, i).is_some_and(|f| !f.is_empty());
        }
        // The third field is a `maj:min` and not a bare number: a colon in it
        // is what every reader splits on.
        shape_ok &= field(line, 2).is_some_and(|f| f.contains(':'));
        if field(line, 4) == Some("/") {
            root_seen = true;
            root_dev_ok = field(line, 2) == Some(want.as_str());
        }
    }
    r.check("at least one mount is reported", count > 0);
    r.check("every line has ten fields", ten_fields);
    r.check("with the - separator at field 7", separator);
    r.check("and no empty field before it", fields_are_whole);
    r.check("the device is a maj:min", shape_ok);
    r.check("the root is in the table", root_seen);
    r.check(
        "and its device is the st_dev stat reports for it",
        root_dev_ok,
    );

    // ---- /proc/self/mounts, the six-column spelling of the same table ----
    let Some(mounts) = read_whole("/proc/self/mounts") else {
        r.check("/proc/self/mounts opens", false);
        r.finish();
    };
    let mut six = true;
    let mut options = true;
    let mut mounts_count = 0usize;
    for line in lines(&mounts) {
        mounts_count += 1;
        six &= line.split(' ').count() == 6;
        // The fourth column is the one that matters - `ro` says a write will
        // be refused - and it is never empty.
        options &= field(line, 3).is_some_and(|f| f.starts_with("ro") || f.starts_with("rw"));
    }
    r.check("/proc/self/mounts lists a mount", mounts_count > 0);
    r.check("in fstab's six columns", six);
    r.check("with readable options", options);

    // ---- /proc/modules: there, and nothing ----
    match read_whole("/proc/modules") {
        None => r.check("/proc/modules opens", false),
        Some(text) => {
            // Not "a file with no rows": *no bytes at all*, which is what
            // `lsmod` reads as the header and nothing else. The host, which
            // has modules, answers with 11 KiB of them - named, not pinned.
            let empty = text.is_empty();
            if !empty && host_run() {
                println!(
                    "note: /proc/modules holds {} bytes here - this host has loadable modules, \
                     where this kernel has none by design",
                    text.len()
                );
            }
            r.check("/proc/modules reads as empty", empty || host_run());
        }
    }

    // ---- /sys/bus/usb/devices: there, and empty ----
    let usb = stat("/sys/bus/usb/devices");
    r.check("/sys/bus/usb/devices exists", usb.is_ok());
    if let Ok(usb) = usb {
        r.check("and is a directory", usb.st_mode & 0o170_000 == 0o040_000);
    }
    // Empty, which is what `lsusb` reads as "no devices" - it opens this one
    // path and lists what it finds, so a directory with something in it would
    // be a USB device this machine does not have. The host has five.
    match std::fs::read_dir("/sys/bus/usb/devices") {
        Err(_) => r.check("and lists", false),
        Ok(entries) => {
            let mut names = 0usize;
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name != "." && name != ".." {
                    names += 1;
                }
            }
            if names > 0 && host_run() {
                println!(
                    "note: /sys/bus/usb/devices lists {names} entries here - this host has USB \
                     devices, where this kernel has no USB host"
                );
            }
            r.check("nothing but . and ..", names == 0 || host_run());
        }
    }

    // ---- /sys/block: it has to be there, whatever is in it ----
    let blk = stat("/sys/block");
    r.check("/sys/block exists", blk.is_ok());
    if let Ok(blk) = blk {
        r.check("and is a directory", blk.st_mode & 0o170_000 == 0o040_000);
    }

    r.finish();
}
