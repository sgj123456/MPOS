//! t-fd-list - `/proc/<pid>/fd` must name every open descriptor.
//!
//! Pinned here: a process opens a known set of descriptors, then reads its own
//! `/proc/self/fd` with `getdents64` and has to find **every one** of them -
//! the three standard descriptors plus the ones it opened, whichever numbers
//! they landed on. The listing is what `ls -l /proc/<pid>/fd`, `udevadm` and a
//! debugger read to learn what a process holds; a listing that silently drops
//! entries is worse than an empty one, because the reader concludes the
//! descriptors are gone.
//!
//! This exists because a diagnosis of `niri` reported the listing going
//! `0, 1, 17, 18` - as if `2..=16` had been dropped. That is also exactly what
//! a process that opened and closed those numbers looks like, so the question
//! had to be settled by construction rather than by reading: this program opens
//! a contiguous run of descriptors and then checks the listing against what it
//! knows it holds. If the two disagree the kernel is dropping entries; if they
//! agree, the compositor's fd numbers were simply not contiguous.
//!
//! The names are compared as numbers, not strings, so fd 10 is checked as ten
//! and not as the byte `'1'` followed by `'0'`.
//!
//! # The directory is read through `rustix`, and one binding was chosen over
//! # another on purpose
//!
//! `rustix::fs::RawDir` is the `getdents64` binding that takes **the caller's
//! buffer** and walks `struct linux_dirent64` with rustix's own offsets - the
//! same call the no-std version made by hand, with the layout maintained
//! instead of remembered. Its high-level sibling `Dir::read_from` is *not* a
//! substitute here, and the reason is measurable in rustix's own source: it
//! first `fcntl(F_GETFL)`s the descriptor and then `openat(fd, ".")`s a
//! descriptor of its own, so reading `/proc/self/fd` through it would make two
//! calls this program's subject never makes - and a procfs directory is
//! exactly where "`.` opens" is a separate thing to be true.
//!
//! # Where the listing comes from
//!
//! The x86_64 test initramfs mounts `/proc` with busybox before it runs
//! anything, and the aarch64 sweep's disk chain does the same
//! (`rootfs-aarch64/init`'s `mount -t proc proc /proc`) before the runner takes
//! over - *measured*, not assumed: both runs of this program report "the
//! payload's own /proc". The mount below is the `t-tmpfile-link` pattern, and it
//! is there for a payload that has none, which is a harness gap rather than the
//! kernel's; it says which `/proc` it used, so a reader can tell the two cases
//! apart. The fixtures are the test's own under `/tmp`, which both runners back
//! with a tmpfs, so nothing here depends on what the payload's image carries.

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::fs::{CWD, Mode, OFlags, RawDir, openat, unlinkat};
use rustix::mount::{MountFlags, mount};

use std::mem::MaybeUninit;

/// How many descriptors this program deliberately holds, beyond the three
/// standard ones. Twelve is enough to cross the one-digit/two-digit boundary
/// and to cover the range that was reported missing.
const EXTRA: usize = 12;
/// One past the largest descriptor number the listing is scanned for.
const MAX_FD: usize = 64;

/// The names this test opens, so every one of them is a path it created itself.
const NAMES: [&str; EXTRA] = [
    "/tmp/t-fd-list-0",
    "/tmp/t-fd-list-1",
    "/tmp/t-fd-list-2",
    "/tmp/t-fd-list-3",
    "/tmp/t-fd-list-4",
    "/tmp/t-fd-list-5",
    "/tmp/t-fd-list-6",
    "/tmp/t-fd-list-7",
    "/tmp/t-fd-list-8",
    "/tmp/t-fd-list-9",
    "/tmp/t-fd-list-a",
    "/tmp/t-fd-list-b",
];

/// A `getdents64` buffer large enough that the `INVAL`-means-grow protocol
/// cannot be reached: a descriptor's name is a handful of bytes.
const BUF: usize = 8192;

/// The mountpoint this test owns when the payload has no `/proc`.
const OWN_PROC: &str = "/t-fd-list-proc";

