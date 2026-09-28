//! t-tcp-listen - a server and a client in the same machine, over the loopback.
//!
//! This is the program `t-udp-loopback` was the prerequisite for: `listen(2)`
//! and `accept(2)` on an IPv4 socket, exercised end to end. Until now the
//! stack was outbound-only and `listen` answered `EOPNOTSUPP`, pinned
//! deliberately in `t-tcp` - so there was no in-guest server, and no local
//! socket pair to watch a connection open, carry bytes, half-close and end.
//!
//! One process drives both ends, which is not a shortcut: `connect`'s own poll
//! advances the listener's handshake as much as the client's, and the loopback
//! device hands every frame straight back, so a single process is the whole
//! path. What the test cannot decide that way - that an *unrelated* process can
//! reach the listener - is not in question: nothing about the accept path is
//! per-process.
//!
//! Every refusal asserted here was measured against this host's kernel before
//! it was written down, because a listener's errnos are exactly the kind of
//! thing that is remembered wrong: `listen` on a connected socket is `EINVAL`,
//! `accept` on a bound-but-not-listening socket is `EINVAL`, `accept` on an
//! empty non-blocking listener is `EAGAIN`, `recv` and `getpeername` on a
//! listener are `ENOTCONN`, and `listen` on an unbound socket *succeeds* by
//! implicitly binding `0.0.0.0` and an ephemeral port.

use rustix::fd::OwnedFd;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, Shutdown, SocketAddrAny, SocketAddrV4, SocketType, accept,
    acceptfrom, bind, connect, getpeername, getsockname, listen, recvfrom, send, shutdown, socket,
};
use std::net::Ipv4Addr;

/// The address that names the loopback, and the only one a server can be
/// reached at from inside this machine.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

const REQUEST: &[u8] = b"hello, listener";
const REPLY: &[u8] = b"hello, client";

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
/// the one family this machine has.
fn as_v4(addr: SocketAddrAny) -> Option<SocketAddrV4> {
    SocketAddrV4::try_from(addr).ok()
}

fn stream() -> OwnedFd {
    socket(AddressFamily::INET, SocketType::STREAM, None).expect("a stream socket")
}

