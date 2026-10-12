//! t-diskbench — §10.5's instrument, part two: the device-read count under a
//! real filesystem.
//!
//! The gap this closes is written down in `docs/notes/` §10.5: the cache policy
//! was measurable only from a host mock, because the test machine booted
//! *without a filesystem* on its disk — an empty sparse image whose every read
//! of an unwritten extent answers zeros without touching the device. Readahead
//! and pipelining were each other's prerequisite and both were unmeasurable.
//! `t-ext4-mount` lays the filesystem; this program is the number.
//!
//! # What it measures, and through which door
//!
//! Two files, because the instrument asks two questions, and each in its own
//! pass with the kernel's per-disk device-access counters read before and after.
//! The counts are the measurement, not a clock: they are the same on every run,
//! under TCG or KVM, which is what makes them gate-worthy (`ext4/cache.rs`'s
//! `Counting` mock established the rule for the host suite; this applies it to
//! the machine). The claims are exact where the layout is known and bounded
//! elsewhere:
//!
//! * the **cold** pass over the 64 MiB `pattern.bin` reads the file's own blocks
//!   from the device **in runs** — 64 MiB is 16384 4-KiB blocks, past the
//!   cache's 2048 slots by eightfold, so every block misses whatever the policy
//!   does, and since §10.5's wide transfer the misses come back a run of blocks
//!   at a time: **1027 calls** (16384/16 plus the three metadata reads) where one
//!   block per call would be 16384. The claim is the shape (`cold <` the block
//!   count) and the *exact* figure is printed, because the width is the device's
//!   to promise and the harness's only interest is that a run happened and no
//!   block was read twice;
//! * the cold pass over the 4 MiB `cached.bin` reads *its* blocks too, in runs —
//!   it is half of the cache, so they fit, and that is the point of the second
//!   file;
//! * the **warm** pass over `cached.bin` reads *nothing more*: every block of a
//!   file that fits is still cached, which is the claim that fails if the cache
//!   forgets between passes. **It is a second file and not the first for a
//!   measured reason**: the first draft asked this of the 64 MiB file, whose
//!   answer is arithmetically forced to be "no" — eight times the cache cannot
//!   be held by it — and the gate showed exactly that (`cold=16387 warm=16384`);
//! * the numbers are printed as one
//!   `DISKSTAT <disk> cold=N warm=M smallcold=K` line the sweep harness greps
//!   out of the serial log and reports beside the verdict — the reader-side half
//!   of "a performance change ships with its numbers" (rule 6), automated
//!   rather than trusted to whoever ran the gate. `warm` is the fitting file's
//!   second pass, the one the cache policy is judged by.
//!
//! The counters arrive through `uname(2)`: on a boot armed for this instrument
//! (`xtask test`, whose command line carries `mpos.diskstat` exactly when this
//! program is staged), `release` answers the private magic word and `version`
//! carries `<name>:<reads>:<writes>,` rows rendered live at call time. It is a
//! *private* ABI word — no real kernel's osrelease is a number, so nothing
//! that parses these fields can collide with it, and there is no other door:
//! `/proc/partitions` is column-parsed by tools like `fdisk`, and this kernel
//! has no sysfs attribute store a `/sys/class/block/*/stat` could live in. The
//! word, the payload shape and the reason are disclosed in `crates/abi` and
//! `docs/uapi/`. On a host run (real Linux) the fields answer their ordinary
//! strings, the protocol simply is not there, and the program says so.
//!
//! # Why it lays its own fixture and mount rather than trusting a neighbour
//!
//! `t-ext4-mount` lays the same filesystem, but the mount table is per-task now
//! (§6.3) and a sweep's programs are separate processes — and, measured on the
//! first gate run of the pair, the runner walks `/bin/t-*` in *name* order, so
//! this program is staged **before** `t-ext4-mount` and would have found a disk
//! still made of zeros (the disk is laid fresh per sweep, `disk::reset_sata_image`,
//! so no earlier run's filesystem can be inherited either). A benchmark that
//! silently measured a ramfs would be worse than one that failed, so it checks
//! what `/mnt/ext4test/pattern.bin` actually *is* (`statx`'s `st_dev` against the
//! root's) and, when the path is not on the fixture's filesystem, lays that
//! filesystem itself through the same block-door copy `t-ext4-mount` uses
//! (`mpos_user_tests::lay_ext4_fixture`) and mounts it — idempotent,
//! order-independent, and honest about which case it ran in.

use mpos_user_tests::Report;
use rustix::fs::{CWD, Mode, OFlags, StatxFlags, mkdirat, statx};
use rustix::mount::{MountFlags, UnmountFlags, mount, unmount};

