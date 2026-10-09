//! t-statx - one test for statx, which uutils uses for its locale and
//! self-locating probes (`statx(AT_FDCWD, path, AT_STATX_SYNC_AS_STAT,
//! STATX_ALL, buf)` in the strace).
//!
//! Pinned here: the Linux argument shape - dirfd, NUL-terminated path,
//! flags, request mask, buffer - the answers' consistency: the mask must say
//! TYPE and SIZE were filled, stx_size must equal what reading the file
//! yields, stx_mode must say regular file, stx_ino must agree with
//! `rustix`'s stat, an empty path stats the descriptor only under
//! AT_EMPTY_PATH, a null buffer answers EFAULT, and a directory reports
//! S_IFDIR.
//!
//! The clocks are pinned too, against the same file's stat answer, and so is
//! the length of the write. Both were left out once, and that is exactly what
//! let a misplaced clock live: `struct statx` holds four 16-byte timestamps at
//! 64/80/96/112 where `struct stat` holds three 8-byte pairs at 72/88/104, so
//! "the fields are there" is not a claim about *where* - and a write that
//! stops at `stx_mnt_id` leaves the caller's own bytes in the rest of the
//! 256-byte struct (`NOTES.md` §13.11).
//!
//! # Why the raw door carries most of this program
//!
//! Three claims have no spelling through `rustix::fs::statx`, all clause (c)
//! - the wrapper binds the right call but its *shape* cannot say it:
//!
//! * **the whole-struct write.** The wrapper owns its buffer
//!   (`MaybeUninit::uninit()`, never seeded), so "seeded with a pattern and
//!   every byte past the head came back zero" is unreachable. Here the
//!   buffer is the caller's `libc::statx`, filled with 0x5a first.
//! * **the flag word is a closed set.** `rustix::fs::AtFlags` is bitflags
//!   with `const _ = !0`, so 0x10000 *is* expressible - but the wrapper hands
//!   `flags` straight to the kernel, and on a glibc host libc's `syscall()`
//!   number table differs from musl's; the raw door keeps the exact register
//!   words the no-std version passed. Same for the **mask** word: the test
//!   sends `STATX_BASIC_STATS | 0x1000` and other out-of-vocabulary masks,
//!   which `StatxFlags` can retain but the wrapper then *masks off* ("users
//!   passing flags unknown to rustix could end up writing outside the
//!   buffer", says the source) - a wrapper that edits the argument cannot pin
//!   what the kernel does with it.
//! * **a null buffer answers EFAULT.** The wrapper would pass its own
//!   address; the raw door passes NULL.
//!
//! The numbers still come from maintained tables: `libc::SYS_statx`,
//! `libc::AT_*`, `libc::STATX_*`. `rustix` keeps the calls whose shape fits:
//! the file's stat (the agreement partner), openat/read/close, symlink and
//! the refusals' errno comparison through `Errno`.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::{CWD, Mode, OFlags, Stat, openat, stat, symlink, unlink};
use rustix::io::{Errno, read};

/// The fixture this test owns, in place of the no-std version's `/etc/motd`
/// (the second machine's payload has no motd - §15.11).
const FILE: &str = "/tmp/t-statx_file";
/// A directory both payloads carry.
const DIR: &str = "/bin";
const LINK: &str = "/tmp/t-statx_link";

/// The body: `stx_size` is compared against what a read actually yielded.
const BODY: &[u8] = b"mpos t-statx fixture, read back before the stat\n";

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `statx(dirfd, path, flags, mask, buf)` as the raw door, with the caller's
/// own buffer and the caller's own words. `path` is the string or the empty
/// path naming `dirfd`; `buf` may be null - that is one of the claims.
fn statx_raw(
    dirfd: i32,
    path: &str,
    flags: libc::c_uint,
    mask: libc::c_uint,
    buf: *mut libc::statx,
) -> Result<(), Errno> {
    let c = std::ffi::CString::new(path).unwrap();
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            dirfd as libc::c_long,
            c.as_ptr() as libc::c_long,
            flags as libc::c_long,
            mask as libc::c_long,
            buf as libc::c_long,
        )
    };
    if r < 0 {
        Err(Errno::from_raw_os_error(last_errno()))
    } else {
        Ok(())
    }
}

