//! t-statfs - what a filesystem says about itself.
//!
//! Pinned here, per statfs(2)/fstatfs(2): `f_type` naming the superblock by
//! Linux's magic (`switch_root` and `df` both read it), `f_bsize` the preferred
//! block size, `f_namelen` the longest name the filesystem takes, and
//! `f_blocks` / `f_bfree` the room; a path that does not exist is ENOENT and a
//! descriptor that is not open is EBADF; and every byte of the struct is
//! written, so a caller that seeded it with 0xff finds the fields it did not
//! ask about zeroed rather than poisoned.
//!
//! The two doors agree about a file inside a mount. `fstatfs` used to name the
//! filesystem by the *inode* the descriptor held, and an inode does not name
//! its filesystem - so a file inside a mount reported the root. The descriptor
//! records the device number it was opened on (`File::dev`), and the mount
//! table is keyed by that number now, so `fstatfs(fd)` and `statfs(path)` are
//! one answer (measured on a current host, `.workbuddy/probe_fstatfs.c`).
//!
//! `/tmp` is memory-backed in every boot, but *which* memory filesystem owns
//! it depends on the boot: the kernel's own M7 self-test mounts a ramfs there
//! (and the test boot always runs with `mpos.selftest`), so the test `/init`'s
//! `mount -t tmpfs tmpfs /tmp` is refused `EBUSY` and `/tmp` answers the
//! `ramfs` magic; a boot without the self-test would get the `tmpfs` one. Both
//! are memory-backed and both answer the same `f_bsize`/`f_namelen`, so the
//! magic is pinned as one of the two rather than as one of them, and the
//! numbers that do not move are pinned exactly. On a host run the same two
//! slots answer ext4/btrfs/whatever the machine really has, so the magic
//! claims carry the `host_run()` escape with their measurement.
//!
//! # Why `/tmp` is not the "inside a mount" fixture
//!
//! The no-std version opened `/etc/motd`; the second machine's payload has no
//! motd (`NOTES.md` §15.11), and the first machine's boot mounts a ramfs at
//! `/tmp` **over a ramfs root** - so a file there and a file on the root are
//! two descriptors that answer the same magic, and a claim comparing them
//! passes whether or not the kernel names the right filesystem. The counterfactual
//! for this item measured exactly that: with the fix reverted, every `/tmp`
//! claim stayed green and only the procfs ones went red. `/proc` is therefore
//! the fixture - its magic differs from a ramfs root *and* from an ext4 root,
//! so the claim cannot pass by two numbers happening to agree.
//!
//! The whole-struct write is the one claim `rustix::fs::statfs` cannot make:
//! the wrapper owns its buffer (`MaybeUninit`, never seeded), so "seeded with
//! 0xff and the untouched fields came back zeroed" has no spelling through it
//! - clause (c) again, the same shape as `t-fstat`'s EFAULT probe. That claim
//!   goes through `libc::syscall(libc::SYS_statfs, ..)` into a
//!   caller-owned `libc::statfs` filled with 0xff first. Every other call is
//!   `rustix`.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, BorrowedFd};
use rustix::fs::{CWD, Mode, OFlags, StatFs, fstatfs, openat, statfs};
use rustix::io::Errno;

/// The magics, from `linux_magic.h` through the maintained tables.
const TMPFS_MAGIC: u64 = 0x0102_1994;
const RAMFS_MAGIC: u64 = 0x8584_58f6;
const PROC_SUPER_MAGIC: u64 = 0x0000_9fa0;
/// The first machine's root is the kernel's initramfs, the second machine's
/// is the ext2/ext4 root partition of its own disk - see the root claim in
/// the body for why the two differ.
const EXT4_SUPER_MAGIC: u64 = 0x0000_ef53;

/// A name that is not there.
const MISSING: &str = "/tmp/t-statfs-gone";

/// A caller-owned `struct statfs`: 120 bytes, eight-aligned, read by offset.
///
/// This exists because `f_fsid` has no spelling through either wrapper:
/// `rustix::fs::StatFs` keeps `Fsid::val` private, and `libc`'s own `fsid_t` is
/// not the same shape on every target - while the kernel's answer is a byte
/// range either way, and reading a byte range is what a raw door is for. Every
/// other claim here goes through `rustix`; only this field cannot.
#[repr(align(8))]
struct Raw([u8; 120]);

