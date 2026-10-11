//! t-ext4-mount — §10.5's disk-backed instrument, part one: a real ext4 on the
//! SATA disk, mounted through the block layer and read back.
//!
//! The gap this closes is written down in `docs/notes/` §10.5: the cache policy
//! was measurable only from a host mock, because the test machine booted
//! *without a filesystem* — an empty sparse image whose every read of an
//! unwritten extent answers zeros without touching the device. Readahead and
//! pipelining were each other's prerequisite and both were unmeasurable; this
//! program and `t-diskbench` are the instrument that changes that, the way
//! `t-pipecost` gave the pipe its number.
//!
//! The fixture rides in each machine's payload at `/dev-sda-fs.img` (an
//! mkfs.ext4 image with three files — a 64 MiB `pattern.bin`, a 4 MiB
//! `cached.bin` and a one-line `hello.txt` — built by `xtask/src/disk.rs`'s
//! `build_ext4_image`; the x86_64 sweep appends it to its test initramfs and
//! the aarch64 one lays it on its sweep medium — one guest path either way).
//! This program copies it onto `/dev/sda` **through the block-device door** —
//! which is itself half of what runs here: the write path
//! of the AHCI driver, at full size, before the read path is judged. It then
//! `mount -t ext4 /dev/sda /mnt` and checks the tree back. The mount is left
//! standing for whatever runs after it in the same sweep, but nothing depends on
//! that: `t-diskbench` runs *before* this program (the runner walks `/bin/t-*` in
//! name order) and lays the same copy itself through the shared loop
//! (`mpos_user_tests::lay_ext4_fixture`), so the two are order-independent set
//! members rather than a sequence.
//!
//! Why a copy rather than a second QEMU drive: one channel (the archive)
//! carries every payload the boot needs, so no host-side file can disagree with
//! the guest's view of it, and the copying *is* the exercise.
//!
//! # The library choices
//!
//! `rustix::fs::{openat, read, write}` carry the I/O (the copy through the
//! crate's shared helper); the mount is `rustix::mount::mount`. The loop reads
//! and writes 1 MiB at a time: it keeps the payload out of any second mapping
//! while the block layer does the moving, and every byte goes through `write(2)`
//! on a block node — the driver's own path, not an mmap flush that would hide it.

use mpos_user_tests::Report;
use rustix::fd::AsFd;
use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

/// The fixture's image in this boot's payload — at the root, and deliberately
/// not under `/dev`: both boots mount a `devtmpfs` over `/dev`, so a payload
/// file unpacked there is shadowed the moment that mount lands (measured on the
/// pair's first gate run, where both programs reported this name missing while
/// the harness had staged it). `xtask`'s `DISKSTAT_FIXTURE_MEMBER` is the same
/// string and its `both_payload_channels_lay_the_fixture_at_one_path` pins the
/// two channels to it.
const SRC: &str = "/dev-sda-fs.img";
const DISK: &str = "/dev/sda";
const MNT: &str = "/mnt/ext4test";
/// The fixture's pattern file, sized in `disk.rs::build_ext4_image`.
const PATTERN: &str = "/mnt/ext4test/pattern.bin";
/// The fixture's *fitting* file — half the cache's slots — sized in the same
/// place. `t-diskbench` is the program that measures it.
const CACHED: &str = "/mnt/ext4test/cached.bin";
const CACHED_BYTES: u64 = 4 * 1024 * 1024;
const SMALL: &str = "/mnt/ext4test/hello.txt";

