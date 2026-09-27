//! t-statfs - what a filesystem says about itself.
//!
//! Pinned here, per statfs(2)/fstatfs(2): the 120-byte `struct statfs` the
//! x86_64 ABI lays out, with `f_type` naming the superblock by Linux's magic
//! (`switch_root` and `df` both read it), `f_bsize` the preferred block size,
//! `f_namelen` the longest name the filesystem takes, and `f_blocks` /
//! `f_bfree` the room; a path that does not exist is ENOENT and a descriptor
//! that is not open is EBADF; and every byte of the 120-byte struct is
//! written, so a caller that seeded it with 0xff finds the fields it did not
//! ask about zeroed rather than poisoned.
//!
//! One mpos divergence, named where it is checked: `fstatfs` names the
//! filesystem by the *inode* the descriptor holds, and mpos keeps no path per
//! descriptor - so a file inside a mount reports the root filesystem, not the
//! mount that serves it.
//!
//! `/tmp` is memory-backed in every boot, but *which* memory filesystem owns
//! it depends on the boot: the kernel's own M7 self-test mounts a ramfs there
//! (and the test boot always runs with `mpos.selftest`), so the test `/init`'s
//! `mount -t tmpfs tmpfs /tmp` is refused `EBUSY` and `/tmp` answers the
//! `ramfs` magic; a boot without the self-test would get the `tmpfs` one. Both
//! are memory-backed and both answer the same `f_bsize`/`f_namelen`, so the
//! magic is pinned as one of the two rather than as one of them, and the
//! numbers that do not move are pinned exactly.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static TMP: &[u8] = b"/tmp\0";
static PROC: &[u8] = b"/proc\0";
static MOTD: &[u8] = b"/etc/motd\0";
static MISSING: &[u8] = b"/tmp/t-statfs-gone\0";
static FILE_PATH: &[u8] = b"/tmp/t-statfs.f\0";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-statfs\n");
    let mut failed = false;

    // /tmp is memory-backed, and the numbers it answers are real.
    let mut tmp = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"statfs on /tmp succeeds",
        lin::statfs(TMP, &mut tmp),
        0,
    );
    lin::check_true(
        &mut failed,
        b"/tmp is memory-backed (the tmpfs or ramfs magic, by which boot mounted it)",
        tmp.f_type == lin::TMPFS_MAGIC as i64 || tmp.f_type == lin::RAMFS_MAGIC as i64,
    );
    lin::check_eq(
        &mut failed,
        b"the preferred block size is 4096",
        tmp.f_bsize as isize,
        4096,
    );
    lin::check_eq(
        &mut failed,
        b"the longest name the filesystem takes is 255",
        tmp.f_namelen as isize,
        255,
    );
    lin::check_true(
        &mut failed,
        b"the mount reports some total blocks",
        tmp.f_blocks > 0,
    );
    lin::check_true(
        &mut failed,
        b"the free blocks do not exceed the total",
        tmp.f_bfree <= tmp.f_blocks,
    );

    // procfs, by its own magic.
    let mut proc = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"statfs on /proc succeeds",
        lin::statfs(PROC, &mut proc),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"/proc is a procfs",
        proc.f_type as isize,
        lin::PROC_SUPER_MAGIC as isize,
    );

    // The refusals: nothing to stat, and no descriptor to stat it through.
    let mut gone = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"statfs on a missing path answers ENOENT",
        lin::statfs(MISSING, &mut gone),
        lin::ENOENT,
    );
    let mut bad = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"fstatfs on a closed descriptor answers EBADF",
        lin::fstatfs(99, &mut bad),
        lin::EBADF,
    );

    // fstatfs through the descriptor door: a descriptor naming a mount's own
    // root carries that mount's identity.
    let tmpfd = lin::openat(lin::AT_FDCWD, TMP, lin::O_RDONLY, 0) as i32;
    lin::check_true(&mut failed, b"the /tmp directory opens", tmpfd >= 0);
    let mut fsb = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"fstatfs on the /tmp descriptor succeeds",
        lin::fstatfs(tmpfd, &mut fsb),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"fstatfs reports the same magic as statfs on the same path",
        fsb.f_type as isize,
        tmp.f_type as isize,
    );
    let _ = lin::close(tmpfd);

    // The root filesystem names itself ramfs, which is the magic a file the
    // root really holds reports.
    let mut root = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"statfs on the root succeeds",
        lin::statfs(b"/\0", &mut root),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the root filesystem reports the ramfs magic",
        root.f_type as isize,
        lin::RAMFS_MAGIC as isize,
    );

    // The divergence, named. A regular file *inside* a mount is not the
    // mount's root inode, so mpos falls back to the root filesystem - the
    // same answer it gives for a file that really is on the root.
    let fd = lin::openat(lin::AT_FDCWD, FILE_PATH, lin::O_CREAT | lin::O_RDWR, 0o644) as i32;
    lin::check_true(&mut failed, b"a scratch file in /tmp is created", fd >= 0);
    let mut file_sb = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"fstatfs on a file inside /tmp succeeds",
        lin::fstatfs(fd, &mut file_sb),
        0,
    );
    let motd = lin::openat(lin::AT_FDCWD, MOTD, lin::O_RDONLY, 0) as i32;
    lin::check_true(
        &mut failed,
        b"a file on the root filesystem opens",
        motd >= 0,
    );
    let mut root_sb = lin::Statfs::new();
    lin::check_eq(
        &mut failed,
        b"fstatfs on the root-filesystem file succeeds",
        lin::fstatfs(motd, &mut root_sb),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"mpos divergence: fstatfs names the root filesystem for a file inside a mount, not the mount serving it",
        file_sb.f_type as isize,
        root_sb.f_type as isize,
    );
    let _ = lin::close(fd);
    let _ = lin::close(motd);
    let _ = lin::unlinkat(lin::AT_FDCWD, FILE_PATH, 0);

    // The whole 120-byte struct is written: fill every byte with 0xff first,
    // and the fields this kernel does not fill read back zeroed.
    let mut seeded = lin::Statfs {
        f_type: -1,
        f_bsize: -1,
        f_blocks: u64::MAX,
        f_bfree: u64::MAX,
        f_bavail: u64::MAX,
        f_files: u64::MAX,
        f_ffree: u64::MAX,
        f_fsid: [-1; 2],
        f_namelen: -1,
        f_frsize: -1,
        f_flags: -1,
        f_spare: [-1; 4],
    };
    lin::check_eq(
        &mut failed,
        b"statfs into a poisoned buffer succeeds",
        lin::statfs(TMP, &mut seeded),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"the kernel zeroed the whole struct: f_spare[0] reads back 0",
        seeded.f_spare[0] as isize,
        0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-statfs: panicked\n")
}
