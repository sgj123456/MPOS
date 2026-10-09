//! t-statx-agree - the two ways a file's metadata is asked for must agree.
//!
//! `statx(2)` and `stat(2)`/`lstat(2)`/`fstatat(2)` are different calls with
//! different structs - `struct statx` is 256 bytes, one layout on every machine,
//! where `struct stat` is the machine's own (144 bytes on x86_64, 128 on
//! aarch64). This kernel serves both, and the trap is that a field can be
//! written at the wrong offset in one of them, or not written at all: invisible
//! until a program reads *that* field, which is a thing no compiler checks.
//!
//! `statx` is not a corner of the ABI. A program reaches for it when it wants
//! something `stat` cannot say - a birth time, a mount id, an attributes mask -
//! and uutils' probes, musl's own `statx` wrapper and this repository's
//! conformance suite all call it directly. (Rust's `std` is not one of them on
//! this libc: its `statx` support is glibc-only, so `fs::metadata` here is
//! `fstatat`, which is why nothing in the `std`-side suite ever looked at the
//! `statx` layout.)
//!
//! So the checks below are agreements about one file: the same device, inode,
//! mode, size, link count and clocks, read through both families, must be the
//! same numbers. The clocks are why the test exists - they used to be written
//! 32 bytes from where Linux puts them and nothing failed, because this kernel's
//! filesystems stamp a file's three clocks from one source and no test read them
//! back (`NOTES.md` §13.11).

use rustix::fs::{AtFlags, CWD, StatxFlags, Timespec, Timestamps};
use std::ffi::CString;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// The clock the test sets, so that a `statx` reporting zero where `stat`
/// reports the time cannot pass by both being zero - which is exactly how the
/// misplaced atime survived.
const SET: i64 = 1_700_000_321;