/// Read `path` fully into a fresh buffer (sizes stay modest: the small files).
fn slurp(path: &str) -> Option<Vec<u8>> {
    let fd = openat(CWD, path, OFlags::RDONLY, Mode::empty()).ok()?;
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match rustix::io::read(&fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    Some(out)
}

fn main() {
    println!("t-ext4-mount");
    let mut r = Report::new();

    // A host run has no /dev/sda and no permission to mount; skip like `t-ahci`
    // does for the same reason, and the payload run (root, disk attached by
    // both wiring arms) treats either absence as a failure.
    if mpos_user_tests::host_run() {
        println!("skip: the host has no SATA disk under this kernel's ABI");
        r.finish();
    }

    // ---- Lay the fixture on the disk, through the block layer ----
    let src = openat(CWD, SRC, OFlags::RDONLY, Mode::empty());
    r.check("the fixture member is there", src.is_ok());
    if src.is_err() {
        r.finish();
    }
    drop(src);
    let disk = openat(CWD, DISK, OFlags::WRONLY, Mode::empty());
    r.check("/dev/sda opens for writing", disk.is_ok());
    if disk.is_err() {
        r.finish();
    }
    let disk = disk.unwrap();
    // The backing file is sized to hold the fixture (§10.5: `SATA_TEST_BYTES`
    // grew when the instrument needed a filesystem *on* this disk). A device
    // smaller than the copy refuses every write past its end — `check_span`'s
    // panic inside the driver — so say which case ran before the first byte,
    // not after a failure that reads as the driver's.
    let st = rustix::fs::fstat(disk.as_fd());
    let sectors = st.map(|s| s.st_size / 512).unwrap_or(0);
    r.check("the SATA disk is big enough for the fixture", sectors > 0);
    drop(disk);

    // The copy itself, in 1 MiB chunks through one anonymous buffer: simple,
    // bounded, and it puts every byte through `write(2)` on a block node — the
    // AHCI write path at volume, which is half of what this program exists to
    // exercise. The loop is shared with `t-diskbench` (`mpos_user_tests::
    // lay_ext4_fixture`), which lays its own copy for the reason the helper's
    // comment gives: the runner walks `/bin/t-*` in name order and that program
    // comes *before* this one.
    let copied = mpos_user_tests::lay_ext4_fixture(SRC, DISK);
    r.check(
        &format!("the fixture copied to the disk ({copied:?})"),
        copied.is_ok_and(|bytes| bytes > 0),
    );
    // `sync(2)`: every write this kernel makes reached its storage already
    // (§1.3's row says why the call is a no-op), so this is the gesture Linux's
    // callers make before trusting a fresh filesystem to another door — cheap,
    // honest, and what a real partitioner does here.
    unsafe { libc::sync() };

    // ---- Mount it ----
    let _ = unmount(MNT, UnmountFlags::empty()); // a leftover from an earlier run
    let _ = mkdirat(CWD, MNT, Mode::from_raw_mode(0o755));
    let m = mount("/dev/sda", MNT, "ext4", MountFlags::empty(), None);
    r.check("mount -t ext4 /dev/sda succeeds", m.is_ok());
    if m.is_err() {
        println!("  mount answered {m:?}");
        r.finish();
    }

    // ---- Read the tree back ----
    let hello = slurp(SMALL);
    r.check(
        "hello.txt reads with the fixture's text",
        hello.as_deref() == Some(b"mpos ext4 test filesystem\n"),
    );
    let pat = openat(CWD, PATTERN, OFlags::RDONLY, Mode::empty());
    r.check("pattern.bin opens", pat.is_ok());
    if let Ok(fd) = &pat {
        let st = rustix::fs::fstat(fd.as_fd());
        r.check(
            "and reports the size the builder wrote",
            st.map(|s| s.st_size).unwrap_or(0) >= 64 * 1024 * 1024,
        );
        let mut first = [0u8; 8192];
        let n = rustix::io::pread(fd, &mut first, 0).unwrap_or(0);
        // The pattern is `(block + byte) % 251`: block 0's bytes are their own
        // offsets, and block 1 starts at 4096 % 251. Content, not just speed.
        let good = n == 8192
            && first[..4096]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == (i % 251) as u8)
            && first[4096..]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == ((1 + i) % 251) as u8);
        r.check("pattern.bin's blocks answer the generated pattern", good);
    }
    // The fixture's second generated file — the one the cache can hold, and the
    // subject of `t-diskbench`'s warm claim. It is checked here because the
    // *tree* the builder wrote is this program's claim: a fixture laid before
    // that file existed would pass a copy-and-mount check and fail the
    // benchmark, and the failure would read as the benchmark's.
    let cached = openat(CWD, CACHED, OFlags::RDONLY, Mode::empty());
    r.check("cached.bin opens", cached.is_ok());
    if let Ok(fd) = &cached {
        r.check(
            "and reports the size the builder wrote for it",
            rustix::fs::fstat(fd.as_fd())
                .map(|s| s.st_size)
                .unwrap_or(0)
                >= CACHED_BYTES as i64,
        );
    }
    // The mount is left standing rather than unmounted, because that is what a
    // person running the pair by hand expects to find — and nothing depends on
    // it: `t-diskbench` lays its own copy and mounts its own view (the mount
    // table is per-task since §6.3, and a sweep's programs are separate
    // processes). The two are order-independent set members, not a sequence.
    r.finish()
}
