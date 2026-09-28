//! t-udp-loopback - `127.0.0.1` is a real wire, and this is the smallest
//! exchange that proves it.
//!
//! The loopback used to be a name and nothing else: `lo` was reported by the
//! link dump, `127.0.0.0/8` appeared in `ip addr`, and a datagram addressed to
//! it left by the NIC and was never seen again - there was no interface behind
//! the name and no route that turned around. What is pinned here is that the
//! address now names a device that hands a transmitted frame straight back to
//! the receiver, so two sockets in one process can talk over it with no
//! network between them.
//!
//! UDP rather than TCP because a datagram needs no listener, so this test
//! decides whether the *loopback* works and nothing else: the accept path has
//! its own program (`t-tcp-listen`).
//!
//! The exchange also runs on the interface a destination chooses, which is the
//! part that is not simply "a second device exists": a socket is created on
//! eth0's fabric and moved to the loopback's when the first address it is
//! given says so, because smoltcp has no way to move one between socket sets.
//! A stack that forgot the move would queue the datagram on an interface with
//! no route to `127.0.0.1` and it would never arrive.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrAny, SocketAddrV4, SocketType, bind,
    getsockname, recvfrom, sendto, socket,
};
use std::net::Ipv4Addr;

/// The address that names the loopback, and the whole point of the test.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// A socket address rustix hands back without knowing its family, narrowed to
/// the one family this machine has. rustix refuses the conversion rather than
/// returning a half-read address, which is why this cannot be a `match`.
fn as_v4(addr: SocketAddrAny) -> Option<SocketAddrV4> {
    SocketAddrV4::try_from(addr).ok()
}

/// Whether `fd` is readable within `ms`, and how many descriptors the wait
/// named. A bounded wait rather than a blocking read: "nothing ever came back"
/// has to be a report, not a hang.
fn readable_within(fd: &impl rustix::fd::AsFd, ms: i64) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let deadline = Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    matches!(poll(&mut fds, Some(&deadline)), Ok(n) if n == 1)
}

fn main() {
    println!("t-udp-loopback");
    let mut r = Report { failed: false };

    let server = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(s) => s,
        Err(err) => {
            r.check(&format!("a datagram socket is created (got {err})"), false);
            std::process::exit(1);
        }
    };

    // Name the receiving end on the loopback with port 0, so the kernel picks
    // the port - which is also the first address this socket is given, and so
    // the moment it decides which interface it belongs to.
    let any_on_lo = SocketAddrV4::new(LOOPBACK, 0);
    match bind(&server, &any_on_lo) {
        Ok(()) => r.check("a socket binds a loopback address", true),
        Err(err) => {
            r.check(
                &format!("a socket binds a loopback address (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    }
    let port = match getsockname(&server) {
        Ok(named) => {
            let named = as_v4(named);
            r.check(
                &format!("the name read back is the loopback's (got {named:?})"),
                named.is_some_and(|a| a.ip() == &LOOPBACK),
            );
            named.map(|a| a.port()).unwrap_or(0)
        }
        Err(err) => {
            r.check(&format!("the bound name is read back (got {err})"), false);
            std::process::exit(1);
        }
    };
    r.check(
        &format!("the kernel chose a port for it (got {port})"),
        port != 0,
    );

    // The sender is never bound: sending to the loopback is what names *it*,
    // through the same rule, on the same call that would otherwise have to
    // choose an anonymous port.
    let client = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(s) => s,
        Err(err) => {
            r.check(
                &format!("a second datagram socket is created (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };
    const PAYLOAD: &[u8] = b"over the loopback";
    let to_server = SocketAddrV4::new(LOOPBACK, port);
    match sendto(&client, PAYLOAD, SendFlags::empty(), &to_server) {
        Ok(n) => r.check(
            &format!("an unbound socket sends to the loopback (sent {n})"),
            n == PAYLOAD.len(),
        ),
        Err(err) => r.check(
            &format!("an unbound socket sends to the loopback (got {err})"),
            false,
        ),
    }

    // It arrives, whole, with the sender named - which is the loopback
    // answering rather than a NIC that dropped it.
    let mut buf = [0u8; 64];
    if !readable_within(&server, 2000) {
        r.check(
            "the datagram arrives over the loopback (nothing was readable)",
            false,
        );
    } else {
        match recvfrom(&server, &mut buf, RecvFlags::empty()) {
            Ok((n, _truncated, from)) => {
                r.check(
                    &format!("the datagram arrives over the loopback (got {n} bytes)"),
                    n == PAYLOAD.len() && &buf[..n] == PAYLOAD,
                );
                let from = from.and_then(as_v4);
                // The sender's own address, which is the loopback's - the
                // destination chose the interface, so the source a socket on it
                // uses is the one that interface carries.
                r.check(
                    &format!("and names the sender on the loopback (got {from:?})"),
                    from.is_some_and(|a| a.ip() == &LOOPBACK && a.port() != 0),
                );
            }
            Err(err) => r.check(&format!("the datagram is received (got {err})"), false),
        }
    }

    // A datagram to a port nobody holds is not refused - UDP has no refusal to
    // send - so the wait is bounded and empty, and that *is* the answer. The
    // reader is the sending socket: nothing is addressed to it, so if the
    // loopback invented a reply this is what would see it.
    let nowhere = SocketAddrV4::new(LOOPBACK, 1);
    let _ = sendto(&client, b"nobody home", SendFlags::empty(), &nowhere);
    r.check(
        "a datagram to a port nobody holds is answered by nothing",
        !readable_within(&client, 300),
    );

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