/// The decimal name of a dirent, or `None` when it is not a number (`.` and
/// `..` are the two that matter).
fn parse_number(name: &[u8]) -> Option<usize> {
    let mut value = 0usize;
    let mut digits = 0;
    for &byte in name {
        if byte == 0 {
            break;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as usize;
        digits += 1;
        if digits > 6 {
            return None;
        }
    }
    (digits > 0).then_some(value)
}

/// Read `path` whole, into a presence table and a count.
fn listing(path: &str) -> Result<([bool; MAX_FD], usize), rustix::io::Errno> {
    let fd = openat(CWD, path, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
    let mut buf = [MaybeUninit::<u8>::uninit(); BUF];
    let mut dir = RawDir::new(&fd, &mut buf);
    let mut present = [false; MAX_FD];
    let mut count = 0usize;
    while let Some(entry) = dir.next() {
        let entry = entry?;
        if let Some(number) = parse_number(entry.file_name().to_bytes()) {
            count += 1;
            if number < MAX_FD {
                present[number] = true;
            }
        }
    }
    Ok((present, count))
}

/// Where a descriptor's listing can be read: the payload's own `/proc` when it
/// has one, otherwise a procfs this test mounts and owns.
///
/// `None` means the mount was refused, which is a setup failure rather than a
/// kernel failure and is reported as its own claim.
fn procfs_root(r: &mut Report) -> Option<&'static str> {
    if std::path::Path::new("/proc/self").exists() {
        println!("     the payload's own /proc");
        return Some("/proc");
    }
    // The mountpoint must exist first: `mount` answers ENOTDIR otherwise.
    let _ = std::fs::create_dir(OWN_PROC);
    if let Err(e) = mount(
        "proc",
        OWN_PROC,
        "proc",
        MountFlags::empty(),
        None::<&std::ffi::CStr>,
    ) {
        r.check(
            &format!("a procfs can be mounted to read the listing from (got {e})"),
            false,
        );
        return None;
    }
    println!("     a procfs mounted by this test ({OWN_PROC}): the payload has none");
    Some(OWN_PROC)
}

fn main() {
    println!("t-fd-list");
    let mut r = Report::new();

    // A contiguous run of descriptors, opened the ordinary way: they take the
    // lowest free numbers, which is what makes the expectation unambiguous.
    let mut held: Vec<OwnedFd> = Vec::with_capacity(EXTRA);
    for name in NAMES {
        match openat(
            CWD,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC,
            Mode::from(0o600),
        ) {
            Ok(fd) => held.push(fd),
            Err(e) => {
                r.check(&format!("the fixture files open (got {e})"), false);
                break;
            }
        }
    }
    r.check(
        &format!("the fixture run of {EXTRA} descriptors is held"),
        held.len() == EXTRA,
    );

    let Some(root) = procfs_root(&mut r) else {
        r.finish()
    };
    let dir = format!("{root}/self/fd");
    let (present, count) = match listing(&dir) {
        Ok(v) => v,
        Err(e) => {
            r.check(&format!("{dir} can be read (got {e})"), false);
            r.finish()
        }
    };
    r.check("the listing could be read", count > 0);

    // The three standard descriptors are the floor: a process cannot be
    // without them, and a listing that omitted one would be lying about the
    // most fundamental thing in the table.
    for fd in 0..3 {
        r.check(&format!("the listing names descriptor {fd}"), present[fd]);
    }

    // And every descriptor this program opened, at the number the open
    // actually returned - so a number the kernel reused is still checked.
    let mut missing = 0usize;
    for fd in held.iter().map(OwnedFd::as_raw_fd) {
        let number = fd as usize;
        if number < MAX_FD && !present[number] {
            missing += 1;
            println!("FAIL: the listing is missing descriptor {number}");
        }
    }
    r.check(
        &format!("no opened descriptor is missing ({missing} of {EXTRA})"),
        missing == 0,
    );

    // The count has to be at least what is held: three standard plus the run.
    r.check(
        "the listing has at least one entry per held descriptor",
        count >= 3 + held.len(),
    );

    for fd in held {
        drop(fd);
    }
    for name in NAMES {
        let _ = unlinkat(CWD, name, rustix::fs::AtFlags::empty());
    }

    r.finish()
}
