//! t-poll - one test for poll: does it report what is actually ready, and
//! does it wait when there is nothing to report?
//!
//! The strace's shape starts it off: Rust std sanitizes fds 0-2 at startup
//! with `events=0, timeout=0`, asking nothing, so nothing can be ready and
//! the answer is 0 with every revents cleared. The test seeds revents with a
//! sentinel, so a kernel that scribbles on the array cannot hide.
//!
//! Beyond that, this pins what a *working* poll does, because a shell is built
//! on it: `ash` polls stdin and reads what the poll says arrived, so a poll
//! that always answers "nothing" is a shell that decides its terminal hung up.
//!
//! * a writable descriptor asked for POLLOUT is ready, and says so;
//! * a pipe with nothing in it and a writer still attached is not ready, and
//!   with a timeout of 0 the call says so instead of waiting;
//! * once a byte lands in it, the same request is ready.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-poll\n");
    let mut failed = false;

    // The strace's call: nothing asked for, nothing ready.
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

    // A descriptor that can always be written to is ready, and the reason is
    // reported - not just "something happened".
    let mut one = [lin::PollFd { fd: 1, events: POLLOUT, revents: 0x5555 }];
    let r = lin::poll(&mut one, 0);
    lin::check_eq(
        &mut failed,
        b"stdout asked for POLLOUT is ready",
        r,
        1,
    );
    lin::check_true(
        &mut failed,
        b"the readiness comes back as POLLOUT, not the sentinel",
        one[0].revents == POLLOUT,
    );

    // A pipe with no data and a writer still attached: not readable, and a
    // timeout of 0 means "say so" rather than "wait".
    let mut pair: [i32; 2] = [-1; 2];
    let r = lin::pipe2(&mut pair, 0);
    lin::check_eq(&mut failed, b"pipe2 for the poll test succeeds", r, 0);
    let mut one = [lin::PollFd { fd: pair[0], events: POLLIN, revents: 0x5555 }];
    let r = lin::poll(&mut one, 0);
    lin::check_eq(
        &mut failed,
        b"an empty pipe with a live writer is not ready",
        r,
        0,
    );
    lin::check_true(
        &mut failed,
        b"and the revents sentinel is cleared rather than answered",
        one[0].revents == 0,
    );

    // Once something is in it, the same request is ready. This is the whole
    // point: a shell waits on this and reads what the poll named.
    let n = lin::write(pair[1], b"x");
    lin::check_eq(&mut failed, b"the byte lands in the pipe", n, 1);
    let mut one = [lin::PollFd { fd: pair[0], events: POLLIN, revents: 0x5555 }];
    let r = lin::poll(&mut one, 0);
    lin::check_eq(
        &mut failed,
        b"a pipe with a byte in it is ready to read",
        r,
        1,
    );
    lin::check_true(&mut failed, b"and says POLLIN", one[0].revents == POLLIN);

    let _ = lin::close(pair[0]);
    let _ = lin::close(pair[1]);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-poll: panicked\n")
}