const DISK: &str = "/dev/sda";
/// The fixture's image in this boot's payload — the same guest path
/// `t-ext4-mount` copies from (the x86_64 initramfs member and the aarch64
/// medium's entry are pinned equal by the harness; one path either way), and at
/// the root rather than under `/dev` for the reason that program's constant
/// gives: `devtmpfs` is mounted over `/dev` on both boots, so a payload file
/// there is shadowed.
const FIXTURE: &str = "/dev-sda-fs.img";
const MNT: &str = "/mnt/ext4test";
const PATTERN: &str = "/mnt/ext4test/pattern.bin";
/// The fixture's second generated file: the one the cache *can* hold, and the
/// subject of the warm claim. Half the cache's 2048 slots, so the cold pass
/// loads it whole and the second pass should touch the device not at all.
const CACHED: &str = "/mnt/ext4test/cached.bin";
/// The fixture's pattern file size, the constant `xtask/src/disk.rs`'s
/// `build_ext4_image` writes and names. Changing either without the other is
/// the drift that comment pins.
const PATTERN_BYTES: u64 = 64 * 1024 * 1024;
/// The fitting file's size — the other half of that pair of constants.
const CACHED_BYTES: u64 = 4 * 1024 * 1024;
/// One 4-KiB filesystem block — the unit the cache counts, and therefore the
/// unit the cold-pass bound is stated in.
const BLOCK: u64 = 4096;
/// `abi::DISKSTAT_OSRELEASE_MAGIC`, spelled here because a user program cannot
/// name the kernel's crate. The pair (magic, payload grammar) is the protocol.
const MAGIC: &str = "0x6d706f7300000001";

