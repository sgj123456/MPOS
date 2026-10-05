//! t-stat-buffer - the memory a stat call fills, and the bytes just past it.
//!
//! `struct stat` is the caller's buffer, and its size is the ABI's: 144 bytes on
//! x86_64, 128 on aarch64, whose three clocks are 16-byte `timespec`s rather
//! than x86_64's three 8-byte pairs. A kernel that fills the x86_64 length on
//! both machines writes 16 bytes past every aarch64 `struct stat` it is handed,
//! and what those bytes land on depends on whose memory follows: the libc's own
//! frame, when the libc does the stat - which is why this check hands the kernel
//! the buffer itself.
//!
//! That indirection is not an accident of one implementation. musl's
//! `__fstatat` - the one function behind `stat`, `lstat` and `fstat` - does the
//! syscall into a `struct stat` of *its own* on the stack and then copies the
//! fields out one by one, because on the machines where the kernel's layout and
//! the libc's differ that copy is the whole point. Measured here: a canary
//! beside a caller's `struct stat` survives a 144-byte aarch64 write, because
//! the kernel never touched the caller's buffer - the 16 bytes went into the
//! frame past musl's own. That is also why the overflow outlived every
//! program's own `stat` calls, and why what it finally broke was somewhere else
//! entirely: a non-empty `std::fs::remove_dir_all` jumped to `pc 0` inside the
//! `fdopendir` that musl's tree walk is built on (`PLAN.md` §13.10, and
//! `t-rmtree` for that end of it).
//!
//! So the claim pinned here is the *length* - canaries on both sides of a real
//! `libc::stat`, checked after each call - and the call is the raw one, with the
//! number from `libc`'s own per-arch table. The offsets are pinned by every
//! caller that reads a field back (`t-rmdir`'s type and permission reads, and on
//! aarch64 the boot's `MPOS_STAT_OK`).

use std::ffi::{CStr, CString};
use std::mem::{align_of, size_of};
use std::os::unix::io::AsRawFd;

/// The pattern the guards hold. No stat call writes it, and it is not a value
/// any field of a real stat could carry.
const CANARY: u64 = 0x5A5A_5A5A_5A5A_5A5A;

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

/// A `struct stat` in a heap block, with canaries of words either side.
///
/// The heap is the point rather than a convenience: the fault this pins is a
/// write *past* the struct, so a struct in this test's own frame would let the
/// bug kill the test before it could report anything. Here the extra bytes land
/// on the guard, and the length of the run says what happened.
struct Guarded {
    words: Vec<u64>,
    /// Where the struct begins, in words, and how many words it occupies.
    at: usize,
    len: usize,
}

impl Guarded {
    fn new() -> Guarded {
        assert!(
            align_of::<libc::stat>() <= size_of::<u64>(),
            "a struct stat with a wider alignment than the guard words"
        );
        let len = size_of::<libc::stat>().div_ceil(size_of::<u64>());
        let mut words = vec![CANARY; len + 8];
        // The struct's own bytes start zeroed, so a field the kernel did not
        // write is distinguishable from a canary the kernel overwrote.
        for word in &mut words[4..4 + len] {
            *word = 0;
        }
        Guarded { words, at: 4, len }
    }

    /// The address a stat call is handed.
    fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        // SAFETY: the block is at least `at + len` words long, 8-byte aligned,
        // and the struct's own alignment was checked above.
        unsafe { self.words.as_mut_ptr().add(self.at) as *mut libc::c_void }
    }

    /// The struct as the call left it.
    fn st(&self) -> &libc::stat {
        // SAFETY: as above; the call has returned, so nothing is writing it.
        unsafe { &*(self.words.as_ptr().add(self.at) as *const libc::stat) }
    }

    fn guards_hold(&self) -> bool {
        self.words[..self.at].iter().all(|w| *w == CANARY)
            && self.words[self.at + self.len..]
                .iter()
                .all(|w| *w == CANARY)
    }

    /// How many guard words the call overwrote, for the report.
    fn guards_broken(&self) -> usize {
        self.words[..self.at]
            .iter()
            .chain(self.words[self.at + self.len..].iter())
            .filter(|w| **w != CANARY)
            .count()
    }
}

/// The `S_IFMT` class of a mode, which is what says whether the answer is about
/// the thing that was named.
fn kind(mode: u32) -> u32 {
    mode & 0o170_000
}

const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

/// `fstat(fd, buf)`: the descriptor's own description, into the caller's buffer
/// - `SYS_fstat`, the number `libc`'s per-arch table gives for this machine.
fn raw_fstat(fd: libc::c_int, buf: *mut libc::c_void) -> isize {
    // The arguments go over as the syscall words they are: `musl`'s `syscall`
    // reads each one back as a full-width `syscall_arg_t`, so a narrower Rust
    // argument would leave the high half of the slot saying something else.
    //
    // SAFETY: `fd` is open and `buf` is as long as a `libc::stat` plus guards;
    // both outlive the call.
    unsafe { libc::syscall(libc::SYS_fstat, fd as libc::c_long, buf) as isize }
}

/// `newfstatat(dirfd, path, buf, flags)`: the path spelling, which is what
/// `lstat` (`AT_SYMLINK_NOFOLLOW`) and `fstat` (`AT_EMPTY_PATH`) both resolve to.
fn raw_newfstatat(
    dirfd: libc::c_int,
    path: &CStr,
    buf: *mut libc::c_void,
    flags: libc::c_int,
) -> isize {
    // SAFETY: `path` is a live C string and `buf` is as long as a `libc::stat`
    // plus guards; both outlive the call. The words are full-width for the
    // reason [`raw_fstat`] gives.
    unsafe {
        libc::syscall(
            libc::SYS_newfstatat,
            dirfd as libc::c_long,
            path.as_ptr(),
            buf,
            flags as libc::c_long,
        ) as isize
    }
}

