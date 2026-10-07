//! t-pipe2 - one test for pipe2, the channel behind the shell's `|`.
//!
//! Pinned here: a flags = 0 call answers 0 and fills the pair with the two
//! lowest free descriptors, bytes written to fds[1] come back from fds[0]
//! whole, a flag word with bits beyond O_CLOEXEC|O_NONBLOCK answers EINVAL,
//! O_CLOEXEC alone is accepted, and once every write end is closed the
//! drained pipe reads 0 - the EOF every reader loop keys on.
//!
//! O_NONBLOCK is pinned with teeth: born through the flags word, an empty
//! read answers EAGAIN rather than parking the reader and a write to a full
//! pipe answers EAGAIN rather than parking the writer - which is the birth
//! state a self-pipe needs (sudo's signal channel was the caller dying on
//! its absence). `fcntl(F_SETFL)` grants the same state to a plain pipe
//! after the fact, and `F_GETFL`/`F_GETFD` report what the flags word left
//! on both ends.
//!
//! # The "lowest free fd" claim moved down a level
//!
//! The no-std version asked the raw door for numbers 3 and 4 relative to a
//! probed free slot. Under musl + Rust std the process owns stdio and the
//! allocator's own descriptors, so *which* numbers are free is runtime
//! business; what the ABI still promises - and what this pins now - is that
//! pipe2 hands back a fresh adjacent pair of valid descriptors, in read-then-
//! write order. The unknown-flag-word claim goes raw (rustix's PipeFlags is
//! a bitflags type, so the invalid word rides `from_bits_retain` - the rule
//! batch 8 corrected, costing no libc); the rest is pure rustix.

use mpos_user_tests::Report;
use rustix::fd::AsRawFd;
use rustix::fs::OFlags;
use rustix::io::{Errno, FdFlags, read, write};
use rustix::pipe::{PipeFlags, pipe_with};

const BIG_LEN: usize = 4096;

/// The door returns `-1` and leaves the refusal in `errno` (batch 12's
/// measurement); the claim is about what the kernel answered.
fn refused(ret: isize, want: i32) -> bool {
    ret == -1 && std::io::Error::last_os_error().raw_os_error() == Some(want)
}

