//! t-at-empty-name - the empty name, door by door, across the whole `*at` family.
//!
//! An empty name is answered by the **name**: `ENOENT`, before the descriptor is
//! asked anything about it - neither whether it is open nor what kind it is.
//! Measured on a current host before this landed (`.workbuddy/probe_at_dirfd.c`,
//! Linux 7.2.9, every call through `syscall(2)`): `newfstatat(1000, "", &st, 0)`
//! with `1000` not open answers `ENOENT`, and so does the same call through a
//! dirfd open on a regular file - where the *relative* name beside it answers
//! `EBADF` and `ENOTDIR` respectively. This kernel reached the descriptor first,
//! so an empty name answered `EBADF` or `ENOTDIR`, which is a statement about
//! the descriptor where Linux's is about the name. The three shapes are pinned
//! below for every door the family reaches this kernel through.
//!
//! `readlinkat` is the one door that *does* consult its descriptor first, and
//! the difference is Linux's own: `do_readlinkat` has no empty-name check to
//! place in front of its walk, so the same empty name answers `EBADF` while the
//! descriptor is not open and `ENOENT` once it is - a regular file included,
//! which is why its `ENOENT` is not `base_for`'s kind check answering. All three
//! are pinned below, at the end.
//!
//! # Why every call here is a raw `libc::syscall`
//!
//! Two reasons, and neither is convenience. `rustix` has no wrapper for three of
//! these doors (`openat2`, `faccessat2`, `fchmodat2`), and the descriptors this
//! program must hand over are numbers *nothing is open on* - which
//! `BorrowedFd`'s contract forbids naming, so a typed call cannot spell the
//! claim at all (`user/tests/Cargo.toml`'s shape clauses). The numbers are
//! `libc`'s own table and so are the machine's; `futimesat` has no arm64 number
//! at all and its claims are `x86_64`'s alone.
//!
//! # What this program is not
//!
//! The *relative* name's contrast (`EBADF` from a closed dirfd, `ENOTDIR` from a
//! regular-file one) is `t-newfstatat`'s and `t-openat`'s subject, and the
//! absolute name's - which stops the descriptor being a question at all - is
//! those two plus `t-link`. This program owns the empty name alone, so that each
//! of the family's three answers has one home.

use mpos_user_tests::Report;
use std::ffi::CString;

/// The file whose descriptor is this program's "dirfd that is not a directory",
/// and the directory that is the ordinary one. Both are fixtures this program
/// makes or opens itself: the claim is about what the door does with an empty
/// name, not about what an image happens to carry.
const FILE: &str = "/tmp/t-at-empty-name-file";
const DIR: &str = "/tmp";

/// A descriptor number nothing is open on. `t-write`, `t-chdir` and the rest of
/// the suite write one down the same way, because one branch answers every
/// number that names nothing.
const BAD_FD: i32 = 77;

/// `fchmodat2`'s number, written down because `libc` leaves it out of its
/// `aarch64`-musl table - see that door below.
const SYS_FCHMODAT2: libc::c_long = 452;

/// One door of the family: called with the caller's dirfd, an *empty* name in
/// the slot that door takes a name, and everything else fixed and harmless. The
/// answer is the syscall's own - `0` or a descriptor on success, a negative
/// errno otherwise.
type Door = fn(i32) -> i64;

/// A NUL-terminated literal for the raw calls. The literals below hold no NUL.
fn c(p: &str) -> CString {
    CString::new(p).expect("a literal with no NUL in it")
}