fn main() {
    println!("t-tcp-listen");
    let mut r = Report { failed: false };

    // A server is named on the loopback and told to listen. Port 0 asks the
    // kernel to pick, which is how a server finds a port no other test is
    // using, and `getsockname` is how it learns which.
    let server = stream();
    let any_on_lo = SocketAddrV4::new(LOOPBACK, 0);
    match bind(&server, &any_on_lo) {
        Ok(()) => r.check("a server binds a loopback address", true),
        Err(err) => {
            r.check(
                &format!("a server binds a loopback address (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    }
    match listen(&server, 4) {
        Ok(()) => r.check("listen on a bound socket succeeds", true),
        Err(err) => {
            // The refusal this test exists to replace: `t-tcp` used to pin
            // `EOPNOTSUPP` here, and a regression to it says so plainly.
            r.check(
                &format!("listen on a bound socket succeeds (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    }
    let server_addr = getsockname(&server)
        .ok()
        .and_then(as_v4)
        .expect("the listener's own name");
    r.check(
        &format!("the listener reports the name it bound (got {server_addr})"),
        server_addr.ip() == &LOOPBACK && server_addr.port() != 0,
    );

    // A client connects to it. This is the whole loopback path in one call:
    // the SYN leaves the client's socket, is handed back by the loopback
    // device, completes the handshake against a socket in the listener's pool,
    // and the SYN-ACK makes the same round trip.
    let client = stream();
    match connect(&client, &server_addr) {
        Ok(()) => r.check("a client connects to it over the loopback", true),
        Err(err) => {
            r.check(
                &format!("a client connects to it over the loopback (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    }
    let client_addr = getsockname(&client).ok().and_then(as_v4);

    // The connection the client just made is waiting to be accepted.
    let accepted = acceptfrom(&server);
    let conn = match accepted {
        Ok((conn, peer)) => {
            r.check("accept hands out the connection", true);
            let peer = peer.and_then(as_v4);
            r.check(
                &format!("and names the peer it came from (got {peer:?})"),
                peer.is_some_and(|p| p.ip() == &LOOPBACK) && peer == client_addr,
            );
            match getsockname(&conn).ok().and_then(as_v4) {
                Some(local) => r.check(
                    &format!("the accepted socket's own name is the listener's (got {local})"),
                    local == server_addr,
                ),
                None => r.check("the accepted socket's own name is the listener's", false),
            }
            conn
        }
        Err(err) => {
            r.check(
                &format!("accept hands out the connection (got {err})"),
                false,
            );
            std::process::exit(1);
        }
    };

    // Bytes both ways, over the connection the handshake built.
    match send(&client, REQUEST, SendFlags::empty()) {
        Ok(n) => r.check(
            &format!("the client's request goes out (sent {n})"),
            n == REQUEST.len(),
        ),
        Err(err) => r.check(&format!("the client's request goes out (got {err})"), false),
    }
    let mut buf = [0u8; 64];
    match recvfrom(&conn, &mut buf, RecvFlags::empty()) {
        Ok((n, _truncated, _)) => r.check(
            &format!("the server reads it (got {n} bytes)"),
            n == REQUEST.len() && &buf[..n] == REQUEST,
        ),
        Err(err) => r.check(&format!("the server reads it (got {err})"), false),
    }
    match send(&conn, REPLY, SendFlags::empty()) {
        Ok(n) => r.check(
            &format!("the server's reply goes out (sent {n})"),
            n == REPLY.len(),
        ),
        Err(err) => r.check(&format!("the server's reply goes out (got {err})"), false),
    }
    match recvfrom(&client, &mut buf, RecvFlags::empty()) {
        Ok((n, _truncated, _)) => r.check(
            &format!("the client reads it (got {n} bytes)"),
            n == REPLY.len() && &buf[..n] == REPLY,
        ),
        Err(err) => r.check(&format!("the client reads it (got {err})"), false),
    }

    // The half-close: the client shuts its writing half down, and the server
    // reads end-of-stream - while the *other* direction stays open, which is
    // what makes this a half-close and not a close. A client that had simply
    // closed would leave the server unable to answer.
    r.check(
        "the client shuts its writing half down",
        shutdown(&client, Shutdown::Write).is_ok(),
    );
    match recvfrom(&conn, &mut buf, RecvFlags::empty()) {
        Ok((0, _truncated, _)) => r.check("the server reads end-of-stream", true),
        Ok((n, _truncated, _)) => r.check(
            &format!("the server reads end-of-stream (read {n} bytes instead)"),
            false,
        ),
        Err(err) => r.check(
            &format!("the server reads end-of-stream (got {err})"),
            false,
        ),
    }
    match send(&conn, REPLY, SendFlags::empty()) {
        Ok(n) => r.check(
            &format!("and the connection's other half still carries (sent {n})"),
            n == REPLY.len(),
        ),
        Err(err) => r.check(
            &format!("and the connection's other half still carries (got {err})"),
            false,
        ),
    }
    match recvfrom(&client, &mut buf, RecvFlags::empty()) {
        Ok((n, _truncated, _)) => r.check(
            &format!("the client still reads (got {n} bytes)"),
            n == REPLY.len(),
        ),
        Err(err) => r.check(&format!("the client still reads (got {err})"), false),
    }
    drop(conn);
    drop(client);

    // A second, sequential connection: a listener whose socket completed one
    // handshake has to have refilled its pool, or it serves exactly one client
    // and then goes deaf.
    let client2 = stream();
    match connect(&client2, &server_addr) {
        Ok(()) => r.check("a second client connects", true),
        Err(err) => r.check(&format!("a second client connects (got {err})"), false),
    }
    match accept(&server) {
        Ok(conn2) => {
            r.check("the second connection is accepted too", true);
            match send(&client2, b"second", SendFlags::empty()) {
                Ok(_) => {
                    let mut b2 = [0u8; 16];
                    match recvfrom(&conn2, &mut b2, RecvFlags::empty()) {
                        Ok((n, _truncated, _)) => r.check(
                            &format!("and carries its own bytes (got {n})"),
                            &b2[..n] == b"second",
                        ),
                        Err(err) => {
                            r.check(&format!("and carries its own bytes (got {err})"), false)
                        }
                    }
                }
                Err(err) => r.check(&format!("the second client writes (got {err})"), false),
            }
            drop(conn2);
        }
        Err(err) => r.check(
            &format!("the second connection is accepted too (got {err})"),
            false,
        ),
    }
    drop(client2);

    // `accept` on a non-blocking listener with nothing waiting is `EAGAIN`,
    // which is what a server's event loop is written against. The listener is
    // made non-blocking here rather than at creation, so the flag path
    // (`F_SETFL` reaching the socket) is exercised too.
    let made_nonblocking = fcntl_getfl(&server)
        .and_then(|f| fcntl_setfl(&server, f | OFlags::NONBLOCK))
        .is_ok();
    r.check("the listener is made non-blocking", made_nonblocking);
    match accept(&server) {
        Err(err) => r.check(
            &format!("accept with nothing waiting is EAGAIN (got {err})"),
            err == Errno::AGAIN || err == Errno::WOULDBLOCK,
        ),
        Ok(_) => r.check(
            "accept with nothing waiting is EAGAIN (it succeeded)",
            false,
        ),
    }
    // Note: the flag is left on. Later checks are reads of state, not waits.

    // A connection to a port nobody listens on, on the same address: the
    // loopback answers the SYN with a reset, which is what makes this
    // `ECONNREFUSED` rather than a timeout - the shape a client program
    // distinguishes.
    let nobody = stream();
    match connect(&nobody, &SocketAddrV4::new(LOOPBACK, 1)) {
        Err(err) => r.check(
            &format!("connecting where nothing listens is refused (got {err})"),
            err == Errno::CONNREFUSED,
        ),
        Ok(()) => r.check(
            "connecting where nothing listens is refused (it connected)",
            false,
        ),
    }
    drop(nobody);

    // The refusals that make a listener a distinct thing rather than a socket
    // that happens to be open. Each was measured on Linux first.
    let connected = stream();
    let _ = bind(&connected, &SocketAddrV4::new(LOOPBACK, 0));
    match connect(&connected, &server_addr) {
        Ok(()) => {
            let answer = listen(&connected, 1);
            r.check(
                &format!("listen on a connected socket is EINVAL (got {answer:?})"),
                answer == Err(Errno::INVAL),
            );
            // Take it off the queue so what is left below is only what this
            // test put there on purpose.
            let _ = accept(&server);
        }
        Err(err) => r.check(
            &format!("listen on a connected socket is EINVAL (could not connect: {err})"),
            false,
        ),
    }
    drop(connected);

    // A listener is not a connection: `recv` and `getpeername` say so, and
    // both answers were measured.
    let mut scratch = [0u8; 8];
    match recvfrom(&server, &mut scratch, RecvFlags::empty()) {
        Err(err) => r.check(
            &format!("recv on a listener is ENOTCONN (got {err})"),
            err == Errno::NOTCONN,
        ),
        Ok((n, _truncated, _)) => r.check(
            &format!("recv on a listener is ENOTCONN (read {n} bytes)"),
            false,
        ),
    }
    match getpeername(&server) {
        Err(err) => r.check(
            &format!("getpeername on a listener is ENOTCONN (got {err})"),
            err == Errno::NOTCONN,
        ),
        Ok(_) => r.check("getpeername on a listener is ENOTCONN (it answered)", false),
    }
    // `send` on a listener is `EPIPE` (measured). `MSG_NOSIGNAL` is asked for
    // so the errno is what is under test: Linux raises `SIGPIPE` for this
    // write, and this kernel raises no signal for a broken channel at all - a
    // divergence of its own, recorded rather than leaned on here.
    match send(&server, b"x", SendFlags::NOSIGNAL) {
        Err(err) => r.check(
            &format!("send on a listener is EPIPE (got {err})"),
            err == Errno::PIPE,
        ),
        Ok(n) => r.check(
            &format!("send on a listener is EPIPE (sent {n} bytes)"),
            false,
        ),
    }

    // And the implicit bind: `listen` on a socket that was never bound
    // *succeeds*, naming it `0.0.0.0` and an ephemeral port - which is what
    // Linux does, and the reason a server that forgets to `bind` still works.
    let unbound = stream();
    match listen(&unbound, 1) {
        Ok(()) => {
            let named = getsockname(&unbound).ok().and_then(as_v4);
            r.check(
                &format!("listen on an unbound socket implicitly binds (got {named:?})"),
                named.is_some_and(|a| a.ip() == &Ipv4Addr::UNSPECIFIED && a.port() != 0),
            );
        }
        Err(err) => r.check(
            &format!("listen on an unbound socket implicitly binds (got {err})"),
            false,
        ),
    }
    drop(unbound);
    drop(server);

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