/// The kernel's per-disk counters, read through `uname(2)`'s armed fields.
///
/// `None` means the protocol is not armed (a host run, or an unarmed boot):
/// `release` answered something that is not the magic word. Otherwise the rows
/// are `(disk, reads, writes)` as the version field spells them.
fn io_counters() -> Option<Vec<(String, u64, u64)>> {
    let mut uts = unsafe { core::mem::zeroed::<libc::utsname>() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return None;
    }
    let field = |raw: *const u8| -> String {
        let bytes = unsafe { core::slice::from_raw_parts(raw, 65) };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(65);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    if field(uts.release.as_ptr().cast()) != MAGIC {
        return None;
    }
    // "#1 sda:1234:56,vda:7:8," — the leading "#1 " stands where the build
    // banner's does, and each row is `<name>:<reads>:<writes>` ending in ','.
    let version = field(uts.version.as_ptr().cast());
    let body = version
        .strip_prefix("#1 ")
        .or_else(|| version.split_once(' ').map(|(_, r)| r))?;
    let mut rows = Vec::new();
    for row in body.split(',').filter(|r| !r.is_empty()) {
        let mut parts = row.split(':');
        let name = parts.next()?.to_string();
        let reads = parts.next()?.parse().ok()?;
        let writes = parts.next()?.parse().ok()?;
        rows.push((name, reads, writes));
    }
    Some(rows)
}

/// The device a path lives on, as `statx` reports it — the same `st_dev` the
/// mount table publishes, which is what tells "the file is on the ext4 mount"
/// from "something made a directory with this name earlier".
fn dev_of(path: &str) -> Option<u64> {
    let st = statx(
        CWD,
        path,
        rustix::fs::AtFlags::empty(),
        StatxFlags::BASIC_STATS,
    )
    .ok()?;
    Some(u64::from(st.stx_dev_major) << 32 | u64::from(st.stx_dev_minor))
}

/// The named disk's read counter, from the rows the door reported.
fn sda_reads(rows: &[(String, u64, u64)]) -> Option<u64> {
    rows.iter()
        .find(|(name, _, _)| name == "sda")
        .map(|(_, r, _)| *r)
}

fn main() {
    println!("t-diskbench");
    let mut r = Report::new();

    if mpos_user_tests::host_run() {
        println!(
            "skip: the host's uname answers its own release; the disk-counter protocol is this kernel's"
        );
        r.finish();
    }

    // ---- The door exists ----
    let first = io_counters();
    r.check(
        "uname answers the armed disk-counter protocol",
        first.is_some(),
    );
    let rows = match first {
        Some(rows) => rows,
        None => r.finish(),
    };
    r.check("and names the SATA disk", sda_reads(&rows).is_some());
    let mut base = match sda_reads(&rows) {
        Some(n) => n,
        None => r.finish(),
    };

    // ---- The filesystem is really under the path ----
    let root_dev = dev_of("/");
    let pat_dev = dev_of(PATTERN);
    let mounted_here = pat_dev.is_some() && pat_dev != root_dev;
    if !mounted_here {
        // Lay it ourselves: this program runs *before* `t-ext4-mount` (the
        // runner's order is the name order), so the disk is still the zeros the
        // sweep laid; the copy is the same block-door one that program makes and
        // the loop is theirs to share (`mpos_user_tests::lay_ext4_fixture`).
        let _ = mkdirat(CWD, MNT, Mode::from_raw_mode(0o755));
        let copied = mpos_user_tests::lay_ext4_fixture(FIXTURE, DISK);
        r.check(
            &format!("the fixture copied to the disk ({copied:?})"),
            copied.is_ok_and(|bytes| bytes > 0),
        );
        let m = mount(DISK, MNT, "ext4", MountFlags::empty(), None);
        r.check("mount -t ext4 /dev/sda succeeds here too", m.is_ok());
        if m.is_err() {
            r.finish();
        }
    } else {
        r.check("the ext4 mount t-ext4-mount left is visible here", true);
    }

    // A mount's own metadata reads move the counter, so measure after the tree
    // is open rather than before: re-baseline.
    let rows = io_counters().unwrap_or_default();
    base = sda_reads(&rows).unwrap_or(base);

    // ---- Cold pass: every block of the oversized file must come off the device ----
    let fd = match rustix::fs::openat(CWD, PATTERN, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("pattern.bin opens ({e:?})"), false);
            r.finish();
        }
    };
    let mut buf = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    let mut total = 0u64;
    let mut ok = true;
    loop {
        match rustix::io::pread(&fd, &mut buf, offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                offset += n as u64;
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    r.check(
        "the cold pass read the whole file",
        ok && total >= PATTERN_BYTES,
    );
    let rows = io_counters().unwrap_or_default();
    let cold = sda_reads(&rows)
        .map(|n| n.saturating_sub(base))
        .unwrap_or(0);
    // **In runs**: §10.5's wide transfer, measured. The counter counts *calls*,
    // so a pass that read one block per call would report the file's block count
    // here (16384) - and the claim is that it does not. The exact figure is
    // reported beside the verdict rather than asserted, because how wide a run
    // this device serves is the kernel's business: what the harness pins is the
    // *shape*, that the file's blocks reach the device in runs and no block is
    // read twice (a call cannot cover fewer than one block, so `cold <= blocks`).
    r.check(
        "the cold pass reached the device for the file's blocks, in runs",
        cold > 0 && cold < PATTERN_BYTES / BLOCK,
    );
    // ...and the runs are *wide*, not merely wider than one: at least eight
    // blocks per call on average. The bound is the test's own and stated as such
    // - the measured figure is two blocks clear of it on each side (a 16-block
    // run gives `16384/16 + 3`, and a run of one gives `16384`) - so it pins the
    // order of magnitude without pinning the kernel's constant.
    r.check(
        "and in runs of at least eight blocks each",
        cold <= PATTERN_BYTES / BLOCK / 8,
    );
    drop(fd);

    // ---- The file that fits: its cold pass, then its warm one ----
    //
    // The order is the measurement's own: the oversized pass above evicted
    // whatever it found, so this file's blocks are loaded fresh here and the
    // cache's *retention* is what the second pass measures.
    let fd = match rustix::fs::openat(CWD, CACHED, OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("cached.bin opens ({e:?})"), false);
            r.finish();
        }
    };
    let rows = io_counters().unwrap_or_default();
    let small_base = sda_reads(&rows).unwrap_or(cold + base);
    offset = 0;
    total = 0;
    ok = true;
    loop {
        match rustix::io::pread(&fd, &mut buf, offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                offset += n as u64;
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    r.check("the fitting file read whole", ok && total >= CACHED_BYTES);
    let rows = io_counters().unwrap_or_default();
    let small_cold = sda_reads(&rows)
        .map(|n| n.saturating_sub(small_base))
        .unwrap_or(0);
    r.check(
        "and its blocks came off the device in runs, because they fit the cache only now",
        small_cold > 0 && small_cold < CACHED_BYTES / BLOCK,
    );

    // ---- Warm pass: the cache must keep every block it just loaded ----
    let rows = io_counters().unwrap_or_default();
    let warm_base = sda_reads(&rows).unwrap_or(small_cold + small_base);
    offset = 0;
    total = 0;
    ok = true;
    loop {
        match rustix::io::pread(&fd, &mut buf, offset) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                offset += n as u64;
            }
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    r.check(
        "the warm pass read the fitting file again",
        ok && total >= CACHED_BYTES,
    );
    let rows = io_counters().unwrap_or_default();
    let warm = sda_reads(&rows)
        .map(|n| n.saturating_sub(warm_base))
        .unwrap_or(u64::MAX);
    r.check(
        "the warm pass added no device reads (the cache held the file it can hold)",
        warm == 0,
    );

    // The numbers themselves, one line the sweep harness greps. This is the
    // instrument's output, not a verdict: the claims above are what a red run
    // means, and the line is what a green run *measured*.
    println!("DISKSTAT sda cold={cold} warm={warm} smallcold={small_cold}");
    drop(fd);
    if !mounted_here {
        let _ = unmount(MNT, UnmountFlags::empty());
    }
    r.finish()
}
