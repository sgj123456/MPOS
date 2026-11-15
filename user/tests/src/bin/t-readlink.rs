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
use rustix::fs::{CWD, readlinkat_raw};
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

    // === how long a symlink target may be ==================================
    //
    // A target is a whole path, so its limit is `PATH_MAX - 1` (4095), not the
    // 255 a single path *component* is capped at. Measured on Linux 7.3.4
    // (`build/scratch/symcap/t.c`): 4095 is created and reads back at 4095,
    // while 4096 and 4097 are `ENAMETOOLONG` (36).
    //
    // This pair is the claim a `PATH_MAX` version and a `MAX_PATH` version each
    // fail on a different side: this kernel used the 255-byte component cap, so
    // a link a real filesystem can hold was refused by the door that makes it -
    // and the *same* number now bounds the `size` an inode on disk may claim,
    // which is a length the kernel allocates at (§8.0.2's shape).
    let long_dir = "/tmp/t-readlink_long";
    // Report *where* the fixture landed and whether it could be made: the same
    // claim fails on a filesystem whose `symlink` refuses for a reason of its
    // own, and a lane that says which step failed is readable where a bare
    // errno is not.
    let made_dir = std::fs::create_dir(long_dir);
    println!(
        "note: long-target fixture dir {long_dir}: {made_dir:?} (a bare `AlreadyExists` is a \
         leftover from an earlier run in this lane)"
    );
    let symlink_to = |target: &[u8], name: &str| -> Result<(), Errno> {
        // SAFETY: both paths are NUL-terminated byte strings this test owns,
        // `target` deliberately is *not* (the call takes it as bytes + no NUL
        // inside, which is the uapi's shape for a target).
        unsafe {
            let mut t = std::vec::Vec::with_capacity(target.len() + 1);
            t.extend_from_slice(target);
            t.push(0);
            let mut n = std::vec::Vec::with_capacity(name.len() + 1);
            n.extend_from_slice(name.as_bytes());
            n.push(0);
            let rc = libc::symlink(
                t.as_ptr().cast::<libc::c_char>(),
                n.as_ptr().cast::<libc::c_char>(),
            );
            if rc == 0 {
                Ok(())
            } else {
                Err(Errno::from_raw_os_error(
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
                ))
            }
        }
    };

    // 4095-byte target: accepted, and reads back whole.
    let longest = std::vec![b't'; 4095];
    let at_bound = symlink_to(&longest, "/tmp/t-readlink_long/at");
    r.check(
        &format!(
            "a 4095-byte target is accepted (the PATH_MAX - 1 line), got {at_bound:?} \
             [dir={made_dir:?}]"
        ),
        at_bound.is_ok(),
    );
    if at_bound.is_ok() {
        let mut big = std::vec![0u8; 4096];
        let got = readlinkat_raw(CWD, "/tmp/t-readlink_long/at", &mut big[..]);
        r.check(
            &format!("and reads back at its own length, got {got:?}"),
            got == Ok(4095),
        );
    }

    // One byte and two bytes past it: ENAMETOOLONG, not EINVAL and not a
    // truncation. 4096 is `PATH_MAX` itself, which a "cap at PATH_MAX" version
    // would wrongly accept.
    for len in [4096usize, 4097] {
        let too_long = std::vec![b'u'; len];
        let name = format!("/tmp/t-readlink_long/over{len}");
        let got = symlink_to(&too_long, &name);
        r.check(
            &format!("a {len}-byte target is ENAMETOOLONG, got {got:?}"),
            got == Err(Errno::NAMETOOLONG),
        );
    }

    let _ = std::fs::remove_dir_all(long_dir);
    let _ = std::fs::remove_file(FILE);
    r.finish()
}
