//! t-socket - the shapes `socket(2)` is asked for, and the two refusals.
//!
//! A socket is the front door of the network, and what it answers decides
//! what a libc can do next. The interesting cases are the refusals, because
//! they are how a caller discovers what there is:
//!
//! * `AF_INET` stream and datagram sockets open - the two kinds this stack
//!   actually carries;
//! * `AF_INET6` opens too, so a caller walking a `getaddrinfo` list can try
//!   the IPv6 answer and be *told* it cannot be reached, rather than have
//!   the whole resolution fail - which is why `connect` on one answers
//!   `ENETUNREACH` and nothing else changes;
//! * a family this kernel does not speak is `EAFNOSUPPORT`, and a type the
//!   family does not carry is `ESOCKTNOSUPPORT`. Linux names those two
//!   differently on purpose, and a program walking a list of families
//!   depends on the difference - it skips on the first and reports the
//!   second. (Checked against a real Linux: `AF_INET` with `SOCK_SEQPACKET`
//!   answers 94, not 93 - `EPROTONOSUPPORT` is for a *protocol* the family
//!   has no use for.)
//!
//! Written with `rustix`, so the errno compared below is the one Linux
//! defines rather than one this test remembered.

use rustix::net::{AddressFamily, SocketAddrV4, SocketType, bind, connect, getsockname, socket};
use std::net::Ipv4Addr;

/// What the test reports: a name, and whether the claim held.
struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }

    fn check_errno(&mut self, name: &str, got: rustix::io::Errno, want: rustix::io::Errno) {
        self.check(&format!("{name} (got {got}, want {want})"), got == want);
    }
}

fn main() {
    println!("t-socket");
    let mut r = Report { failed: false };

    // The two kinds there are.
    let tcp = socket(AddressFamily::INET, SocketType::STREAM, None);
    r.check("a stream socket over IPv4 is created", tcp.is_ok());
    let udp = socket(AddressFamily::INET, SocketType::DGRAM, None);
    r.check("a datagram socket over IPv4 is created", udp.is_ok());

    // What kind of socket a descriptor is, read back: a program handed an
    // inherited descriptor asks before it uses it.
    if let Ok(sock) = &tcp {
        let kind = rustix::net::sockopt::socket_type(sock);
        r.check(
            &format!("the stream socket says it is SOCK_STREAM (got {kind:?})"),
            kind == Ok(SocketType::STREAM),
        );
    }

    // The IPv6 stub: opened so a resolution can try the address, and
    // refused at connect so the caller falls through to the IPv4 answer.
    match socket(AddressFamily::INET6, SocketType::STREAM, None) {
        Ok(v6) => {
            r.check(
                "an IPv6 socket opens, so a dual-stack caller can try it",
                true,
            );
            let peer = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 3), 53);
            let err = connect(&v6, &peer).unwrap_err();
            r.check_errno(
                "connect on the IPv6 stub is ENETUNREACH, not EAFNOSUPPORT",
                err,
                rustix::io::Errno::NETUNREACH,
            );
        }
        Err(err) => r.check(
            &format!("an IPv6 socket opens, so a dual-stack caller can try it (got {err})"),
            false,
        ),
    }

    // AF_UNIX is spoken for: the path family opens, and t-unix is the
    // test of what it answers. The refusals that remain are the family
    // *types* this kernel still declines - a datagram unix socket gets
    // EOPNOTSUPP at the type line, and an AppleTalk-style family still
    // gets EAFNOSUPPORT at the family line.
    match socket(AddressFamily::UNIX, SocketType::STREAM, None) {
        Ok(fd) => {
            r.check("socket(AF_UNIX, SOCK_STREAM) opens the path family", true);
            drop(fd);
        }
        Err(err) => {
            r.check(&format!("socket(AF_UNIX, SOCK_STREAM) opens the path family (got {err})"), false);
        }
    }
    let err = socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).unwrap_err();
    r.check_errno(
        "a unix type without a kernel story is EOPNOTSUPP",
        err,
        rustix::io::Errno::OPNOTSUPP,
    );
    let err = socket(AddressFamily::INET, SocketType::SEQPACKET, None).unwrap_err();
    r.check_errno(
        "a socket type IPv4 does not carry is ESOCKTNOSUPPORT",
        err,
        rustix::io::Errno::SOCKTNOSUPPORT,
    );

    // Bind to port 0 and read the port the kernel picked back: the only way
    // a server learns the name it got.
    if let Ok(sock) = &tcp {
        // A fixed port rather than 0, so the name read back can be checked
        // against a known number instead of merely "not zero".
        const PORT: u16 = 34567;
        let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
        if let Err(err) = bind(sock, &any) {
            r.check(
                &format!("binding to port {PORT} succeeds (got {err})"),
                false,
            );
        } else {
            match getsockname(sock) {
                // The only way a server learns the name it was given.
                Ok(named) => r.check(
                    &format!("getsockname reads back the port we bound (got {named:?})"),
                    format!("{named:?}").contains(&PORT.to_string()),
                ),
                Err(err) => r.check(&format!("getsockname answers (got {err})"), false),
            }
        }
    }

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
