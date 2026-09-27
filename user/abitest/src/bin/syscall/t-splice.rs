//! t-splice - the kernel's byte pump between a pipe and a file.
//!
//! Pinned here, per splice(2): at least one end must be a pipe (a file to
//! file move is EINVAL), a pipe end refuses any non-null offset word (its
//! data is a stream, and the descriptor's own cursor is the only position
//! it has), the flag vocabulary is the SPLICE_F_* set and an unknown bit
//! is EINVAL, a missing descriptor is EBADF, and a zero length answers 0.
//! The file end is where the offsets mean something: the null-offset
//! spelling reads and writes through the descriptor's own cursor, so a
//! file-to-pipe splice leaves the file positioned after what it moved and
//! a pipe-to-file splice appends at wherever the file already stood.
//!
//! Two orderings are pinned with the rest, both read off the handler. The
//! flags word is validated before the length, so an unknown bit answers
//! EINVAL even at `len == 0`; and a legitimate zero-length call answers 0
//! without ever looking at the descriptors - Linux's own early `!len`
//! return, and not a divergence.
//!
//! mpos narrows the man page in one place this test does not reach: its
//! pump moves bytes only when at least one end is a pipe *inode*, so the
//! socket-to-pipe and pipe-to-socket forms Linux allows answer EINVAL
//! here. The file-to-pipe and pipe-to-file forms - the ones a `sendfile`
//! fallback and a shell's copy through a pipe actually use - are exact.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static PATH: &[u8] = b"/tmp/t-splice\0";
static PATH2: &[u8] = b"/tmp/t-splice2\0";

static mut BUF: [u8; 16] = [0; 16];

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-splice\n");
    let mut failed = false;

    let file = lin::openat(
        lin::AT_FDCWD,
        PATH,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o644,
    ) as i32;
    lin::check_true(&mut failed, b"the scratch file is created", file >= 0);
    lin::check_eq(
        &mut failed,
        b"the scratch file takes 6 bytes",
        lin::write(file, b"SPLICE"),
        6,
    );
    lin::check_eq(
        &mut failed,
        b"the scratch file seeks back to 0",
        lin::lseek(file, 0, lin::SEEK_SET),
        0,
    );

    let mut pipes: [i32; 2] = [-1; 2];
    lin::check_eq(
        &mut failed,
        b"the pipe pair is made",
        lin::pipe2(&mut pipes, 0),
        0,
    );

    // file -> pipe: the file's cursor names the range, and the splice
    // leaves it just past what it moved.
    let n = lin::splice(file, 0, pipes[1], 0, 6, 0);
    lin::check_eq(&mut failed, b"a file-to-pipe splice moves 6 bytes", n, 6);
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(pipes[0], buf);
    lin::check_eq(&mut failed, b"the pipe yields the 6 bytes spliced in", n, 6);
    lin::check_true(
        &mut failed,
        b"the bytes in the pipe are the bytes the file held",
        n == 6 && buf[..6] == *b"SPLICE",
    );

    // pipe -> file: the file end writes at its own cursor, so the file
    // grows by exactly the length spliced in and the bytes land there.
    lin::check_eq(
        &mut failed,
        b"the pipe takes 5 bytes to move out",
        lin::write(pipes[1], b"PIPED"),
        5,
    );
    let before = lin::lseek(file, 0, lin::SEEK_END);
    lin::check_eq(
        &mut failed,
        b"the file's end is at 6 before the splice",
        before,
        6,
    );
    let n = lin::splice(pipes[0], 0, file, 0, 5, 0);
    lin::check_eq(&mut failed, b"a pipe-to-file splice moves 5 bytes", n, 5);
    lin::check_eq(
        &mut failed,
        b"the file grew by the 5 bytes spliced in",
        lin::lseek(file, 0, lin::SEEK_END),
        before + 5,
    );
    lin::check_eq(
        &mut failed,
        b"the file seeks back to the spliced range",
        lin::lseek(file, before, lin::SEEK_SET),
        before,
    );
    let buf = unsafe { core::slice::from_raw_parts_mut(&raw mut BUF as *mut u8, 16) };
    let n = lin::read(file, buf);
    lin::check_true(
        &mut failed,
        b"the bytes that landed in the file are the bytes spliced",
        n == 5 && buf[..5] == *b"PIPED",
    );

    // A zero length is answered before any descriptor is touched, which
    // matches Linux's own early `!len` return rather than diverging from
    // it - the same file/pipe pair it moved through above.
    lin::check_eq(
        &mut failed,
        b"a zero length answers 0",
        lin::splice(file, 0, pipes[1], 0, 0, 0),
        0,
    );

    // The flags word is checked first, so an unknown bit is EINVAL even
    // when there is nothing to move.
    lin::check_eq(
        &mut failed,
        b"an unknown flag bit answers EINVAL",
        lin::splice(file, 0, pipes[1], 0, 1, 0x80),
        lin::EINVAL,
    );
    lin::check_eq(
        &mut failed,
        b"an unknown flag bit answers EINVAL even at length 0",
        lin::splice(file, 0, pipes[1], 0, 0, 0x80),
        lin::EINVAL,
    );

    // A pipe end takes no offset word at all: not a zero value, a null
    // pointer. The in end here is the pipe, so any non-null word is EINVAL.
    let mut zero: i64 = 0;
    lin::check_eq(
        &mut failed,
        b"a pipe end refuses a non-null offset word",
        lin::splice(pipes[0], &raw mut zero as usize, file, 0, 1, 0),
        lin::EINVAL,
    );

    // Neither end a pipe: the man page's own EINVAL.
    let file2 = lin::openat(
        lin::AT_FDCWD,
        PATH2,
        lin::O_CREAT | lin::O_RDWR | lin::O_TRUNC,
        0o644,
    ) as i32;
    lin::check_true(
        &mut failed,
        b"the second scratch file is created",
        file2 >= 0,
    );
    lin::check_eq(
        &mut failed,
        b"a file-to-file splice answers EINVAL",
        lin::splice(file, 0, file2, 0, 1, 0),
        lin::EINVAL,
    );

    // A descriptor the table does not hold: EBADF before the output end is
    // ever looked at.
    lin::check_eq(
        &mut failed,
        b"a closed input descriptor answers EBADF",
        lin::splice(99, 0, pipes[1], 0, 6, 0),
        lin::EBADF,
    );

    let _ = lin::close(pipes[0]);
    let _ = lin::close(pipes[1]);
    let _ = lin::close(file);
    let _ = lin::close(file2);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, PATH2, 0);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-splice: panicked\n")
}