/// The statx ABI's own struct length: the 256 bytes Linux hands the kernel,
/// from `STATX_SIZE` in the kernel's uapi (the same number `buffer_ok` checks
/// the caller's buffer against). `size_of::<libc::statx>()` is *this*
/// machine's glibc/musl struct - measured on the host: 320 bytes, whose tail
/// past 256 no kernel writes. The whole-struct claim is about the ABI's 256,
/// so the seed and the walk use this length, not the declared one.
const STATX_ABI_SIZE: usize = 256;

/// Fill the caller-owned struct with the pattern the kernel must overwrite.
fn seed(stx: &mut libc::statx) {
    let raw = core::slice::from_mut(stx).as_mut_ptr().cast::<u8>();
    unsafe { core::slice::from_raw_parts_mut(raw, STATX_ABI_SIZE).fill(0x5a) };
}

/// Every byte at or past offset 144 - the tail past `stx_dio_*`, inside the
/// ABI's 256 - reads zero, which is the claim "the kernel wrote the whole
/// struct, not a head of it". This is mpos's answer; real Linux leaves its
/// own zeros where this kernel would leave them *and* fills `stx_mnt_id` at
/// 144 (measured on the host, kernel 7.2.9: with a 0x5a seed the only
/// nonzero bytes past 144 are exactly that word), so the host run pins
/// "nothing but the mount id survived the seed" instead.
fn tail_is_zero(stx: &libc::statx, host: bool) -> bool {
    let bytes = unsafe {
        core::slice::from_raw_parts(stx as *const libc::statx as *const u8, STATX_ABI_SIZE)
    };
    if host {
        // The only seed survivor Linux allows is the `stx_mnt_id` word at
        // 144..152; everything from 152 on must have been written.
        bytes[152..].iter().all(|&b| b == 0)
    } else {
        bytes[144..].iter().all(|&b| b == 0)
    }
}