impl Raw {
    fn new() -> Raw {
        Raw([0u8; 120])
    }
    /// Every byte seeded, so a field the kernel did not write reads as the
    /// seed rather than as a plausible zero.
    fn poisoned() -> Raw {
        Raw([0xffu8; 120])
    }
    fn fsid(&self) -> u64 {
        u64::from(u32::from_le_bytes(self.0[56..60].try_into().unwrap()))
            | (u64::from(u32::from_le_bytes(self.0[60..64].try_into().unwrap())) << 32)
    }
    fn word(&self, at: usize) -> u64 {
        u64::from_le_bytes(self.0[at..at + 8].try_into().unwrap())
    }
}

/// `statfs(path, buf)` as the **raw** call, into the caller's own buffer -
/// the only way to see which bytes the kernel wrote. Returns whether the
/// call answered success.
fn statfs_raw(path: &str, buf: &mut Raw) -> bool {
    let c = std::ffi::CString::new(path).unwrap();
    unsafe {
        libc::syscall(
            libc::SYS_statfs,
            c.as_ptr() as libc::c_long,
            buf.0.as_mut_ptr() as libc::c_long,
        ) == 0
    }
}

/// `fstatfs(fd, buf)` - the same door by descriptor, same reason.
fn fstatfs_raw(fd: BorrowedFd<'_>, buf: &mut Raw) -> bool {
    unsafe {
        libc::syscall(
            libc::SYS_fstatfs,
            libc::c_long::from(rustix::fd::AsRawFd::as_raw_fd(&fd)),
            buf.0.as_mut_ptr() as libc::c_long,
        ) == 0
    }
}

/// The magic field of either struct spelling, widened to one type.
fn magic_of(st: &StatFs) -> u64 {
    st.f_type as u64
}

/// The same, read out of a raw buffer.
fn raw_magic(buf: &Raw) -> u64 {
    buf.word(0)
}

