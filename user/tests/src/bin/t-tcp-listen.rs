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
use rustix::net::sockopt::{set_socket_reuseaddr, socket_reuseaddr};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, Shutdown, SocketAddrAny, SocketAddrV4, SocketType, accept,
    acceptfrom, bind, connect, getpeername, getsockname, listen, recvfrom, send, shutdown, socket,
};
use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

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

    // A *negative* backlog is Linux's "as deep as you allow" rather than a
    // refusal: `__sys_listen` compares the argument to `SOMAXCONN` as an
    // unsigned int, so `-1` lands on the ceiling. It is not a number a caller
    // has to go looking for either - Rust's std passes exactly it on Linux from
    // `TcpListener::bind` and `UnixListener::bind` alike ("silently capped to
    // `/proc/sys/net/core/somaxconn`"), so a server written in Rust asks for it
    // on every bind. Read as zero it is a queue one deep, which refuses the
    // second of two simultaneous clients that Linux would have accepted.
    // Measured on the host first: `listen(fd, -1)` answers 0 for `AF_INET` and
    // `AF_UNIX`, and `ss -ltn` reports the backlog column as 4096 - this host's
    // `net.core.somaxconn`, not 0.
    let deep = stream();
    r.check(
        "a listener binds for the negative backlog",
        bind(&deep, &SocketAddrV4::new(LOOPBACK, 0)).is_ok(),
    );
    match listen(&deep, -1) {
        Ok(()) => r.check("listen(-1) opens the queue instead of refusing it", true),
        Err(err) => r.check(
            &format!("listen(-1) opens the queue instead of refusing it (got {err})"),
            false,
        ),
    }
    let deep_addr = getsockname(&deep)
        .ok()
        .and_then(as_v4)
        .expect("the deep listener's own name");
    // Both clients connect *before* either is accepted, which is what makes the
    // depth visible: on the ceiling both handshakes complete, where a queue of
    // one leaves the second with nowhere to land.
    let burst1 = stream();
    let burst2 = stream();
    let first_in = connect(&burst1, &deep_addr).is_ok();
    let second_in = connect(&burst2, &deep_addr).is_ok();
    r.check("the first of two waiting clients connects", first_in);
    r.check("the second one finds room beside it", second_in);
    // Accepted only when both are there: a blocking `accept` against a
    // connection that never completed would be the test hanging rather than
    // failing, and a hang is a worse report than a red check.
    if first_in && second_in {
        let taken1 = accept(&deep).is_ok();
        let taken2 = accept(&deep).is_ok();
        r.check(
            "both waiting connections are there to accept",
            taken1 && taken2,
        );
    }
    drop(burst1);
    drop(burst2);
    drop(deep);

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

    // `SO_REUSEADDR`, and the single exemption it gates. Every answer below was
    // measured on a real kernel first (`PLAN.md` §4.3.3 has the table), and the
    // rule they add up to is not the one the man page suggests:
    //
    //   a holder that is *listening* keeps its name; any other holder yields it
    //   only to a caller that set `SO_REUSEADDR` **and** whose holder had set it
    //   too.
    //
    // The second half is the surprising one, and it is what makes the option
    // work in practice: a server sets it on the listener, its accepted
    // connections inherit it (Linux copies it across, and the row below pins
    // that this kernel does too), and *that* is why a restart can take back the
    // port its own dead connections are holding.
    //
    // `try_bind` is the whole instrument: bind with the option set or not, and
    // keep the descriptor on success so the name stays held.
    fn try_bind(addr: &SocketAddrV4, reuse: bool) -> Result<OwnedFd, Errno> {
        let s = stream();
        if reuse {
            let _ = set_socket_reuseaddr(&s, true);
        }
        bind(&s, addr).map(|()| s)
    }
    let loopback = SocketAddrV4::new(LOOPBACK, 0);

    // A listener's own name is never given away, whatever the two sockets say:
    // two live listeners on one name is what `SO_REUSEPORT` is for, and this
    // kernel does not implement it - setting the option on both is not enough
    // on Linux either.
    let live = stream();
    r.check(
        "a listener can set SO_REUSEADDR",
        set_socket_reuseaddr(&live, true).is_ok(),
    );
    let _ = bind(&live, &loopback);
    let _ = listen(&live, 1);
    let live_addr = getsockname(&live)
        .ok()
        .and_then(as_v4)
        .expect("the live listener's name");
    for (tag, reuse) in [("without", false), ("with", true)] {
        match try_bind(&live_addr, reuse) {
            Err(err) => r.check(
                &format!("a listening name is EADDRINUSE {tag} SO_REUSEADDR (got {err})"),
                err == Errno::ADDRINUSE,
            ),
            Ok(_) => r.check(
                &format!("a listening name is EADDRINUSE {tag} SO_REUSEADDR (it bound)"),
                false,
            ),
        }
    }

    // A connection that ends with *this* side closing first leaves its 4-tuple
    // in `TIME-WAIT`, holding the name the listener was bound to. Accepting it
    // from a listener that asked for the option is the shape a server has, and
    // the inherited bit is what a restart leans on.
    let srv = stream();
    let _ = set_socket_reuseaddr(&srv, true);
    let _ = bind(&srv, &loopback);
    let _ = listen(&srv, 4);
    let srv_addr = getsockname(&srv)
        .ok()
        .and_then(as_v4)
        .expect("the server's name");
    let first_client = stream();
    let first_conn = match connect(&first_client, &srv_addr).and_then(|()| accept(&srv)) {
        Ok(conn) => conn,
        Err(err) => {
            r.check(
                &format!("the connection whose close makes the TIME-WAIT is made (got {err})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    r.check(
        "an accepted connection inherits the listener's SO_REUSEADDR",
        socket_reuseaddr(&first_conn).unwrap_or(false),
    );
    // The listener goes first, then the connection it accepted - so the
    // *server's* side is the active closer and its 4-tuple is the one that
    // lingers - then the client, whose `FIN` is what lets the server's socket
    // finish rather than sit in `FIN-WAIT`. The sleep is the polls: the stack
    // advances on the service task's beat, and the transition is on the wire,
    // not in this process.
    drop(srv);
    drop(first_conn);
    drop(first_client);
    thread::sleep(Duration::from_millis(250));

    // Without the option the name is gone for as long as the 4-tuple lives,
    // which is the whole reason the option exists.
    match try_bind(&srv_addr, false) {
        Err(err) => r.check(
            &format!("a TIME-WAIT name is EADDRINUSE without SO_REUSEADDR (got {err})"),
            err == Errno::ADDRINUSE,
        ),
        Ok(_) => r.check(
            "a TIME-WAIT name is EADDRINUSE without SO_REUSEADDR (it bound)",
            false,
        ),
    }
    let rebind = match try_bind(&srv_addr, true) {
        Ok(sock) => {
            r.check("and comes back to a restart that asks for it", true);
            sock
        }
        Err(err) => {
            r.check(
                &format!("and comes back to a restart that asks for it (got {err})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    // The taken-back name has to be *usable*, not merely accepted: a listener
    // on it answers a fresh client, whose 4-tuple is its own and so is not the
    // one still winding down.
    match listen(&rebind, 1) {
        Ok(()) => r.check("the restart listens on the name it took back", true),
        Err(err) => r.check(
            &format!("the restart listens on the name it took back (got {err})"),
            false,
        ),
    }
    let fresh = stream();
    match connect(&fresh, &srv_addr).and_then(|()| accept(&rebind)) {
        Ok(conn) => {
            r.check("and a fresh client reaches it", true);
            let carried = send(&fresh, REQUEST, SendFlags::empty())
                .is_ok_and(|n| n == REQUEST.len())
                && recvfrom(&conn, &mut buf, RecvFlags::empty())
                    .is_ok_and(|(n, _, _)| n == REQUEST.len() && &buf[..n] == REQUEST);
            r.check("carrying its own bytes", carried);
            drop(conn);
        }
        Err(err) => r.check(&format!("and a fresh client reaches it (got {err})"), false),
    }
    drop(fresh);

    // The row that decides whether the rule was read right: the holder did
    // *not* have the option, so the new socket's own is not enough.
    let noopt = stream();
    let _ = bind(&noopt, &loopback);
    let _ = listen(&noopt, 1);
    let noopt_addr = getsockname(&noopt)
        .ok()
        .and_then(as_v4)
        .expect("the second server's name");
    let noopt_client = stream();
    let noopt_conn = match connect(&noopt_client, &noopt_addr).and_then(|()| accept(&noopt)) {
        Ok(conn) => conn,
        Err(err) => {
            r.check(
                &format!("the optionless server's connection is made (got {err})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    r.check(
        "an accepted connection of an optionless listener inherits nothing",
        !socket_reuseaddr(&noopt_conn).unwrap_or(true),
    );
    drop(noopt);
    drop(noopt_conn);
    drop(noopt_client);
    thread::sleep(Duration::from_millis(250));
    match try_bind(&noopt_addr, true) {
        Err(err) => r.check(
            &format!("a holder without the option keeps its name (got {err})"),
            err == Errno::ADDRINUSE,
        ),
        Ok(_) => r.check(
            "a holder without the option keeps its name (it bound)",
            false,
        ),
    }

    // And the exemption is not about `TIME-WAIT` at all: a *live* connection
    // holds the name by the same clause, so a port can be taken back while a
    // connection is still open on it - `SO_REUSEPORT` and `SO_REUSEADDR` are
    // two different promises and neither of them is "nothing else may use
    // this".
    let held = stream();
    let _ = set_socket_reuseaddr(&held, true);
    let _ = bind(&held, &loopback);
    let _ = listen(&held, 4);
    let held_addr = getsockname(&held)
        .ok()
        .and_then(as_v4)
        .expect("the third server's name");
    let held_client = stream();
    let held_conn = match connect(&held_client, &held_addr).and_then(|()| accept(&held)) {
        Ok(conn) => conn,
        Err(err) => {
            r.check(
                &format!("the long-lived server's connection is made (got {err})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    // The listener goes; the *connection* stays open and is the holder now.
    drop(held);
    thread::sleep(Duration::from_millis(100));
    match try_bind(&held_addr, false) {
        Err(err) => r.check(
            &format!("a live connection holds its name without the option (got {err})"),
            err == Errno::ADDRINUSE,
        ),
        Ok(_) => r.check(
            "a live connection holds its name without the option (it bound)",
            false,
        ),
    }
    let shared = match try_bind(&held_addr, true) {
        Ok(sock) => {
            r.check("and yields it when both sides asked", true);
            sock
        }
        Err(err) => {
            r.check(
                &format!("and yields it when both sides asked (got {err})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };
    // Usable, again: the listener that now shares the name with the open
    // connection answers a client of its own, while the connection keeps the
    // one it already had.
    let listens_anyway = listen(&shared, 1).is_ok();
    let fresh2 = stream();
    let both_carry = match connect(&fresh2, &held_addr).and_then(|()| accept(&shared)) {
        Ok(conn) => {
            let carried = send(&fresh2, REQUEST, SendFlags::empty())
                .is_ok_and(|n| n == REQUEST.len())
                && recvfrom(&conn, &mut buf, RecvFlags::empty())
                    .is_ok_and(|(n, _, _)| n == REQUEST.len() && &buf[..n] == REQUEST);
            drop(conn);
            carried
        }
        Err(_) => false,
    };
    r.check(
        "a listener can share a name with an open connection, and both serve",
        listens_anyway && both_carry,
    );
    drop(fresh2);
    drop(held_conn);
    drop(held_client);

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
