//! t_fcntl - one test for fcntl, which Rust std's startup uses on fds 0-2
//! (`fcntl(0, F_GETFD) = 0` and friends in the strace).
//!
//! Pinned here: F_GETFD on a live descriptor answers the descriptor's flag
//! word (fresh: 0), F_SETFD stores FD_CLOEXEC and F_GETFD reads it back,
//! and a descriptor that does not exist answers EBADF. A stub that
//! answers 0 to everything fails the roundtrip by design - "accept and
//! forget" is not flag storage.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_fcntl\n");
    let mut failed = false;

    let r = lin::fcntl(1, lin::F_GETFD, 0);
    lin::check_eq(
        &mut failed,
        b"F_GETFD on stdout answers a clear flag word",
        r,
        0,
    );

    let r = lin::fcntl(1, lin::F_SETFD, lin::FD_CLOEXEC);
    lin::check_eq(
        &mut failed,
        b"F_SETFD stores FD_CLOEXEC",
        r,
        0,
    );

    let r = lin::fcntl(1, lin::F_GETFD, 0);
    lin::check_eq(
        &mut failed,
        b"F_GETFD reads the stored flag back",
        r,
        lin::FD_CLOEXEC as isize,
    );

    let r = lin::fcntl(1, lin::F_SETFD, 0);
    lin::check_eq(&mut failed, b"F_SETFD clears the flag again", r, 0);

    let r = lin::fcntl(-1, lin::F_GETFD, 0);
    lin::check_eq(
        &mut failed,
        b"a descriptor that does not exist answers EBADF",
        r,
        lin::EBADF,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_fcntl: panicked\n")
}
