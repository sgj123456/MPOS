//! t-sock-flags - what the IP socket flag word owes a caller.
//!
//! Two contracts the `AF_INET` receive path used to break. The first has since
//! been *served* rather than refused, so this file keeps only the half that is
//! still its subject:
//!
//! * `MSG_DONTWAIT` must end the wait with `EAGAIN` instead of parking. Before
//!   this, the IP tail decided nonblocking from the descriptor's `O_NONBLOCK`
//!   only, so a `recv` asked not to wait still slept forever on an empty queue.
//!
//! * `MSG_PEEK` used to be refused `EOPNOTSUPP` here (the honest answer while
//!   peek-preserving delivery did not exist, and what this program pinned).
//!   The IP path serves it now - see `t-msg-peek` for the whole measured
//!   contract - so what is left to pin about a peek in *this* file is that the
//!   flag word composes: a peek on an empty queue is a wait like any other, so
//!   `MSG_PEEK|MSG_DONTWAIT` answers `EAGAIN` and never a refusal.
//!
//! Both are pinned on an empty socket: `MSG_DONTWAIT`'s answer is what an empty
//! queue gives, and a peek that ignored the flag would hang here - a claim that
//! blocks reads as a hang, which is worse than one that fails.
//!
//! The descriptor is made non-blocking on a host run as well (`t-msg-peek`'s
//! rule): a claim that can park is worse than one that fails, and the flags
//! under test are the call's, not the descriptor's - so the answers are the
//! same either way.

use mpos_user_tests::Report;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::net::{AddressFamily, RecvFlags, SocketType, recvfrom, socket};

/// The answer to one receive, as the negated errno it refused with, or `0` for
/// a call that returned a length. A length is never the answer any claim here
/// expects, so `0` reads as "it did not refuse" in a failure line.
fn recv_answer(fd: &rustix::fd::OwnedFd, flags: RecvFlags) -> i32 {
    let mut buf = [0u8; 16];
    match recvfrom(fd, &mut buf, flags) {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error(),
    }
}

const AGAIN: i32 = -(Errno::AGAIN.raw_os_error());

fn main() {
    println!("t-sock-flags");
    let mut r = Report::new();

    let opened = socket(AddressFamily::INET, SocketType::DGRAM, None);
    r.check("a UDP (AF_INET, SOCK_DGRAM) socket opens", opened.is_ok());
    // Nothing else is answerable without one, and `finish` is the only exit a
    // test has, so the refusal is the end of the run rather than a panic.
    let fd = match opened {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };

    // A descriptor whose own flag could park would hide the call's answer, so
    // `O_NONBLOCK` is set beside the `MSG_DONTWAIT` under test.
    match fcntl_getfl(&fd) {
        Ok(flags) => {
            let _ = fcntl_setfl(&fd, flags | OFlags::NONBLOCK);
        }
        Err(e) => println!("note: fcntl(F_GETFL) answered {}", e),
    }

    // `MSG_PEEK` is served now, and a peek on an empty queue is still a wait:
    // the flags compose rather than one of them overriding the other.
    let peek_nowait = recv_answer(&fd, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    r.check(
        "MSG_PEEK on an empty socket with MSG_DONTWAIT answers EAGAIN",
        peek_nowait == AGAIN,
    );

    // The same with `MSG_TRUNC`, which changes what a *datagram* answers and
    // nothing about an empty queue.
    let peek_trunc = recv_answer(
        &fd,
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "and MSG_TRUNC beside them does not change that answer",
        peek_trunc == AGAIN,
    );

    // `MSG_DONTWAIT` alone on an empty queue answers `EAGAIN` now, not a park.
    let dontwait = recv_answer(&fd, RecvFlags::DONTWAIT);
    r.check(
        "MSG_DONTWAIT on an empty socket answers EAGAIN, not forever",
        dontwait == AGAIN,
    );

    r.finish();
}