fn main() {
    println!("t-pipe2");
    let mut r = Report::new();

    let (a, b) = match pipe_with(PipeFlags::empty()) {
        Ok(p) => p,
        Err(_) => {
            r.check("pipe2 with no flags succeeds", false);
            r.finish();
        }
    };
    r.check("pipe2 with no flags succeeds", true);
    // The pair is adjacent and the read end comes first - the shape a
    // self-pipe and every `dup2` idiom rely on.
    let (fa, fb) = (a.as_raw_fd(), b.as_raw_fd());
    r.check(
        "the pair is two adjacent descriptors, read end first",
        fb == fa + 1,
    );

    let n = write(&b, b"ping");
    r.check("the write end takes all 4 bytes", n == Ok(4));
    let mut buf = [0u8; 16];
    let n = read(&a, &mut buf);
    r.check("the read end hands the 4 bytes back", n == Ok(4));
    r.check(
        "the bytes that came back are the bytes written",
        n == Ok(4) && buf[..4] == *b"ping",
    );
    drop((a, b));

    // Flag bits beyond O_CLOEXEC|O_NONBLOCK answer EINVAL. rustix's own door
    // (`pipe_with`) takes a typed PipeFlags and cannot name the invalid word,
    // so this probe is the raw call - clause (c) on the shape, with the
    // number from libc's table (`SYS_pipe2`: 293 / 59).
    let mut fds = [-1i32; 2];
    let bad = unsafe {
        libc::syscall(
            libc::SYS_pipe2,
            &mut fds as *mut _ as usize,
            0xFFFF_FFFFusize,
        ) as isize
    };
    r.check(
        "flag bits beyond O_CLOEXEC|O_NONBLOCK answer EINVAL",
        refused(bad, libc::EINVAL),
    );

    let (a, b) = match pipe_with(PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            r.check("pipe2 with O_CLOEXEC succeeds", false);
            r.finish();
        }
    };
    r.check("pipe2 with O_CLOEXEC succeeds", true);
    r.check(
        "the new pair again takes two adjacent descriptors",
        b.as_raw_fd() == a.as_raw_fd() + 1,
    );

    drop(b);
    let mut buf = [0u8; 16];
    let n = read(&a, &mut buf);
    r.check("the drained pipe with no writers reads 0 (EOF)", n == Ok(0));
    drop(a);

    // O_NONBLOCK: the birth state a self-pipe asks for. An empty read with
    // the writer still alive answers EAGAIN rather than parking the reader.
    let (a, b) = match pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC) {
        Ok(p) => p,
        Err(_) => {
            r.check("pipe2 with O_NONBLOCK|O_CLOEXEC succeeds", false);
            r.finish();
        }
    };
    r.check("pipe2 with O_NONBLOCK|O_CLOEXEC succeeds", true);
    let mut buf = [0u8; 16];
    let n = read(&a, &mut buf);
    r.check(
        "an empty nonblocking read answers EAGAIN",
        n == Err(Errno::AGAIN),
    );
    let fl = rustix::fs::fcntl_getfl(&a);
    r.check(
        "F_GETFL reports the O_NONBLOCK the pipe was born with",
        matches!(fl, Ok(f) if f.contains(OFlags::NONBLOCK)),
    );
    let clo_a = rustix::io::fcntl_getfd(&a);
    let clo_b = rustix::io::fcntl_getfd(&b);
    r.check(
        "O_CLOEXEC set FD_CLOEXEC on both ends",
        matches!(&clo_a, Ok(f) if f.contains(FdFlags::CLOEXEC))
            && matches!(&clo_b, Ok(f) if f.contains(FdFlags::CLOEXEC)),
    );
    // Data flows as ever: the flag changes what a *wait* is, not what a
    // read or a write takes.
    let n = write(&b, b"pong");
    r.check("a nonblocking write with room succeeds", n == Ok(4));
    let mut buf = [0u8; 16];
    let n = read(&a, &mut buf);
    r.check(
        "the bytes came back whole",
        n == Ok(4) && buf[..4] == *b"pong",
    );
    // And the flag does not wedge the writer either: fill the pipe, and
    // once nothing more fits the write answers EAGAIN rather than waiting.
    let chunk = [0u8; BIG_LEN];
    let mut landed = 0usize;
    let mut blocked = false;
    for _ in 0..64 {
        match write(&b, &chunk) {
            Ok(n) => landed += n,
            Err(Errno::AGAIN) => {
                blocked = true;
                break;
            }
            Err(_) => break,
        }
    }
    r.check("a full pipe's nonblocking write answers EAGAIN", blocked);
    r.check(
        "the writes before it landed whole",
        landed >= BIG_LEN && landed.is_multiple_of(BIG_LEN),
    );
    drop((a, b));

    // `fcntl(F_SETFL)` grants the same state to a plain pipe after the
    // fact - the shape a program that cannot know about pipe2's flags word
    // uses, and the one a libc's `pipe()` plus `fcntl` idiom spells.
    let (a, b) = match pipe_with(PipeFlags::empty()) {
        Ok(p) => p,
        Err(_) => {
            r.check("a plain pipe2(0) still succeeds", false);
            r.finish();
        }
    };
    r.check("a plain pipe2(0) still succeeds", true);
    let fl = rustix::fs::fcntl_getfl(&a);
    r.check(
        "a plain pipe is born without O_NONBLOCK",
        matches!(fl, Ok(f) if !f.contains(OFlags::NONBLOCK)),
    );
    let set = rustix::fs::fcntl_setfl(&a, OFlags::NONBLOCK);
    r.check("F_SETFL sets O_NONBLOCK", set.is_ok());
    let mut buf = [0u8; 16];
    let n = read(&a, &mut buf);
    r.check(
        "F_SETFL's O_NONBLOCK makes an empty read answer EAGAIN",
        n == Err(Errno::AGAIN),
    );
    drop((a, b));

    r.finish()
}
