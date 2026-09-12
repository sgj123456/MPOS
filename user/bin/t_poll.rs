//! t_poll - one test for poll, exactly the shape of the strace's
//! `poll([{fd=0, events=0}, {fd=1, events=0}, {fd=2, events=0}], 3, 0) = 0`.
//!
//! Rust std's startup polls the standard descriptors with events=0 and
//! timeout=0 to sanitize them - checking they exist without asking anything
//! of them. Nothing is requested, so nothing can be ready: the call must
//! answer 0 and leave every revents clear. The test seeds revents with a
//! sentinel so a kernel that scribbles on the array cannot hide.

#![no_std]
#![no_main]

#[path = "../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t_poll\n");
    let mut failed = false;

    let mut fds = [
        lin::PollFd { fd: 0, events: 0, revents: 0x5a5a },
        lin::PollFd { fd: 1, events: 0, revents: 0x5a5a },
        lin::PollFd { fd: 2, events: 0, revents: 0x5a5a },
    ];
    let r = lin::poll(&mut fds, 0);
    lin::check_eq(
        &mut failed,
        b"poll over fds 0-2 with events=0, timeout=0 reports nothing ready",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"no revents come back for events=0",
        fds[0].revents == 0 && fds[1].revents == 0 && fds[2].revents == 0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_poll: panicked\n")
}