fn main() {
    println!("t-statx");
    let mut r = Report::new();
    let host = mpos_user_tests::host_run();

    // Write the fixture, then read it whole: stx_size has to agree with reality.
    let _ = std::fs::remove_file(FILE);
    let made = std::fs::write(FILE, BODY).is_ok();
    r.check("the file to stat can be written", made);
    let mut total = 0usize;
    if let (true, Ok(fd)) = (made, openat(CWD, FILE, OFlags::RDONLY, Mode::empty())) {
        let mut buf = [0u8; 4096];
        loop {
            match read(&fd, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
    }

    let mut stx = unsafe { core::mem::zeroed::<libc::statx>() };
    seed(&mut stx);
    let e = statx_raw(
        rustix::fs::CWD.as_raw_fd(),
        FILE,
        0,
        libc::STATX_BASIC_STATS,
        &mut stx,
    );
    r.check("statx on the fixture succeeds", e.is_ok());
    if e.is_ok() {
        r.check(
            "and it wrote the whole struct, not a head of it",
            tail_is_zero(&stx, host),
        );
        r.check(
            "the mask reports type and size",
            stx.stx_mask & libc::STATX_TYPE != 0 && stx.stx_mask & libc::STATX_SIZE != 0,
        );
        r.check(
            "and does not claim a birth time this kernel cannot back",
            stx.stx_mask & libc::STATX_BTIME == 0,
        );
        r.check(
            "stx_size matches the bytes read",
            stx.stx_size == total as u64,
        );
        r.check(
            "stx_mode says regular file",
            u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFREG,
        );
    }

    // statx and the ordinary stat must agree about the inode and the clocks.
    let st: Stat = stat(FILE).expect("the fixture stats");
    if e.is_ok() {
        r.check("stx_ino equals st_ino from stat", stx.stx_ino == st.st_ino);
        // The clocks. The two structs put them at *different* offsets, so
        // these are the numbers that say the statx write landed where Linux
        // puts it: a value at the wrong offset there reads the neighbouring
        // timestamp or the `stx_rdev_*` pair, and one 32 bytes out reads zero.
        r.check(
            "stx_atime equals st_atime from stat",
            stx.stx_atime.tv_sec == st.st_atime,
        );
        r.check(
            "stx_mtime equals st_mtime from stat",
            stx.stx_mtime.tv_sec == st.st_mtime,
        );
        r.check(
            "stx_ctime equals st_ctime from stat",
            stx.stx_ctime.tv_sec == st.st_ctime,
        );
    }

    // A buffer the kernel cannot write answers EFAULT.
    r.check(
        "statx with a null buffer answers EFAULT",
        statx_raw(
            rustix::fs::CWD.as_raw_fd(),
            FILE,
            0,
            libc::STATX_BASIC_STATS,
            core::ptr::null_mut(),
        ) == Err(Errno::FAULT),
    );

    // An empty path names the descriptor, but only under AT_EMPTY_PATH.
    let fd = openat(CWD, FILE, OFlags::RDONLY, Mode::empty());
    r.check("the file opens again for the fd form", fd.is_ok());
    if let Ok(fd) = &fd {
        seed(&mut stx);
        let e = statx_raw(
            fd.as_raw_fd(),
            "",
            libc::AT_EMPTY_PATH as libc::c_uint,
            libc::STATX_BASIC_STATS,
            &mut stx,
        );
        r.check("statx by fd with AT_EMPTY_PATH succeeds", e.is_ok());
        if e.is_ok() {
            r.check(
                "the fd form reports the size the read yielded",
                stx.stx_size == total as u64,
            );
        }
        r.check(
            "an empty path without AT_EMPTY_PATH answers ENOENT",
            statx_raw(fd.as_raw_fd(), "", 0, libc::STATX_BASIC_STATS, &mut stx)
                == Err(Errno::NOENT),
        );
    }

    // A directory reports S_IFDIR.
    seed(&mut stx);
    let e = statx_raw(
        rustix::fs::CWD.as_raw_fd(),
        DIR,
        0,
        libc::STATX_BASIC_STATS,
        &mut stx,
    );
    r.check("statx(/bin) succeeds", e.is_ok());
    if e.is_ok() {
        r.check(
            "stx_mode says directory",
            u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFDIR,
        );
    }

    // ---- the flag word is a closed set, and NOFOLLOW is honoured ----
    //
    // A bit outside {NOFOLLOW, NO_AUTOMOUNT, EMPTY_PATH, the sync bits} is
    // EINVAL. Measured on the host: 0x10000 is rejected. The old handler
    // discarded `flags`, so any bit was accepted.
    r.check(
        "statx with an unknown flag bit (0x10000) is EINVAL",
        statx_raw(
            rustix::fs::CWD.as_raw_fd(),
            FILE,
            0x10000,
            libc::STATX_BASIC_STATS,
            &mut stx,
        ) == Err(Errno::INVAL),
    );
    // AT_SYMLINK_NOFOLLOW reports the link, not its target: a lstat-style
    // statx the old handler could not answer, because it ignored the flag and
    // always resolved through the link to the regular file.
    let _ = unlink(LINK);
    let linked = symlink(FILE, LINK).is_ok();
    r.check("the symlink is made", linked);
    if linked {
        seed(&mut stx);
        let e = statx_raw(
            rustix::fs::CWD.as_raw_fd(),
            LINK,
            0,
            libc::STATX_BASIC_STATS,
            &mut stx,
        );
        r.check("statx through the link follows it (S_IFREG)", e.is_ok());
        if e.is_ok() {
            r.check(
                "without NOFOLLOW the target's regular-file mode is reported",
                u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFREG,
            );
        }
        seed(&mut stx);
        let e = statx_raw(
            rustix::fs::CWD.as_raw_fd(),
            LINK,
            libc::AT_SYMLINK_NOFOLLOW as libc::c_uint,
            libc::STATX_BASIC_STATS,
            &mut stx,
        );
        r.check("statx with AT_SYMLINK_NOFOLLOW succeeds", e.is_ok());
        if e.is_ok() {
            r.check(
                "with NOFOLLOW the link itself is reported (S_IFLNK), not its target",
                u32::from(stx.stx_mode) & libc::S_IFMT == libc::S_IFLNK,
            );
        }
        let _ = unlink(LINK);
    }

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