fn main() {
    println!("t-stat-buffer");
    let mut r = Report { failed: false };

    // The premise the canary rests on: the struct the ABI defines - and so the
    // length the kernel is asked to fill - is this many bytes here.
    #[cfg(target_arch = "aarch64")]
    let abi_bytes = 128;
    #[cfg(target_arch = "x86_64")]
    let abi_bytes = 144;
    let size = size_of::<libc::stat>();
    r.check(
        &format!("the ABI's struct stat is {abi_bytes} bytes here (libc's is {size})"),
        size == abi_bytes,
    );

    // A tree this test owns, made and removed with the calls that are not under
    // test: `create_dir`/`write` and `remove_dir`/`remove_file` leave
    // `remove_dir_all` - and so the tree walk - to `t-rmtree`.
    let dir = "/tmp/t-stat-buffer";
    let file = "/tmp/t-stat-buffer-file";
    let link = "/tmp/t-stat-buffer-link";
    let _ = std::fs::remove_file(link);
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_dir(dir);
    if std::fs::create_dir(dir).is_err() || std::fs::write(file, b"x").is_err() {
        r.check("the scratch directory and file can be made", false);
        std::process::exit(1);
    }
    let linked = std::os::unix::fs::symlink(file, link).is_ok();

    let cdir = CString::new(dir).unwrap();
    let cfile = CString::new(file).unwrap();
    let clink = CString::new(link).unwrap();

    // lstat's syscall, into this process's own buffer: the answer is about the
    // name, which for a directory is the directory itself.
    let mut g = Guarded::new();
    let stat_dir = raw_newfstatat(
        libc::AT_FDCWD,
        &cdir,
        g.as_mut_ptr(),
        libc::AT_SYMLINK_NOFOLLOW,
    );
    r.check(
        &format!("newfstatat of a directory succeeds (got {stat_dir})"),
        stat_dir == 0,
    );
    r.check(
        &format!(
            "and the bytes past the struct are untouched ({} guard words written)",
            g.guards_broken()
        ),
        g.guards_hold(),
    );
    r.check(
        &format!(
            "and it reads back as a directory (mode {:#o})",
            g.st().st_mode
        ),
        kind(g.st().st_mode as u32) == S_IFDIR,
    );

    // The same call for a file: the same writer, a different answer in the
    // struct - and the fields a wrong length would shift.
    let mut g = Guarded::new();
    let stat_file = raw_newfstatat(
        libc::AT_FDCWD,
        &cfile,
        g.as_mut_ptr(),
        libc::AT_SYMLINK_NOFOLLOW,
    );
    r.check(
        &format!("newfstatat of a file succeeds (got {stat_file})"),
        stat_file == 0,
    );
    r.check(
        &format!(
            "and the bytes past the struct are untouched ({} guard words written)",
            g.guards_broken()
        ),
        g.guards_hold(),
    );
    r.check(
        &format!(
            "and it reads back as a regular file of one byte (mode {:#o}, size {})",
            g.st().st_mode,
            g.st().st_size
        ),
        kind(g.st().st_mode as u32) == S_IFREG && g.st().st_size == 1,
    );

    // lstat of a symlink names the link rather than what it points at, so the
    // same field carries a third kind.
    if linked {
        let mut g = Guarded::new();
        let stat_link = raw_newfstatat(
            libc::AT_FDCWD,
            &clink,
            g.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
        r.check(
            &format!("newfstatat of a symlink succeeds (got {stat_link})"),
            stat_link == 0,
        );
        r.check(
            &format!(
                "and the bytes past the struct are untouched ({} guard words written)",
                g.guards_broken()
            ),
            g.guards_hold(),
        );
        r.check(
            &format!("and it reads back as a link (mode {:#o})", g.st().st_mode),
            kind(g.st().st_mode as u32) == S_IFLNK,
        );
    }

    // fstat, and the descriptor spelling of the path call with it: the two
    // calls musl's `fdopendir` makes before it reads a directory - which is
    // where the overflow was finally felt.
    match std::fs::File::open(file) {
        Ok(f) => {
            let fd = f.as_raw_fd();
            let mut g = Guarded::new();
            let stat_fd = raw_fstat(fd, g.as_mut_ptr());
            r.check(
                &format!("fstat of an open file succeeds (got {stat_fd})"),
                stat_fd == 0,
            );
            r.check(
                &format!(
                    "and the bytes past the struct are untouched ({} guard words written)",
                    g.guards_broken()
                ),
                g.guards_hold(),
            );
            r.check(
                &format!(
                    "and it reads back as the same regular file (mode {:#o}, size {})",
                    g.st().st_mode,
                    g.st().st_size
                ),
                kind(g.st().st_mode as u32) == S_IFREG && g.st().st_size == 1,
            );

            let empty = CString::new("").unwrap();
            let mut g = Guarded::new();
            let stat_fd_path = raw_newfstatat(fd, &empty, g.as_mut_ptr(), libc::AT_EMPTY_PATH);
            r.check(
                &format!("newfstatat with an empty path names the descriptor (got {stat_fd_path})"),
                stat_fd_path == 0,
            );
            r.check(
                &format!(
                    "and the bytes past the struct are untouched ({} guard words written)",
                    g.guards_broken()
                ),
                g.guards_hold(),
            );
        }
        Err(e) => r.check(&format!("the scratch file can be opened (got {e})"), false),
    }

    // The tidy-up, with the calls that are not under test.
    let _ = std::fs::remove_file(link);
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_dir(dir);

    if r.failed {
        println!("t-stat-buffer: FAILED");
        std::process::exit(1);
    }
    println!("t-stat-buffer: all passed");
}
