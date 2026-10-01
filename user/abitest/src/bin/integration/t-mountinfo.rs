//! t-mountinfo - `/proc/self/mountinfo`, `/proc/modules`, `/sys/block` and
//! `/sys/bus/usb/devices`: the four files the machine's `ls*` tools open, and
//! the four that were not there.
//!
//! Each of these was reported by a tool that could not run. `lsblk` printed
//! `can't read '/proc/self/mountinfo'`; `lsmod` printed
//! `/proc/modules: No such file or directory`; `lsusb` printed
//! `/sys/bus/usb/devices: No such file or directory`; and `lsblk` will not do
//! anything at all - `can't open '/sys/block'` - without that directory, whatever
//! else the tree has. So all four are pinned here, in the shape the tool that
//! reads them needs:
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
//!   * `/proc/modules` exists and reads as **nothing**, and `/sys/bus/usb/devices`
//!     exists and is **empty**. Both are the honest answer for this machine -
//!     it has no loadable modules and no USB host - and an absent file was the
//!     *wrong* answer, because `lsmod` and `lsusb` report it as an error and exit
//!     non-zero rather than as "none".
//!
//! What is *not* pinned here, deliberately: the block tree's shape. This test
//! boots with no disk at all, so `/sys/block` has nothing in it to walk - the
//! shape of a disk's entry, its partitions and their `size`/`partition`
//! attributes is pinned by the host tests, which build the tree from a numbering
//! they write down, and the real thing is checked in the guest on the real
//! image.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOUNTINFO: &[u8] = b"/proc/self/mountinfo\0";
static MOUNTS: &[u8] = b"/proc/self/mounts\0";
static MODULES: &[u8] = b"/proc/modules\0";
static USB: &[u8] = b"/sys/bus/usb/devices\0";
static SYSBLOCK: &[u8] = b"/sys/block\0";
static ROOT: &[u8] = b"/\0";

/// Sizes as constants rather than `BUF.len()`: reading the length of a mutable
/// static is a shared reference to it, which the 2024 rules forbid - and the
/// existing tests in this crate spell their buffers the same way.
const BUF_LEN: usize = 8192;
const SMALL_LEN: usize = 256;
static mut BUF: [u8; BUF_LEN] = [0; BUF_LEN];
static mut SMALL: [u8; SMALL_LEN] = [0; SMALL_LEN];

