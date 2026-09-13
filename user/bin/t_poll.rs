//! t_poll - one test for poll, exactly the shape of the strace's
//! `poll([{fd=0, events=0}, {fd=1, events=0}, {fd=2, events=0}], 3, 0) = 0`.
//!
//! Rust std's startup polls the standard descriptors with events=0 and
//! timeout=0 to sanitize them - checking they exist without asking anything
//! of them. Nothing is requested, so nothing can be ready: the call must
//! answer 0 and leave every revents clear. The test seeds revents with a
//! sentinel so a kernel that scribbles on the array cannot hide.
//!
//! Beyond the strace's call: an nfds of 0 answers 0 with no array at all,
//! a real POLLIN request on fd 1 still reports nothing ready and still
//! clears a seeded revents (mpos's pinned contract - poll never blocks and
//! nothing is ever reported ready), and a timeout of -1 changes neither:
//! the call answers 0 immediately whatever the caller asked to wait for.

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

    // nfds = 0: the call is trivially over before it starts.
    let empty: &mut [lin::PollFd] = &mut [];
    let r = lin::poll(empty, 0);
    lin::check_eq(
        &mut failed,
        b"poll with an nfds of 0 answers 0",
        r,
        0,
    );

    // A real request - POLLIN on fd 1 - cannot be satisfied, and the
    // out-parameter is cleared even so: the caller must not read its own
    // sentinel back as an answer.
    let mut one = [lin::PollFd { fd: 1, events: 1, revents: 0x5555 }];
    let r = lin::poll(&mut one, 0);
    lin::check_eq(
        &mut failed,
        b"poll asking POLLIN on fd 1 with timeout=0 answers 0",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"the POLLIN revents sentinel is cleared, never reported ready",
        one[0].revents == 0,
    );

    // A negative timeout means "wait forever" on Linux; here nothing ever
    // waits, so the call answers 0 immediately and clears revents the same.
    let mut one = [lin::PollFd { fd: 1, events: 1, revents: 0x5555 }];
    let r = lin::poll(&mut one, -1);
    lin::check_eq(
        &mut failed,
        b"poll with timeout=-1 answers 0 immediately",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"timeout=-1 clears the revents sentinel too",
        one[0].revents == 0,
    );

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t_poll: panicked\n")
}
