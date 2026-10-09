//! t-readlink - one test for readlink, which the strace shows uutils using
//! to locate itself: `readlink("/proc/self/exe", ..., 256) = 29`.
//!
//! Pinned here: the Linux argument shape - NUL-terminated path, buffer,
//! capacity, no length word; a successful read answers the byte count and
//! the content is an absolute path without a NUL; a zero-size buffer
//! answers EINVAL, a smaller buffer truncates silently to its size, an
//! existing path that is not a symlink answers EINVAL, and a missing one
//! ENOENT. On mpos, /proc/self/exe means the image path the task was
//! spawned from - the kernel knows it and must hand it back.
//!
//! # Why `readlinkat_raw` and not `readlink`
//!
//! `rustix::fs::readlink` binds the call but not the contract, the shape
//! `user/tests/Cargo.toml`'s `libc` rule calls clause (c): it takes a `Vec<u8>`
//! and returns a `CString`, growing the vector until the whole target fits, so
//! the *size the caller chose* - and with it the silent truncation and the
//! zero-size EINVAL this program is about - is outside it. The rule would send
//! that to `libc` except for what next to it exists: `readlinkat_raw` is the same
//! call with the caller's buffer, returning *how many bytes were initialised*,
//! so truncation is the answer rather than something the wrapper hid. Passing
//! `&mut [u8]` gives back a `usize` (its `Buffer` impls' contract), and a
//! `&mut [0u8; 0]` is a real zero-size buffer. That is `rustix` expressing the
//! claim, so the program needs no `libc`.
//!
//! # What this pins on each machine
//!
//! `readlink` has no arm64 number (`nr_aarch64.rs`: `READLINK = 64015; // no
//! arm64 number; unreachable on aarch64`), so musl implements it over
//! `readlinkat` there - and `rustix::fs::readlink`/`readlinkat_raw` are written
//! that way on every machine, measured in its linux_raw backend: both issue
//! `__NR_readlinkat`. So this program pins one handler on both machines and the
//! per-claim output is identical.
//!
//! `/proc/self/exe` is answered by the *path string*, not by a lookup: the
//! handler matches that literal name and hands back the path the task was
//! execve'd from, so it answers whether or not a procfs is mounted at `/proc`.
//! That is what lets the first three claims - the ones the no-std version was
//! written for - run on the second machine, whose test payload mounts no procfs
//! at all.
//!
//! The "not a symlink" fixture is a file this test writes itself under `/tmp`,
//! not the no-std version's `/etc/motd`: the second machine's sweep disk carries
//! no motd (`NOTES.md` §15.11), and the claim is about the kind of the name, not
//! about which file has it.

use mpos_user_tests::Report;
use rustix::fs::{CWD, Mode, OFlags, readlinkat_raw};
use rustix::io::Errno;

/// The link whose target is the running image's path - the name uutils reads to
/// find where it lives.
const EXE: &str = "/proc/self/exe";
/// The name this test writes as the "not a symlink" fixture, in place of the
/// no-std version's `/etc/motd`.
const FILE: &str = "/tmp/t-readlink_file";
const NO_SUCH: &str = "/no/such";

fn main() {
    println!("t-readlink");
    let mut r = Report::new();

    let mut buf = [0u8; 256];
    let n = readlinkat_raw(CWD, EXE, &mut buf[..]);
    r.check(
        "readlink(/proc/self/exe) answers the image path",
        matches!(n, Ok(n) if n > 0 && n <= buf.len()),
    );
    if let Ok(n) = n {
        r.check("the target is an absolute path", buf[0] == b'/');
        // A buffer one byte short truncates silently: Linux answers the
        // truncated length, not an error.
        if n > 1 {
            let m = readlinkat_raw(CWD, EXE, &mut buf[..n - 1]);
            r.check("a smaller buffer truncates the answer", m == Ok(n - 1));
        }
    }

    // A zero-size buffer is a caller error, not a zero-byte read.
    let mut empty = [0u8; 0];
    r.check(
        "readlink with a zero-size buffer answers EINVAL",
        readlinkat_raw(CWD, EXE, &mut empty) == Err(Errno::INVAL),
    );

    // An existing path that is not a symlink answers EINVAL. The fixture is made
    // here, so a failure to make it fails the claim rather than skipping it.
    let _ = std::fs::remove_file(FILE);
    let fixture = std::fs::write(FILE, b"mpos t-readlink fixture\n").is_ok();
    r.check(
        "readlink on a non-symlink answers EINVAL",
        fixture && readlinkat_raw(CWD, FILE, &mut buf[..]) == Err(Errno::INVAL),
    );

    r.check(
        "a path that does not exist answers ENOENT",
        readlinkat_raw(CWD, NO_SUCH, &mut buf[..]) == Err(Errno::NOENT),
    );

    let _ = std::fs::remove_file(FILE);
    r.finish()
}
