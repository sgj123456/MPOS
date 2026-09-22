//! t-sockmsg - the message calls on a `socketpair`, which is how sudo talks
//! to itself.
//!
//! A program that forks a helper it must stay in step with - sudo's monitor
//! process, a logger, a job runner - makes an `AF_UNIX` pair and speaks to
//! the other end with `send` and `recv` (which musl builds on `sendto` and
//! `recvfrom` with a null address) or their `sendmsg`/`recvmsg` relatives.
//! The pair is a channel, not a protocol: no address to name, no control
//! data passing here - so the message calls on it are the read and write the
//! channel always had, wearing socket clothes. A kernel that answers them
//! with `ENOTSOCK` leaves every such caller saying "unable to send message
//! to monitor process", which is exactly what sudo did.
//!
//! What is pinned here:
//!
//! * `send` carries bytes to the peer's `recv`, in both directions;
//! * `sendmsg` gathers iovecs into one stream, and `recvmsg` answers with
//!   no source address and no flags - the message shape of "nothing to say";
//! * `MSG_DONTWAIT` on an empty channel is `EAGAIN`, not a park;
//! * the channel end is not an IP socket: a destination on a connected pair
//!   is `EISCONN`, which is what Linux answers too.
//!
//! Written with `rustix`, so every errno compared below is the one Linux
//! defines rather than one this test remembered.

use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, SendAncillaryBuffer, SendFlags, SocketAddrUnix,
    SocketFlags, SocketType, recv, recvmsg, send, sendmsg, sendto, socketpair,
};
use std::io::IoSlice;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: Errno, want: Errno) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }
}

fn main() {
    println!("t-sockmsg");
    let mut r = Report { failed: false };

    let (a, b) = match socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            r.check(
                &format!("an AF_UNIX stream pair is created (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };
    r.check("an AF_UNIX stream pair is created", true);

    // `send`/`recv`: sudo's backchannel shape. `NOSIGNAL` is what sudo
    // actually passes - a broken channel must raise no SIGPIPE here.
    r.check(
        "send carries a message into the pair",
        send(&a, b"status: 0", SendFlags::NOSIGNAL).is_ok_and(|n| n == 9),
    );
    let mut buf = [0u8; 64];
    match recv(&b, &mut buf, RecvFlags::empty()) {
        Ok((n, _)) => r.check(
            &format!("recv takes it back whole ({n} bytes)"),
            &buf[..n] == b"status: 0",
        ),
        Err(err) => r.check(&format!("recv takes it back whole (got {err})"), false),
    }
    r.check(
        "and the other direction sends too",
        send(&b, b"ack", SendFlags::NOSIGNAL).is_ok_and(|n| n == 3),
    );
    match recv(&a, &mut buf, RecvFlags::empty()) {
        Ok((n, _)) => r.check(
            &format!("and the first end reads it ({n} bytes)"),
            &buf[..n] == b"ack",
        ),
        Err(err) => r.check(&format!("and the first end reads it (got {err})"), false),
    }

    // `sendmsg`/`recvmsg`: the scatter-gather forms, with no address and no
    // control data - the ordinary case for a pair. The gather side has to
    // arrive as one stream; the answer carries no source address and no
    // flags, which is what "nothing to say" looks like in the msghdr.
    let mut empty_send = SendAncillaryBuffer::default();
    let sent = sendmsg(
        &a,
        &[IoSlice::new(b"one "), IoSlice::new(b"two ")],
        &mut empty_send,
        SendFlags::empty(),
    );
    r.check(
        &format!("sendmsg gathers two iovecs ({sent:?})"),
        sent.is_ok_and(|n| n == 8),
    );
    let mut space: [std::mem::MaybeUninit<u8>; 0] = [];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let mut rbuf = [0u8; 64];
    match recvmsg(
        &b,
        &mut [rustix::io::IoSliceMut::new(&mut rbuf)],
        &mut ancillary,
        RecvFlags::empty(),
    ) {
        Ok(msg) => {
            r.check(
                &format!(
                    "recvmsg takes the gathered bytes ({:?})",
                    &rbuf[..msg.bytes]
                ),
                &rbuf[..msg.bytes] == b"one two ",
            );
            r.check("recvmsg names no source address", msg.address.is_none());
        }
        Err(err) => r.check(
            &format!("recvmsg takes the gathered bytes (got {err})"),
            false,
        ),
    }

    // `MSG_DONTWAIT` on an empty channel: `EAGAIN`, not a park. A test that
    // blocked here would hang the whole self-test run, so the wrong answer
    // is loud in its own way too.
    match recv(&a, &mut buf, RecvFlags::DONTWAIT) {
        Err(err) => r.check_errno("recv with DONTWAIT on empty is EAGAIN", err, Errno::AGAIN),
        Ok((n, _)) => r.check(
            &format!("recv with DONTWAIT on empty is EAGAIN (got {n} bytes)"),
            false,
        ),
    }

    // The channel end is connected: naming a destination on it is the same
    // mistake it would be on a TCP stream, and Linux answers `EISCONN`. The
    // address is never even looked at - a connected socket is told so first.
    match socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    ) {
        Ok((c, _d)) => {
            let addr = SocketAddrUnix::new("/nowhere").expect("a unix address");
            match sendto(&c, b"to whom?", SendFlags::empty(), &addr) {
                Ok(n) => r.check(
                    &format!("a destination on a connected pair is EISCONN (sent {n})"),
                    false,
                ),
                Err(err) => r.check_errno(
                    "a destination on a connected pair is EISCONN",
                    err,
                    Errno::ISCONN,
                ),
            }
        }
        Err(err) => r.check(
            &format!("a second pair is created for the refusal check (got {err})"),
            false,
        ),
    }

    if r.failed {
        println!("t-sockmsg: FAILED");
        std::process::exit(1);
    }
    println!("t-sockmsg: all passed");
}
