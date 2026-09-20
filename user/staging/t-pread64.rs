//! t-pread64 - reading and writing at a position the caller names.
//!
//! The promise of the offset pair is the negative half: `pread` must not
//! move the descriptor's position. A program interleaves positioned and
//! sequential reads on one descriptor - a database reading a page header
//! between two sequential scans, a loader reading a symbol table - and a
//! `pread` that leaves the position where it read silently skips bytes.
//!
//! So the test does exactly that: read, read at an offset, read again, and
//! check the third read picks up where the first left off. Then the write
//! half, in the same shape: `pwrite` in the middle of a file must not
//! disturb what a following `write` appends to the end.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const EINVAL: isize = -22;
const EBADF: isize = -9;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-pread64\n");
    let mut failed = false;

    // A file with the alphabet in it, so every offset names a known byte.
    let alphabet = b"abcdefghij";
    let fd = lin::openat(
        lin::AT_FDCWD,
        b"/tmp/pr_file",
        lin::O_CREAT | lin::O_RDWR,
        0o644,
    );
    lin::check_true(&mut failed, b"the test file opens", fd >= 0);
    if fd < 0 {
        lin::done(true);
    }
    let fd = fd as i32;
    lin::check_eq(
        &mut failed,
        b"the whole alphabet is written",
        lin::write(fd, alphabet),
        10,
    );

    // --- pread does not move the position -------------------------------
    let mut buf = [0u8; 4];
    let n = lin::read(fd, &mut buf[..3]);
    lin::check_eq(&mut failed, b"a plain read gets three bytes", n, 3);
    lin::check_true(&mut failed, b"and they are abc", &buf[..3] == b"abc");

    // Read the same three bytes again by position: same answer, and the
    // descriptor must not have noticed.
    let mut at = [0u8; 3];
    let n = lin::pread(fd, &mut at, 0);
    lin::check_eq(&mut failed, b"pread at 0 returns three bytes", n, 3);
    lin::check_true(&mut failed, b"and they are abc too", at == *b"abc");

    // The next sequential read: still at 3, because pread left it alone.
    let mut next = [0u8; 3];
    let n = lin::read(fd, &mut next);
    lin::check_eq(&mut failed, b"the read after a pread continues", n, 3);
    lin::check_true(
        &mut failed,
        b"and it reads def, not abc - pread did not move the position",
        next == *b"def",
    );

    // Reading at an offset past the end is the end of the file, not an
    // error and not a short read from the start.
    let mut at_end = [0u8; 4];
    let n = lin::pread(fd, &mut at_end, 100);
    lin::check_eq(&mut failed, b"pread past the end returns 0", n, 0);

    // A negative offset is not a position.
    let mut bad = [0u8; 4];
    let r = lin::pread(fd, &mut bad, -1);
    lin::check_eq(
        &mut failed,
        b"pread with a negative offset is EINVAL",
        r,
        EINVAL,
    );

    // --- pwrite, in the same shape --------------------------------------
    // Overwrite the middle by position; the descriptor's own position is
    // wherever the reads above left it, and must stay there.
    let pos_before = lin::lseek(fd, 0, lin::SEEK_CUR);
    lin::check_true(
        &mut failed,
        b"the position is 6 after three reads of three",
        pos_before == 6,
    );
    let n = lin::pwrite(fd, b"XYZ", 1);
    lin::check_eq(&mut failed, b"pwrite writes three bytes", n, 3);
    lin::check_eq(
        &mut failed,
        b"and leaves the position where it was",
        lin::lseek(fd, 0, lin::SEEK_CUR),
        6,
    );

    // The write landed in the middle: aXYZefghij.
    let mut whole = [0u8; 16];
    let n = lin::pread(fd, &mut whole, 0);
    lin::check_eq(&mut failed, b"the whole file reads back", n, 10);
    lin::check_true(
        &mut failed,
        b"pwrite changed the middle, not the start or the end",
        &whole[..10] == b"aXYZefghij",
    );

    let r = lin::pwrite(fd, b"Q", -1);
    lin::check_eq(
        &mut failed,
        b"pwrite with a negative offset is EINVAL",
        r,
        EINVAL,
    );

    // --- a descriptor that is not open ----------------------------------
    let mut buf = [0u8; 4];
    let r = lin::pread(fd + 100, &mut buf, 0);
    lin::check_eq(&mut failed, b"pread on a closed fd is EBADF", r, EBADF);
    let r = lin::pwrite(fd + 100, b"x", 0);
    lin::check_eq(&mut failed, b"pwrite on a closed fd is EBADF", r, EBADF);

    lin::close(fd);
    let _ = lin::unlinkat(lin::AT_FDCWD, b"/tmp/pr_file", 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-pread64: panicked\n")
}