/// The errno a door answered, or `None` when it succeeded - the success arm is
/// not expected anywhere below, but a door that *did* open something would leak
/// the descriptor into every claim after it, so it is closed rather than ignored.
///
/// `libc::syscall` speaks the *libc* protocol rather than the kernel's: it
/// answers -1 with `errno` set, so the number is read back here rather than
/// taken from the return value (`t-fchmodat2`'s helper negates its own `errno`
/// for the same reason, and the C probe in `.workbuddy/` prints it beside each
/// call).
fn refused(value: i64) -> Option<i32> {
    if value >= 0 {
        unsafe { libc::close(value as i32) };
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
}

fn newfstatat(fd: i32) -> i64 {
    let mut st = [0u64; 32];
    unsafe {
        libc::syscall(
            libc::SYS_newfstatat,
            fd,
            c("").as_ptr(),
            st.as_mut_ptr(),
            0usize,
        ) as i64
    }
}

fn statx(fd: i32) -> i64 {
    let mut st = [0u64; 64]; // `struct statx` is 256 bytes on both machines
    unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c("").as_ptr(),
            0usize,
            0x7ffu32 as usize, // STATX_BASIC_STATS
            st.as_mut_ptr(),
        ) as i64
    }
}

fn openat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_openat, fd, c("").as_ptr(), libc::O_RDONLY, 0usize) as i64 }
}

fn openat2(fd: i32) -> i64 {
    // `struct open_how` is three `__u64`s, and 24 is its size.
    let how = [libc::O_RDONLY as u64, 0, 0];
    unsafe { libc::syscall(libc::SYS_openat2, fd, c("").as_ptr(), how.as_ptr(), 24usize) as i64 }
}

fn faccessat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_faccessat, fd, c("").as_ptr(), libc::F_OK) as i64 }
}

fn faccessat2(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_faccessat2, fd, c("").as_ptr(), libc::F_OK, 0usize) as i64 }
}

fn utimensat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_utimensat, fd, c("").as_ptr(), 0usize, 0usize) as i64 }
}

#[cfg(target_arch = "x86_64")]
fn futimesat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_futimesat, fd, c("").as_ptr(), 0usize) as i64 }
}

fn mkdirat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_mkdirat, fd, c("").as_ptr(), 0o755usize) as i64 }
}

fn unlinkat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_unlinkat, fd, c("").as_ptr(), 0usize) as i64 }
}

fn symlinkat(fd: i32) -> i64 {
    // The *link's* name is the empty one; the target is stored, not resolved.
    unsafe {
        libc::syscall(
            libc::SYS_symlinkat,
            c("/etc/passwd").as_ptr(),
            fd,
            c("").as_ptr(),
        ) as i64
    }
}

fn renameat(fd: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_renameat,
            fd,
            c("").as_ptr(),
            libc::AT_FDCWD,
            c("t-at-empty-name-to").as_ptr(),
        ) as i64
    }
}

fn renameat2(fd: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            fd,
            c("").as_ptr(),
            libc::AT_FDCWD,
            c("t-at-empty-name-to").as_ptr(),
            0usize,
        ) as i64
    }
}

fn linkat(fd: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_linkat,
            fd,
            c("").as_ptr(),
            libc::AT_FDCWD,
            c("t-at-empty-name-link").as_ptr(),
            0usize,
        ) as i64
    }
}

fn fchmodat(fd: i32) -> i64 {
    unsafe { libc::syscall(libc::SYS_fchmodat, fd, c("").as_ptr(), 0o600usize) as i64 }
}

fn fchmodat2(fd: i32) -> i64 {
    // `libc`'s aarch64-musl table leaves `SYS_fchmodat2` out while defining it
    // for x86_64, so the number is written down the way `t-fchmodat2` writes it
    // down for the same door: `452` on both machines (`nr_x86_64.rs` /
    // `nr_aarch64.rs`).
    unsafe { libc::syscall(SYS_FCHMODAT2, fd, c("").as_ptr(), 0o600usize, 0usize) as i64 }
}

fn fchownat(fd: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_fchownat,
            fd,
            c("").as_ptr(),
            libc::getuid() as usize,
            libc::getgid() as usize,
            0usize,
        ) as i64
    }
}

fn mknodat(fd: i32) -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_mknodat,
            fd,
            c("").as_ptr(),
            (libc::S_IFIFO | 0o644) as usize,
            0usize,
        ) as i64
    }
}