fn main() {
    println!("t-statfs");
    let mut r = Report::new();
    let host = mpos_user_tests::host_run();

    // /tmp is memory-backed, and the numbers it answers are real.
    match statfs("/tmp") {
        Ok(tmp) => {
            r.check("statfs on /tmp succeeds", true);
            r.check(
                "/tmp is memory-backed (the tmpfs or ramfs magic, by which boot mounted it)",
                host || magic_of(&tmp) == TMPFS_MAGIC || magic_of(&tmp) == RAMFS_MAGIC,
            );
            r.check("the preferred block size is 4096", tmp.f_bsize == 4096);
            r.check(
                "the longest name the filesystem takes is 255",
                tmp.f_namelen == 255,
            );
            r.check("the mount reports some total blocks", tmp.f_blocks > 0);
            r.check(
                "the free blocks do not exceed the total",
                tmp.f_bfree <= tmp.f_blocks,
            );

            // procfs, by its own magic.
            match statfs("/proc") {
                Ok(proc) => {
                    r.check("statfs on /proc succeeds", true);
                    r.check(
                        "/proc is a procfs",
                        host || magic_of(&proc) == PROC_SUPER_MAGIC,
                    );
                }
                Err(_) => {
                    r.check("statfs on /proc succeeds", false);
                    r.check("/proc is a procfs", false);
                }
            }

            // The refusals: nothing to stat, and no descriptor to stat it
            // through.
            r.check(
                "statfs on a missing path answers ENOENT",
                matches!(statfs(MISSING), Err(Errno::NOENT)),
            );
            // The number is *minted* rather than assumed: a hardcoded one is
            // only closed when the caller's own table happens to be empty
            // there, and a test run from a host process that has that many
            // descriptors open answers the seed instead of `EBADF` - measured
            // here, fd 99 was an inherited directory the first time this ran
            // under a live shell. Opening and closing a file of this test's
            // own makes the number one that is closed by construction, and
            // nothing opens another descriptor between the two.
            let minted = openat(CWD, "/tmp", OFlags::RDONLY, Mode::empty())
                .map(rustix::fd::OwnedFd::from)
                .ok();
            match minted {
                Some(fd) => {
                    let number = rustix::fd::AsRawFd::as_raw_fd(&fd);
                    drop(fd);
                    let bad = unsafe { BorrowedFd::borrow_raw(number) };
                    r.check(
                        "fstatfs on a closed descriptor answers EBADF",
                        matches!(fstatfs(bad), Err(Errno::BADF)),
                    );
                }
                None => r.check("a descriptor to close can be minted", false),
            }

            // fstatfs through the descriptor door: a descriptor naming a
            // mount's own root carries that mount's identity.
            let tmpfd = openat(CWD, "/tmp", OFlags::RDONLY, Mode::empty());
            r.check("the /tmp directory opens", tmpfd.is_ok());
            if let Ok(tmpfd) = &tmpfd {
                match fstatfs(tmpfd) {
                    Ok(fsb) => {
                        r.check("fstatfs on the /tmp descriptor succeeds", true);
                        r.check(
                            "fstatfs reports the same magic as statfs on the same path",
                            magic_of(&fsb) == magic_of(&tmp),
                        );
                    }
                    Err(_) => {
                        r.check("fstatfs on the /tmp descriptor succeeds", false);
                        r.check(
                            "fstatfs reports the same magic as statfs on the same path",
                            false,
                        );
                    }
                }
            }

            // The root filesystem answers the magic *its machine's boot*
            // mounts there, which is not the same filesystem on the two
            // machines: the first machine's sweep keeps the kernel's own
            // initramfs as `/` (a ramfs, the magic a file the root really
            // holds reports), while the second machine's payload rides the
            // ext2/ext4 root partition of its own disk and `/` is that
            // filesystem (`disk::sweep_medium`; the boot's own
            // `MPOS_SWITCH_ROOT_OK: / is /dev/vda2 (ext4)` line is the same
            // fact from the other side). A host's root is whatever the
            // machine was installed on, so the host escape names that third
            // case.
            const EXPECTED_ROOT_MAGIC: u64 = if cfg!(target_arch = "x86_64") {
                RAMFS_MAGIC
            } else {
                EXT4_SUPER_MAGIC
            };
            match statfs("/") {
                Ok(root) => {
                    r.check("statfs on the root succeeds", true);
                    r.check(
                        "the root filesystem reports the magic its boot mounted",
                        host || magic_of(&root) == EXPECTED_ROOT_MAGIC,
                    );
                }
                Err(_) => {
                    r.check("statfs on the root succeeds", false);
                    r.check(
                        "the root filesystem reports the magic its boot mounted",
                        false,
                    );
                }
            }

            // A file *inside* a mount is served by that mount, and the two
            // doors that report a filesystem for a file agree: `statfs` on the
            // path and `fstatfs` on a descriptor opened through it. The
            // descriptor records the device number it was opened on, so the
            // answer no longer needs a path - and a file that is not the
            // mount's root inode is exactly the shape that used to fall back
            // to the root filesystem. Measured on a current host
            // (`.workbuddy/probe_fstatfs.c`, Linux 7.2.11): `fstatfs` on a file
            // inside `/dev/shm` or `/proc` reports that mount's magic exactly
            // as `statfs` on the same path does.
            //
            // `/proc/self/status` is the fixture because its magic differs
            // from the root's on *both* machines (a ramfs root on one, an ext4
            // root on the other, and a procfs over either), so the claim
            // cannot pass by the two numbers happening to agree - which is
            // what a `/tmp` fixture did on the first machine, where the root
            // and `/tmp` are both ramfs.
            let outside = openat(CWD, "/", OFlags::RDONLY, Mode::empty());
            r.check("a file on the root filesystem opens", outside.is_ok());
            let mount_root = openat(CWD, "/proc", OFlags::RDONLY, Mode::empty());
            r.check("the mountpoint itself opens", mount_root.is_ok());
            let inside = openat(CWD, "/proc/self/status", OFlags::RDONLY, Mode::empty());
            r.check("a procfs file inside a mount opens", inside.is_ok());
            if let (Ok(inside), Ok(outside), Ok(mount_root)) = (&inside, &outside, &mount_root) {
                let file_sb = fstatfs(inside);
                let root_sb = fstatfs(outside);
                let mount_sb = fstatfs(mount_root);
                r.check("fstatfs on a file inside a mount succeeds", file_sb.is_ok());
                r.check(
                    "fstatfs on the root-filesystem file succeeds",
                    root_sb.is_ok(),
                );
                if let (Ok(file_sb), Ok(root_sb)) = (file_sb, root_sb) {
                    // The mountpoint's own descriptor is the one even the old
                    // answer got right (it is the mount's root inode), so this
                    // pair cannot agree by accident: whichever of the two
                    // reported the root filesystem, they disagree.
                    if let Ok(mount_sb) = &mount_sb {
                        r.check(
                            "and it agrees with fstatfs on the mountpoint's own descriptor",
                            magic_of(mount_sb) == magic_of(&file_sb),
                        );
                    }
                    r.check(
                        "fstatfs on a file inside a mount names the mount, not the root",
                        magic_of(&file_sb) == PROC_SUPER_MAGIC
                            && magic_of(&file_sb) != magic_of(&root_sb),
                    );
                    let by_path = statfs("/proc/self/status");
                    r.check(
                        "and statfs on that file's path agrees with fstatfs",
                        by_path.is_ok_and(|p| magic_of(&p) == magic_of(&file_sb)),
                    );
                }
            }
            drop(inside);
            drop(outside);
            drop(mount_root);

            // `f_fsid` names the filesystem *instance*, and Linux never
            // answers it zero: modern kernels hash the superblock's UUID into
            // it, and a filesystem with no UUID gets the superblock's device
            // number encoded instead. Measured on a current host
            // (`.workbuddy/probe_fsid.c`, Linux 7.2.11): `/` answers a UUID
            // hash, `/proc` answers `0x18` — exactly its own `st_dev` put
            // through `new_encode_dev` — and in every row `statfs(path)` and
            // `fstatfs(fd)` agree on it.
            //
            // So the *value* is not what this pins; a UUID hash is not the
            // same number on three machines. What pins is the structure every
            // row there has: non-zero, shared by every name one instance
            // serves, and different between two mounts that are alive at once.
            // All three were zero here before this item - the field was left
            // cleared - so every filesystem looked like the same filesystem to
            // anything that compared them, which is the comparison
            // `statfs(2)` offers this field for.
            let mut root_statfs = Raw::new();
            let mut proc_statfs = Raw::new();
            let mut file_statfs = Raw::new();
            let ok_root = statfs_raw("/", &mut root_statfs);
            let ok_proc = statfs_raw("/proc", &mut proc_statfs);
            let ok_file = statfs_raw("/proc/self/status", &mut file_statfs);
            // The three reads are claims of their own, so the block below
            // cannot be skipped silently: a shape that measured nothing would
            // otherwise leave `f_fsid` unexamined and still print green.
            r.check("statfs into a caller's own buffer succeeds on /", ok_root);
            r.check(
                "statfs into a caller's own buffer succeeds on /proc",
                ok_proc,
            );
            r.check(
                "statfs into a caller's own buffer succeeds on a file inside /proc",
                ok_file,
            );
            if ok_root && ok_proc && ok_file {
                r.check(
                    "the root filesystem reports a non-zero f_fsid",
                    root_statfs.fsid() != 0,
                );
                r.check("procfs reports a non-zero f_fsid", proc_statfs.fsid() != 0);
                r.check(
                    "two names one filesystem serves report the same f_fsid",
                    proc_statfs.fsid() == file_statfs.fsid(),
                );
                r.check(
                    "and two mounts that are both alive report different ones",
                    proc_statfs.fsid() != root_statfs.fsid(),
                );
                let again = openat(CWD, "/proc/self/status", OFlags::RDONLY, Mode::empty());
                r.check("the procfs file opens again", again.is_ok());
                if let Ok(again) = &again {
                    let mut by_fd = Raw::new();
                    r.check(
                        "fstatfs on its descriptor succeeds",
                        fstatfs_raw(again.as_fd(), &mut by_fd),
                    );
                    r.check(
                        "and fstatfs reports the fsid statfs reports on that descriptor's path",
                        by_fd.fsid() == file_statfs.fsid(),
                    );
                }
            }

            // The whole struct is written: fill every byte with 0xff first,
            // and the fields this kernel does not fill read back zeroed.
            // This is the raw door's claim - `rustix`'s wrapper owns its
            // buffer and seeds it with `MaybeUninit`, never with a pattern.
            let mut seeded = Raw::poisoned();
            let ok = statfs_raw("/tmp", &mut seeded);
            r.check("statfs into a poisoned buffer succeeds", ok);
            if ok {
                // The seed was the point: the fields the kernel fills came
                // back real (the magic matches what `rustix`'s own call
                // answered), and the fields it does not fill came back zero.
                r.check(
                    "the kernel wrote the whole struct: f_type is no longer the seed",
                    raw_magic(&seeded) == magic_of(&tmp),
                );
                r.check(
                    "the kernel zeroed the whole struct: f_spare[0] reads back 0",
                    seeded.word(88) == 0,
                );
            }
        }
        Err(e) => {
            r.check(&format!("statfs on /tmp succeeds (got {e})"), false);
        }
    }

    r.finish()
}
