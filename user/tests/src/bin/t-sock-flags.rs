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
//!   a named sub-gap in `NOTES.md` §12.2.)
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
//!
//! **Host divergence, and why the socket is made non-blocking there.** Linux
//! *does* implement `MSG_PEEK`: on an empty socket it waits for a datagram, and
//! this program has no peer to send one, so the honest host spelling of "no
//! answer yet" is `EAGAIN` once the descriptor is `O_NONBLOCK` - never the
//! `EOPNOTSUPP` this kernel refuses with. Two things follow from that, and both
//! are in the code below rather than in a comment: a host run sets
//! `O_NONBLOCK` *first*, because a claim that can park is worse than one that
//! fails (measured on a peerless socket: `recvfrom(MSG_PEEK)` waits), and the
//! two peek claims accept `EAGAIN` beside the refusal, naming the gap instead
//! of pinning Linux's answer as this kernel's.

use mpos_user_tests::{Report, host_run};
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

/// Linux's "nothing to read" and this kernel's "not offered", looked up rather
/// than written down: both sides of every claim below are these two numbers.
const AGAIN: i32 = -(Errno::AGAIN.raw_os_error());
const OPNOTSUPP: i32 = -(Errno::OPNOTSUPP.raw_os_error());

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

    // A host run makes the descriptor non-blocking before any claim is made:
    // Linux's peek waits for a datagram that no peer will send, and a claim
    // that hangs is worse than one that reads a different errno. This kernel's
    // answers do not depend on the descriptor's flag, so the pinning is the
    // same file either way.
    if host_run() {
        match fcntl_getfl(&fd) {
            Ok(flags) => {
                let _ = fcntl_setfl(&fd, flags | OFlags::NONBLOCK);
            }
            Err(e) => println!("note: fcntl(F_GETFL) answered {}", e),
        }
    }

    // `MSG_PEEK` is refused rather than answered by consuming. The socket is
    // empty, so the only correct non-hanging answer is the refusal - and on a
    // host, which implements peek, the empty queue's own answer.
    let peek = recv_answer(&fd, RecvFlags::PEEK);
    r.check(
        "MSG_PEEK is refused EOPNOTSUPP, not a consumed datagram",
        peek == OPNOTSUPP || host_run() && peek == AGAIN,
    );

    // The peek refusal wins before any wait, so it stands even with
    // `MSG_DONTWAIT` alongside it.
    let peek_nowait = recv_answer(&fd, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    r.check(
        "peek is refused even together with MSG_DONTWAIT",
        peek_nowait == OPNOTSUPP || host_run() && peek_nowait == AGAIN,
    );

    // `MSG_DONTWAIT` alone on an empty queue answers `EAGAIN` now, not a park.
    // Both kernels agree here, which is what makes the two lines above the
    // divergence they are: the gap is peek, not the wait.
    let dontwait = recv_answer(&fd, RecvFlags::DONTWAIT);
    r.check(
        "MSG_DONTWAIT on an empty socket answers EAGAIN, not forever",
        dontwait == AGAIN,
    );

    r.finish();
}
