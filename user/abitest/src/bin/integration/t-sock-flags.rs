//! t-sock-flags - what the IP socket flag word owes a caller.
//!
//! Two contracts the `AF_INET` receive path used to break, both silent:
//!
//! * `MSG_PEEK` must not consume what it looks at. This kernel does not
//!   implement peek-preserving delivery over the IP path, so the honest answer
//!   is `EOPNOTSUPP` - the same one the `AF_UNIX`, `NETLINK` and `PACKET`
//!   families already give. Before this, the flag was not consulted at all and
//!   a peek **consumed the datagram and reported success**: a caller that asked
//!   to look without taking had its byte destroyed and never learned. Worse
//!   than refusing, because the corruption is undetectable. (Real `MSG_PEEK` is
//!   a named sub-gap in `PLAN.md` §12.2.)
//!
//! * `MSG_DONTWAIT` must end the wait with `EAGAIN` instead of parking. Before
//!   this, the IP tail decided nonblocking from the descriptor's `O_NONBLOCK`
//!   only, so a `recv` asked not to wait still slept forever on an empty queue.
//!
//! Both are pinned without any data on the socket: the peek refusal comes
//! before the wait, and the `DONTWAIT` answer is what an empty queue gives. Run
//! without the fix, the `MSG_PEEK` call here blocks (no data, flag ignored) and
//! the `MSG_DONTWAIT` call blocks too - so the test hangs, which is exactly the
//! counterfactual.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-sock-flags\n");
    let mut failed = false;

    let fd = lin::socket(lin::AF_INET as usize, lin::SOCK_DGRAM, 0) as i32;
    lin::check_true(
        &mut failed,
        b"a UDP (AF_INET, SOCK_DGRAM) socket opens",
        fd >= 0,
    );

    let mut buf = [0u8; 16];

    // `MSG_PEEK` is refused rather than answered by consuming. The socket is
    // empty, so the only correct non-hanging answer is the refusal.
    let r = lin::recvfrom(fd, &mut buf, lin::MSG_PEEK, None);
    lin::check_eq(
        &mut failed,
        b"MSG_PEEK is refused EOPNOTSUPP, not a consumed datagram",
        r,
        lin::EOPNOTSUPP,
    );

    // The peek refusal wins before any wait, so it stands even with
    // `MSG_DONTWAIT` alongside it.
    let r = lin::recvfrom(fd, &mut buf, lin::MSG_PEEK | lin::MSG_DONTWAIT, None);
    lin::check_eq(
        &mut failed,
        b"peek is refused even together with MSG_DONTWAIT",
        r,
        lin::EOPNOTSUPP,
    );

    // `MSG_DONTWAIT` alone on an empty queue answers `EAGAIN` now, not a park.
    let r = lin::recvfrom(fd, &mut buf, lin::MSG_DONTWAIT, None);
    lin::check_eq(
        &mut failed,
        b"MSG_DONTWAIT on an empty socket answers EAGAIN, not forever",
        r,
        lin::EAGAIN,
    );

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-sock-flags: panicked\n")
}
