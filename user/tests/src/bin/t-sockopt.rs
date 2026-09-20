//! t-sockopt - the socket options a libc sets without looking.
//!
//! No program asks whether `SO_REUSEADDR` is supported before setting it;
//! it sets it and looks at the answer. A stack that refuses the options it
//! does not implement turns an ordinary server's start-up into an error.
//!
//! What is pinned here is that the ordinary ones are accepted, and that the
//! two a caller actually reads back - what kind of socket this is, and
//! whether anything has gone wrong on it - tell the truth. `SO_TYPE` is the
//! load-bearing one: a program handed an inherited descriptor asks what it
//! is before using it.

use rustix::net::sockopt::socket_type;
use rustix::net::{AddressFamily, SocketType, socket};

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
    println!("t-sockopt");
    let mut r = Report { failed: false };

    // No sockets, nothing to ask about options. Reported rather than
    // unwrapped: a test that dies on a panic leaves the log saying only
    // "Aborted", when the whole point of the run is to name what went wrong.
    let (tcp, udp) = match (
        socket(AddressFamily::INET, SocketType::STREAM, None),
        socket(AddressFamily::INET, SocketType::DGRAM, None),
    ) {
        (Ok(tcp), Ok(udp)) => (tcp, udp),
        (stream, datagram) => {
            r.check(
                &format!("sockets are created (stream {stream:?}, datagram {datagram:?})"),
                false,
            );
            println!("FAILED");
            std::process::exit(1);
        }
    };

    // The option every server sets before it binds, whether or not the
    // stack has any reuse to do.
    match rustix::net::sockopt::set_socket_reuseaddr(&tcp, true) {
        Ok(()) => r.check("SO_REUSEADDR is accepted", true),
        Err(err) => r.check(&format!("SO_REUSEADDR is accepted (got {err})"), false),
    }

    // What kind of socket this is - the answer a program handed an
    // inherited descriptor asks for before using it.
    match socket_type(&tcp) {
        Ok(kind) => r.check(
            &format!("a stream socket says it is SOCK_STREAM (got {kind:?})"),
            kind == SocketType::STREAM,
        ),
        Err(err) => r.check(&format!("getsockopt(SO_TYPE) answers (got {err})"), false),
    }
    match socket_type(&udp) {
        // The same question has to give the other answer, or SO_TYPE is not
        // reporting anything.
        Ok(kind) => r.check(
            &format!("a datagram socket says it is SOCK_DGRAM (got {kind:?})"),
            kind == SocketType::DGRAM,
        ),
        Err(err) => r.check(&format!("getsockopt(SO_TYPE) answers (got {err})"), false),
    }

    // An option on a descriptor that is not a socket at all: the caller
    // hears ENOTSOCK, not success. stdout is as good a non-socket as any.
    let stdout = rustix::stdio::stdout();
    match socket_type(&stdout) {
        Ok(kind) => r.check(
            &format!("getsockopt on a non-socket fails (answered {kind:?})"),
            false,
        ),
        Err(err) => r.check(
            &format!("getsockopt on a non-socket fails (got {err})"),
            err == rustix::io::Errno::NOTSOCK,
        ),
    }

    if r.failed {
        println!("FAILED");
        std::process::exit(1);
    }
    println!("PASS");
}
