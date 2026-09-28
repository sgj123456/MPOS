//! t-tcp - the stream shapes that do not need a peer on the internet.
//!
//! A connection to a server somewhere is a test of the internet as much as
//! of the kernel, so what is pinned here is the part that is ours: a socket
//! can be given a local name, told to listen, and asked what name it got -
//! which is how a server finds the port the kernel picked for it.
//!
//! The refused connection is the other half. Connecting to a port nothing
//! is listening on must *fail*: a stack that answers success for a
//! handshake nobody completed hands every program a socket to nowhere. The
//! exact errno is not asserted - `ECONNREFUSED` and `ETIMEDOUT` are both
//! honest answers depending on whether the far end resets or stays silent -
//! but succeeding is not one of them.

use rustix::net::{
    AddressFamily, SocketAddrV4, SocketType, bind, connect, getsockname, listen, socket,
};
use std::net::Ipv4Addr;

/// A fixed port, so the name read back can be checked against a known
/// number instead of merely "not zero".
const PORT: u16 = 34567;

struct Report {
    failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool) {
        println!("{}  {}", if ok { "ok:  " } else { "FAIL:" }, name);
        self.failed |= !ok;
    }
}

fn main() {
    println!("t-tcp");
    let mut r = Report { failed: false };

    let sock = match socket(AddressFamily::INET, SocketType::STREAM, None) {
        Ok(s) => s,
        Err(err) => {
            r.check(&format!("a stream socket is created (got {err})"), false);
            std::process::exit(1);
        }
    };
    r.check("a stream socket is created", true);

    // Name the local end, then ask what we got. This is the only way a
    // server learns the port it was given.
    let any = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT);
    match bind(&sock, &any) {
        Ok(()) => {
            r.check(&format!("binding to port {PORT} succeeds"), true);
            match getsockname(&sock) {
                Ok(named) => r.check(
                    &format!("getsockname reads back the port we bound (got {named:?})"),
                    format!("{named:?}").contains(&PORT.to_string()),
                ),
                Err(err) => r.check(&format!("getsockname answers (got {err})"), false),
            }
        }
        Err(err) => r.check(
            &format!("binding to port {PORT} succeeds (got {err})"),
            false,
        ),
    }

    // Binding the same port twice is refused - two sockets cannot own one
    // name unless the first asked to share.
    let second = socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    let err = bind(&second, &any).unwrap_err();
    r.check(
        &format!("binding a port already bound is refused (got {err})"),
        err != rustix::io::Errno::OPNOTSUPP,
    );

    // `listen` on an IPv4 socket succeeds now - the passive-open side is real,
    // and `t-tcp-listen` is where it is exercised end to end. What is pinned
    // here is only that this call no longer answers `EOPNOTSUPP`: this file
    // held that refusal deliberately ("a stack that answers success for a
    // handshake nobody completed hands every program a socket to nowhere"), so
    // the check that the refusal is gone belongs next to the comment that
    // explained it. The socket is bound to the wildcard above, so this is also
    // the case that does not need a loopback address to be listening.
    match listen(&sock, 1) {
        Ok(()) => r.check("listen on a bound IPv4 socket succeeds", true),
        Err(err) => r.check(
            &format!("listen on a bound IPv4 socket succeeds (got {err})"),
            false,
        ),
    }

    // A connection to a port nobody is listening on: slirp's 10.0.2.2 is
    // the host, and port 9 (discard) has nothing behind it here.
    let nowhere = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 9);
    let third = socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    match connect(&third, &nowhere) {
        Ok(()) => r.check("connecting where nothing listens fails", false),
        Err(err) => r.check(
            &format!("connecting where nothing listens fails (got {err})"),
            true,
        ),
    }

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