fn main() {
    println!("t-statx-agree");
    let mut r = Report { failed: false };

    let path = "/tmp/t-statx-agree.txt";
    let _ = std::fs::remove_file(path);
    let wrote = std::fs::write(path, b"statx agrees\n");
    r.check(
        &format!("the scratch file can be written ({wrote:?})"),
        wrote.is_ok(),
    );

    // A clock of the test's own choosing. `utimensat` keeps a real time on the
    // filesystem this runs on - `/tmp` is memory-backed everywhere here, and
    // that tree stores the mtime it is given and derives the other two from it,
    // which is a time both calls then have to agree about.
    let stamp = Timestamps {
        last_access: Timespec {
            tv_sec: SET,
            tv_nsec: 0,
        },
        last_modification: Timespec {
            tv_sec: SET,
            tv_nsec: 0,
        },
    };
    let set = rustix::fs::utimensat(CWD, path, &stamp, AtFlags::empty());
    r.check(
        &format!("and its times can be set to {SET} ({set:?})"),
        set.is_ok(),
    );

    // The `fstatat` side, through `libc` - the call `stat`/`lstat`/`fstat` are
    // all spelled in terms of, and the one Rust's `std` uses here.
    let c = CString::new(path).expect("no interior nul");
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let fstatat = unsafe { libc::lstat(c.as_ptr(), &mut st) };
    r.check(&format!("libc's lstat answers ({fstatat})"), fstatat == 0);
    r.check(
        &format!(
            "and it reports the clock the test set (atime {}, mtime {})",
            st.st_atime, st.st_mtime
        ),
        st.st_atime == SET && st.st_mtime == SET,
    );

    // The `statx` side, called directly rather than through anything that might
    // quietly fall back to `fstatat`.
    match rustix::fs::statx(CWD, path, AtFlags::empty(), StatxFlags::BASIC_STATS) {
        Ok(x) => {
            r.check(
                &format!(
                    "statx answers and claims type and size (mask {:#x})",
                    x.stx_mask
                ),
                x.stx_mask & 0x1 != 0 && x.stx_mask & 0x200 != 0,
            );
            r.check(
                &format!("... the same mode ({:#o} vs {:#o})", x.stx_mode, st.st_mode),
                x.stx_mode as u32 == st.st_mode,
            );
            r.check(
                &format!("... the same size ({} vs {})", x.stx_size, st.st_size),
                x.stx_size == st.st_size as u64,
            );
            r.check(
                &format!("... the same inode ({} vs {})", x.stx_ino, st.st_ino),
                x.stx_ino == st.st_ino,
            );
            r.check(
                &format!(
                    "... the same link count ({} vs {})",
                    x.stx_nlink, st.st_nlink
                ),
                x.stx_nlink as u64 == st.st_nlink as u64,
            );
            // The three clocks: the fields this test is for. `stx_atime` used to
            // be written where `stx_ctime` belongs and `stx_ctime` where
            // `stx_rdev_major` does, so these read the wrong word or zero.
            r.check(
                &format!(
                    "... the same access time ({} vs {}, set to {SET})",
                    x.stx_atime.tv_sec, st.st_atime
                ),
                x.stx_atime.tv_sec == st.st_atime && x.stx_atime.tv_sec == SET,
            );
            r.check(
                &format!(
                    "... the same modification time ({} vs {}, set to {SET})",
                    x.stx_mtime.tv_sec, st.st_mtime
                ),
                x.stx_mtime.tv_sec == st.st_mtime && x.stx_mtime.tv_sec == SET,
            );
            r.check(
                &format!(
                    "... the same change time ({} vs {})",
                    x.stx_ctime.tv_sec, st.st_ctime
                ),
                x.stx_ctime.tv_sec == st.st_ctime,
            );
            // The device, and the reason it is checked here rather than trusted:
            // `statx` splits it into a `maj:min` pair and `struct stat` packs it
            // into one number, so a split that swapped the two reports a
            // different filesystem. `rustix`'s `makedev` does the packing, so the
            // comparison is the kernel's split against the kernel's packing.
            let packed = rustix::fs::makedev(x.stx_dev_major, x.stx_dev_minor);
            r.check(
                &format!(
                    "... the same device ({}:{} packs to {:#x}, stat says {:#x})",
                    x.stx_dev_major, x.stx_dev_minor, packed, st.st_dev
                ),
                packed as u64 == st.st_dev,
            );
            r.check(
                &format!(
                    "... and no device for a plain file ({}:{})",
                    x.stx_rdev_major, x.stx_rdev_minor
                ),
                x.stx_rdev_major == 0 && x.stx_rdev_minor == 0,
            );
        }
        Err(e) => r.check(&format!("statx answers ({e})"), false),
    }

    // The root is a second filesystem, and an anonymous device and a disk's are
    // packed differently - a split that only works for one of them is not a
    // split. Both calls are made on the same path here, so nothing but the
    // layout can make them disagree.
    match (
        rustix::fs::statx(CWD, "/", AtFlags::empty(), StatxFlags::BASIC_STATS),
        rustix::fs::lstat("/"),
    ) {
        (Ok(x), Ok(sys)) => {
            let packed = rustix::fs::makedev(x.stx_dev_major, x.stx_dev_minor);
            r.check(
                &format!(
                    "the root's device agrees too ({}:{} packs to {:#x}, stat says {:#x})",
                    x.stx_dev_major, x.stx_dev_minor, packed, sys.st_dev
                ),
                packed as u64 == sys.st_dev,
            );
            r.check(
                &format!(
                    "and the root's mode agrees ({:#o} vs {:#o})",
                    x.stx_mode, sys.st_mode
                ),
                x.stx_mode as u32 == sys.st_mode,
            );
        }
        (a, b) => r.check(
            &format!("the root can be stat'ed both ways ({a:?} / {b:?})"),
            false,
        ),
    }

    let _ = std::fs::remove_file(path);

    if r.failed {
        println!("t-statx-agree: FAILED");
        std::process::exit(1);
    }
    println!("t-statx-agree: all passed");
}
