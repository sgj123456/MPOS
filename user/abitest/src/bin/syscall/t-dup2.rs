//! t-dup2 - one test for dup2, the syscall every redirection is built
//! from: `dup2(1, 5)` and friends in the shell's plumbing.
//!
//! Pinned here: the copy lands exactly on newfd (returned, even one far
//! above the current table), oldfd == newfd on an open descriptor is the
//! POSIX no-op that answers newfd, writes through the duplicate reach the
//! same console and report every byte, and an oldfd that is not open
//! answers EBADF.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static MOTD: [u8; 10] = *b"/etc/motd\0";
static LINE: &[u8; 9] = b"via fd40\n";

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dup2\n");
    let mut failed = false;
    // Which number the lowest free descriptor is depends on what this process
    // inherited, so measure it rather than assuming 3.
    let base = lin::free_fd();

    let a = lin::openat(lin::AT_FDCWD, &MOTD, lin::O_RDONLY, 0);
    lin::check_eq(
        &mut failed,
        b"the file opens on the lowest free fd",
        a,
        base,
    );

    let r = lin::dup2(1, 5);
    lin::check_eq(&mut failed, b"dup2(stdout, 5) returns 5", r, 5);

    let r = lin::dup2(base as i32, base as i32);
    lin::check_eq(
        &mut failed,
        b"dup2(fd, fd) on an open fd is the no-op that returns it",
        r,
        base,
    );

    let r = lin::dup2(1, 40);
    lin::check_eq(
        &mut failed,
        b"dup2 onto a high number grows the table to 40",
        r,
        40,
    );
    let n = lin::write(40, LINE);
    lin::check_eq(
        &mut failed,
        b"the duplicate writes to the console, every byte",
        n,
        LINE.len() as isize,
    );
    let r = lin::close(40);
    lin::check_eq(&mut failed, b"closing fd 40 succeeds", r, 0);

    let _ = lin::close(a as i32);
    let r = lin::dup2(base as i32, base as i32 + 3);
    lin::check_eq(
        &mut failed,
        b"an oldfd that is not open answers EBADF",
        r,
        lin::EBADF,
    );

    let _ = lin::close(5);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-dup2: panicked\n")
}