/// Read a generated file whole. The loop is the point rather than a formality:
/// a `read` that comes back short of what the file holds is a bug in the file,
/// and a test that read once and believed the answer would not see it.
fn read_whole(path: &[u8], buf: &'static mut [u8]) -> Option<&'static [u8]> {
    let fd = lin::openat(lin::AT_FDCWD, path, lin::O_RDONLY, 0);
    if fd < 0 {
        return None;
    }
    let fd = fd as i32;
    let mut used = 0usize;
    while used < buf.len() {
        let n = lin::read(fd, &mut buf[used..]);
        if n <= 0 {
            break;
        }
        used += n as usize;
    }
    lin::close(fd);
    Some(&buf[..used])
}

/// Decimal, without an allocator: `lsattr`-sized numbers go through here.
fn dec(mut n: u64, out: &mut [u8]) -> usize {
    if n == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut at = 0usize;
    while n > 0 {
        out[at] = b'0' + (n % 10) as u8;
        n /= 10;
        at += 1;
    }
    out[..at].reverse();
    at
}

/// `dev` in the `maj:min` spelling mountinfo prints and `/sys` publishes, decoded
/// exactly the way libc decodes it - which is the point of printing it this way
/// rather than as the raw word.
fn majmin(dev: u64, out: &mut [u8; 24]) -> usize {
    let at = dec((dev >> 8) & 0xfff, out);
    out[at] = b':';
    at + 1 + dec((dev & 0xff) | ((dev >> 12) & 0xfff00), &mut out[at + 1..])
}

/// The `n`th space-separated field of a line.
fn field(line: &[u8], n: usize) -> Option<&[u8]> {
    line.split(|&b| b == b' ').nth(n)
}

/// The lines of a file, without its trailing newline's empty tail.
fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.split(|&b| b == b'\n').filter(|l| !l.is_empty())
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-mountinfo\n");
    let mut failed = false;

    // ---- stat("/") first: its `st_dev` is what mountinfo's third field must
    // ---- print, so the answer has to be in hand before the file is read.
    let st = unsafe { &mut *(&raw mut AUX as *mut lin::Stat) };
    lin::check_eq(
        &mut failed,
        b"stat(\"/\")",
        lin::stat(ROOT, st as *mut lin::Stat),
        0,
    );
    let mut want = [0u8; 24];
    let want_len = majmin(st.st_dev, &mut want);
    let want = &want[..want_len];

    // ---- /proc/self/mountinfo ----
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, BUF_LEN) };
    let Some(text) = read_whole(MOUNTINFO, buf) else {
        lin::check_true(&mut failed, b"/proc/self/mountinfo opens", false);
        lin::done(failed);
    };
    lin::check_true(
        &mut failed,
        b"/proc/self/mountinfo is not empty",
        !text.is_empty(),
    );
    lin::check_eq(
        &mut failed,
        b"and ends with a newline",
        text.last().copied().unwrap_or(0) as isize,
        b'\n' as isize,
    );

    let mut count = 0usize;
    let mut shape_ok = true;
    let mut ten_fields = true;
    let mut separator = true;
    let mut root_seen = false;
    let mut root_dev_ok = false;
    let mut fields_are_whole = true;
    for line in lines(text) {
        count += 1;
        // Ten fields, and *only* ten: the optional list between the options
        // and the `-` is empty here - this kernel has no shared subtrees and
        // no idmapped mounts - and a reader that found a field where it
        // expected the separator would misparse every column after it.
        let n = line.split(|&b| b == b' ').count();
        ten_fields &= n == 10;
        separator &= field(line, 6) == Some(&b"-"[..]);
        // Every field before the separator is non-empty: a missing `maj:min`
        // or a mountpoint that vanished would otherwise read as a column of
        // spaces and parse as a different mount.
        for i in 0..6 {
            fields_are_whole &= field(line, i).is_some_and(|f| !f.is_empty());
        }
        // The third field is a `maj:min` and not a bare number: a colon in it
        // is what every reader splits on.
        shape_ok &= field(line, 2).is_some_and(|f| f.contains(&b':'));
        if field(line, 4) == Some(&b"/"[..]) {
            root_seen = true;
            root_dev_ok = field(line, 2) == Some(want);
        }
    }
    lin::check_true(&mut failed, b"at least one mount is reported", count > 0);
    lin::check_true(&mut failed, b"every line has ten fields", ten_fields);
    lin::check_true(&mut failed, b"with the - separator at field 7", separator);
    lin::check_true(
        &mut failed,
        b"and no empty field before it",
        fields_are_whole,
    );
    lin::check_true(&mut failed, b"the device is a maj:min", shape_ok);
    lin::check_true(&mut failed, b"the root is in the table", root_seen);
    lin::check_true(
        &mut failed,
        b"and its device is the st_dev stat reports for it",
        root_dev_ok,
    );

    // ---- /proc/self/mounts, the six-column spelling of the same table ----
    let small = unsafe { core::slice::from_raw_parts_mut(&raw mut SMALL as *mut u8, SMALL_LEN) };
    let Some(mounts) = read_whole(MOUNTS, small) else {
        lin::check_true(&mut failed, b"/proc/self/mounts opens", false);
        lin::done(failed);
    };
    let mut six = true;
    let mut options = true;
    let mut mounts_count = 0usize;
    for line in lines(mounts) {
        mounts_count += 1;
        six &= line.split(|&b| b == b' ').count() == 6;
        // The fourth column is the one that matters - `ro` says a write will
        // be refused - and it is never empty.
        options &= field(line, 3).is_some_and(|f| f.starts_with(b"ro") || f.starts_with(b"rw"));
    }
    lin::check_true(
        &mut failed,
        b"/proc/self/mounts lists a mount",
        mounts_count > 0,
    );
    lin::check_true(&mut failed, b"in fstab's six columns", six);
    lin::check_true(&mut failed, b"with readable options", options);

    // ---- /proc/modules: there, and nothing ----
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, BUF_LEN) };
    match read_whole(MODULES, buf) {
        None => lin::check_true(&mut failed, b"/proc/modules opens", false),
        Some(text) => {
            // Not "a file with no rows": *no bytes at all*, which is what
            // `lsmod` reads as the header and nothing else.
            lin::check_eq(
                &mut failed,
                b"/proc/modules reads as empty",
                text.len() as isize,
                0,
            );
        }
    }

    // ---- /sys/bus/usb/devices: there, and empty ----
    let mut dir = unsafe { core::mem::zeroed::<lin::Stat>() };
    lin::check_eq(
        &mut failed,
        b"/sys/bus/usb/devices exists",
        lin::stat(USB, &raw mut dir),
        0,
    );
    lin::check_true(
        &mut failed,
        b"and is a directory",
        dir.st_mode & lin::S_IFMT == lin::S_IFDIR,
    );
    // Empty, which is what `lsusb` reads as "no devices" - it opens this one
    // path and lists what it finds, so a directory with something in it would
    // be a USB device this machine does not have.
    let fd = lin::openat(lin::AT_FDCWD, USB, lin::O_RDONLY | lin::O_DIRECTORY, 0);
    lin::check_true(&mut failed, b"and opens as one", fd >= 0);
    if fd >= 0 {
        let fd = fd as i32;
        let mut buf = [0u8; 1024];
        let n = lin::getdents64(fd, &mut buf);
        lin::check_true(&mut failed, b"and lists", n >= 0);
        let mut names = 0usize;
        let mut off = 0usize;
        while off < n as usize {
            if let Some((d, reclen)) = lin::parse_dirent64(&buf[off..n as usize]) {
                if d.name != b"." && d.name != b".." {
                    names += 1;
                }
                off += reclen;
            } else {
                off = n as usize;
            }
        }
        lin::check_eq(&mut failed, b"nothing but . and ..", names as isize, 0);
        lin::close(fd);
    }

    // ---- /sys/block: it has to be there, whatever is in it ----
    let mut blk = unsafe { core::mem::zeroed::<lin::Stat>() };
    lin::check_eq(
        &mut failed,
        b"/sys/block exists",
        lin::stat(SYSBLOCK, &raw mut blk),
        0,
    );
    lin::check_true(
        &mut failed,
        b"and is a directory",
        blk.st_mode & lin::S_IFMT == lin::S_IFDIR,
    );

    lin::done(failed)
}

/// A `struct stat` with its own storage, because the checks above hold a
/// reference into it across the reads that use the other buffer.
static mut AUX: lin::Stat = lin::Stat {
    st_dev: 0,
    st_ino: 0,
    st_nlink: 0,
    st_mode: 0,
    st_uid: 0,
    st_gid: 0,
    _pad: 0,
    st_rdev: 0,
    st_size: 0,
    st_blksize: 0,
    st_blocks: 0,
    st_atime: 0,
    st_atime_nsec: 0,
    st_mtime: 0,
    st_mtime_nsec: 0,
    st_ctime: 0,
    st_ctime_nsec: 0,
    _unused: [0; 3],
};

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-mountinfo: panicked\n")
}
