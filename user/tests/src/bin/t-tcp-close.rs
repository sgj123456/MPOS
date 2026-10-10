//! t-tcp-close - does a `close(2)` tell the peer, or is it silent?
//!
//! mpos's TCP is outbound-only (IPv4 `listen` is refused, by `t-tcp`), so the
//! only peer available is one the host is running: slirp forwards a guest
//! connect to `10.0.2.2:PORT` to the host's loopback on the same port. That
//! makes the *host* the instrument, and this program the thing it measures -
//! it connects, says a word, and closes. An orderly close leaves a `FIN` on the
//! wire and the host's `recv` returns end-of-stream; a close that tears the
//! socket out of its set before the stack has polled removes any chance of
//! that, and the host sits in silence until it gives up.
//!
//! The measurement is therefore *out of process*: what the guest half can
//! claim is that the close it performs succeeds, and that the option meant to
//! change it is recorded and read back. Whether a `FIN` reached the far end is
//! the host script's to say (`NOTES.md` §4.3 says how to run it), and with
//! nothing listening - the normal state of a sweep - the connect is refused and
//! the connected half is named and not made.
//!
//! What survives without a peer is the option pair, and it is worth pinning on
//! both machines: `SO_LINGER{on, 0}` asks for a reset rather than an orderly
//! close. This kernel records the request and reports it back, but cannot send
//! the reset (smoltcp 0.13's `abort()` transmits nothing, and its resets are
//! reply-driven), so the close stays orderly - which is exactly the gap the
//! host instrument sees. Pinning the round-trip means a change to that
//! recording turns up here rather than as a mystery in a twelve-second wait.
//!
//! A host run has no guest network to speak of: `10.0.2.2` is slirp's address
//! for *its* caller, not a route this machine has, so connecting there would
//! measure the sandbox's routing rather than a close. The option claims are
//! made; the connected ones are not.

use mpos_user_tests::{Report, host_run};
use rustix::net::{AddressFamily, SocketType, socket};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsRawFd, IntoRawFd};

/// slirp's host, and the port the measuring script listens on.
const PORT: u16 = 4599;
const HOST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// `libc` answers a failure the way this target's C does - a plain `-1` with
/// the errno beside it - and these claims compare against a negated errno.
fn rc(rc: libc::c_int) -> isize {
    if rc == -1 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc as isize
    }
}

/// `struct linger { int l_onoff; int l_linger; }`, the pair the socket door
/// exchanges. `rustix` binds no socket option for it, so both directions are
/// the raw door and the shape is this program's.
fn set_linger(fd: libc::c_int, pair: [libc::c_int; 2]) -> isize {
    rc(unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            pair.as_ptr() as *const libc::c_void,
            std::mem::size_of::<[libc::c_int; 2]>() as libc::socklen_t,
        )
    })
}

fn get_linger(fd: libc::c_int) -> (isize, [libc::c_int; 2]) {
    let mut back = [0i32; 2];
    let mut len = std::mem::size_of::<[libc::c_int; 2]>() as libc::socklen_t;
    let r = rc(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            back.as_mut_ptr() as *mut libc::c_void,
            &mut len,
        )
    });
    (r, back)
}

fn main() {
    println!("t-tcp-close");
    let mut r = Report::new();

    // The option, on a socket that needs no peer: what it reads before
    // anything asks for anything, that the reset request is accepted, and that
    // what it reads afterwards is what was asked for.
    let Ok(opts) = socket(AddressFamily::INET, SocketType::STREAM, None) else {
        r.check("a TCP socket opens", false);
        r.finish();
    };
    r.check("a TCP socket opens", true);
    let raw = opts.as_raw_fd();

    let (got, pair) = get_linger(raw);
    r.check("getsockopt(SO_LINGER) succeeds", got == 0);
    r.check(
        "a socket that never asked reports l_onoff 0, l_linger 0",
        pair == [0, 0],
    );
    r.check("SO_LINGER{on, 0} is accepted", set_linger(raw, [1, 0]) == 0);
    let (got, pair) = get_linger(raw);
    r.check("getsockopt(SO_LINGER) succeeds after the set", got == 0);
    r.check(
        "and reports the l_onoff/l_linger pair it was given",
        pair == [1, 0],
    );
    drop(opts);

    // The half that needs a listener. Not made on a host run: 10.0.2.2 is the
    // address slirp gives *its* guest, and a connect to it from outside
    // measures whether the sandbox has a route to a private address.
    if host_run() {
        println!(
            "note: no host listener is reachable at 10.0.2.2:4599 from outside the guest - slirp's host address is the guest's, so the connected half of this test is named and not made here"
        );
        r.finish();
    }

    let Ok(sock) = socket(AddressFamily::INET, SocketType::STREAM, None) else {
        r.check("a second TCP socket opens", false);
        r.finish();
    };
    let peer = SocketAddrV4::new(HOST, PORT);
    match rustix::net::connect(&sock, &peer) {
        Ok(()) => {}
        Err(e) => {
            // Nothing listening: the ordinary state. Say so and pass - the
            // measurement belongs to the host script.
            println!("note: no host listener on 10.0.2.2:4599 ({e}) - nothing to close towards");
            r.finish();
        }
    }
    r.check("the guest reaches the host through slirp", true);
    r.check(
        "the greeting is sent",
        matches!(rustix::io::write(&sock, b"hi"), Ok(2)),
    );
    // The act under measurement, from the guest's side only: the close must
    // succeed, and must not be the last thing the stack does quietly. The raw
    // door is what the claim needs: `rustix::io::close` takes the descriptor
    // and returns nothing, so there would be no answer to pin.
    let closing = IntoRawFd::into_raw_fd(sock);
    r.check("close succeeds", rc(unsafe { libc::close(closing) }) == 0);

    r.finish()
}
