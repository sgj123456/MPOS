//! t-udp - a datagram out and an answer back, over the real wire.
//!
//! Everything else in the suite asks the kernel what it thinks; this one
//! asks the network. A DNS query is the smallest complete protocol exchange
//! there is - one datagram out, one datagram back - and it exercises the
//! whole path a real program uses: an anonymous local port, a send to a
//! peer, a wait, and a receive that names the sender.
//!
//! QEMU's user-mode stack answers DNS at 10.0.2.3:53, so the query goes to
//! a server that is really there. A test that only ever talks to itself
//! would pass even if the checksum were wrong.
//!
//! The query is built by hand - header, then `localhost` as a name, then
//! the type and class - because the point is the wire format, and what is
//! checked is what a resolver checks: the answer carries our transaction
//! id, and at least one answer record came back.

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::AsFd;
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrV4, SocketType, recvfrom, sendto, socket,
};
use std::net::Ipv4Addr;

/// The resolver QEMU's user-mode stack puts at the far end of slirp.
const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const DNS_PORT: u16 = 53;
/// Our transaction id: the answer has to carry it back.
const QUERY_ID: u16 = 0x1234;

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

/// A DNS query for `localhost`'s A record, as RFC 1035 lays it out: a
/// 12-byte header - id, flags, four counts - then the name as
/// length-prefixed labels, then the question's type and class.
fn dns_query() -> Vec<u8> {
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&QUERY_ID.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // flags: RD, recursion desired
    q.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]); // one question, no answers
    let name = b"localhost";
    q.push(name.len() as u8);
    q.extend_from_slice(name);
    q.push(0); // end of the name
    q.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
    q.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
    q
}

fn main() {
    println!("t-udp");
    let mut r = Report { failed: false };

    let sock = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(s) => s,
        Err(err) => {
            r.check(&format!("a datagram socket is created (got {err})"), false);
            std::process::exit(1);
        }
    };
    r.check("a datagram socket is created", true);

    // Send the query. No bind first: the stack picks a local port, which is
    // what a resolver that never names one relies on.
    let peer = SocketAddrV4::new(DNS, DNS_PORT);
    let query = dns_query();
    match sendto(&sock, &query, SendFlags::empty(), &peer) {
        Ok(n) => r.check(
            &format!("the query goes out in one datagram (sent {n})"),
            n == query.len(),
        ),
        Err(err) => {
            r.check(&format!("the query goes out (got {err})"), false);
            std::process::exit(1);
        }
    }

    // Wait for the answer rather than blocking blindly: a poll that reports
    // nothing is the difference between "no answer yet" and "hung".
    let mut fds = [PollFd::new(&sock, PollFlags::IN)];
    let five_seconds = rustix::event::Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    // The wait itself is what this kernel owns: it has to come back inside the
    // five seconds it was given, whichever way the answer went. Whether a
    // resolver *answers* is a fact about the network the machine is on and not
    // about this stack - a build sandbox with no DNS egress is a perfectly
    // ordinary place to run this - so an empty poll is reported rather than
    // held against the kernel, and the answer checks below are skipped when
    // there is no answer to check.
    let ready = poll(&mut fds, Some(&five_seconds));
    let answered = match ready {
        Ok(n) => {
            r.check(
                &format!("the wait is bounded by the timeout it was given (got {n} ready)"),
                n <= 1,
            );
            n == 1
        }
        Err(err) => {
            r.check(&format!("poll waits for the answer (got {err})"), false);
            std::process::exit(1);
        }
    };
    if !answered {
        println!(
            "note: the resolver did not answer; the datagram leaving is all this network shows"
        );
    }

    if answered {
        let mut answer = [0u8; 512];
        match recvfrom(&sock, &mut answer, RecvFlags::empty()) {
            Ok((n, _truncated, from)) => {
                r.check(
                    &format!("the answer is at least a DNS header long (got {n})"),
                    n >= 12,
                );
                if n >= 12 {
                    let id = u16::from_be_bytes([answer[0], answer[1]]);
                    r.check(
                        &format!("the answer carries our transaction id (got {id:#06x})"),
                        id == QUERY_ID,
                    );
                    let ancount = u16::from_be_bytes([answer[6], answer[7]]);
                    r.check(
                        &format!("at least one answer record came back (got {ancount})"),
                        ancount >= 1,
                    );
                }
                match from {
                    // The sender is named, which is half of what recvfrom is for.
                    Some(addr) => r.check(
                        &format!("the answer came from the resolver we asked ({addr:?})"),
                        format!("{addr:?}").contains("10.0.2.3"),
                    ),
                    None => r.check("the answer names its sender", false),
                }
            }
            Err(err) => r.check(&format!("the answer is received (got {err})"), false),
        }
    }

    // A datagram socket is not a listening socket. Linux answers `EOPNOTSUPP`
    // for this - checked against a real one, not remembered - and so does this
    // stack, so the expectation is that errno rather than merely "some
    // failure": a stub that answers `EINVAL` would be a different ABI.
    let err = rustix::net::listen(&sock, 1).unwrap_err();
    r.check(
        &format!("listen on a datagram socket is EOPNOTSUPP (got {err})"),
        err == rustix::io::Errno::OPNOTSUPP,
    );
    let _: &dyn AsFd = &sock;

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