fn readlinkat(fd: i32) -> i64 {
    let mut buf = [0u8; 64];
    let len = buf.len();
    unsafe {
        libc::syscall(
            libc::SYS_readlinkat,
            fd,
            c("").as_ptr(),
            buf.as_mut_ptr(),
            len,
        ) as i64
    }
}

fn main() {
    println!("t-at-empty-name");
    let mut r = Report::new();

    let _ = std::fs::remove_file(FILE);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(FILE)
    {
        Ok(file) => file,
        Err(e) => {
            r.check(&format!("the fixture opens (got {e})"), false);
            r.finish()
        }
    };
    let filefd = {
        use std::os::fd::AsRawFd;
        file.as_raw_fd()
    };
    let dir = match std::fs::File::open(DIR) {
        Ok(dir) => dir,
        Err(e) => {
            r.check(&format!("{DIR} opens as a descriptor (got {e})"), false);
            r.finish()
        }
    };
    let dirfd = {
        use std::os::fd::AsRawFd;
        dir.as_raw_fd()
    };

    // Every door but `readlinkat`, each through the two descriptors an empty
    // name must not be answered by.
    let doors: &[(&str, Door)] = &[
        ("newfstatat", newfstatat),
        ("statx", statx),
        ("openat", openat),
        ("openat2", openat2),
        ("faccessat", faccessat),
        ("faccessat2", faccessat2),
        ("utimensat", utimensat),
        ("mkdirat", mkdirat),
        ("unlinkat", unlinkat),
        ("symlinkat", symlinkat),
        ("renameat", renameat),
        ("renameat2", renameat2),
        ("linkat", linkat),
        ("fchmodat", fchmodat),
        ("fchmodat2", fchmodat2),
        ("fchownat", fchownat),
        ("mknodat", mknodat),
    ];
    for (name, door) in doors {
        r.check(
            &format!("{name}: an empty name with a dirfd nothing is open on is ENOENT"),
            refused(door(BAD_FD)) == Some(libc::ENOENT),
        );
        r.check(
            &format!("{name}: and through a dirfd open on a regular file too"),
            refused(door(filefd)) == Some(libc::ENOENT),
        );
    }

    // The second machine has no `futimesat` number, so its claims are the first
    // machine's alone - said out loud rather than left out silently.
    #[cfg(target_arch = "x86_64")]
    {
        r.check(
            "futimesat: an empty name with a dirfd nothing is open on is ENOENT",
            refused(futimesat(BAD_FD)) == Some(libc::ENOENT),
        );
        r.check(
            "futimesat: and through a dirfd open on a regular file too",
            refused(futimesat(filefd)) == Some(libc::ENOENT),
        );
    }

    // The answer that must not change while the two above are added: an empty
    // name through a *directory* is refused for the same reason, and it was
    // already `ENOENT` before (from the walk) - what moved is only which
    // question answers it.
    r.check(
        "newfstatat: an empty name through a directory dirfd is ENOENT",
        refused(newfstatat(dirfd)) == Some(libc::ENOENT),
    );
    r.check(
        "openat: an empty name through a directory dirfd is ENOENT",
        refused(openat(dirfd)) == Some(libc::ENOENT),
    );

    // `readlinkat`, whose descriptor is asked *first*: EBADF while it is not
    // open, and then the empty name's own ENOENT - through a regular file as
    // well, where a kind check would have answered ENOTDIR.
    r.check(
        "readlinkat: the descriptor is asked first, so a closed one is EBADF",
        refused(readlinkat(BAD_FD)) == Some(libc::EBADF),
    );
    r.check(
        "readlinkat: an empty name through a regular-file dirfd is ENOENT, not ENOTDIR",
        refused(readlinkat(filefd)) == Some(libc::ENOENT),
    );
    r.check(
        "readlinkat: and through a directory dirfd too",
        refused(readlinkat(dirfd)) == Some(libc::ENOENT),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
