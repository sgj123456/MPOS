//! t-sockbuf - a socket's buffer size is **Linux's number**, not the caller's:
//! doubled, clamped from below at each option's floor, capped at the sysctl
//! maximum, and never an error for a negative amount. On both machines.
//!
//! `SO_SNDBUF`/`SO_RCVBUF` are the one pair of socket options whose *value* is
//! part of the ABI rather than a hint: a caller computes a datagram size as
//! `getsockopt(SO_SNDBUF) / 2`, and every program that tunes a socket checks
//! the figure it reads back. This kernel used to store what it was asked for
//! and read that back, which no caller's arithmetic survives.
//!
//! Measured on a current host before anything was written
//! (`.workbuddy/probe_sockbuf.c`, Linux 7.2.11, `net.core.wmem_max` and
//! `rmem_max` both 4194304):
//!
//! * `setsockopt` stores **twice** the ask - 2304 reads back 4608, 10000 reads
//!   back 20000, 100000 reads back 200000;
//! * each option has its own floor, applied to the doubled figure: `SO_SNDBUF`
//!   never reads below **4608** (asking 100, or 1, or 0, all read 4608) and
//!   `SO_RCVBUF` never below **2304** (asking 100 reads 2304, while 2048 reads
//!   4096 - the doubling is visible again as soon as the ask is above half the
//!   floor);
//! * the ceiling is the sysctl maximum applied to the *ask*: asking ten million
//!   reads back 8388608, twice the four-mebibyte maximum;
//! * a **negative** ask is not an error. Linux reads the `int` as unsigned,
//!   which the maximum then caps, so `-1` means "as much as allowed":
//!   `setsockopt` answers 0 and the option reads 8388608.
//!
//! # What this program pins
//!
//! The transformations above, on a datagram socket and (for the doubling and a
//! stream's own default) on a stream socket too. The floors and the ceiling are
//! this kernel's constants while they are the host's sysctls on a host run, and
//! both were chosen to be Linux's - `src/net/mod.rs`'s `SOCK_MIN_SNDBUF`,
//! `SOCK_MIN_RCVBUF` and `SOCK_MEM_MAX` - which is what lets the same
//! assertions run in both places.
//!
//! # What is *not* here
//!
//! A **datagram** socket's receive side is still this kernel's fixed ring
//! (`src/net/mod.rs`'s `UDP_PAYLOAD_BYTES`), so `SO_RCVBUF` there is the
//! number a caller reads and not a window this kernel resizes - named in
//! `docs/uapi/` §5.2. A **stream** socket's number *is* its receive window now
//! (item 98): a listener hands the option to the socket it accepts, and the
//! window an unread connection absorbs scales with it. The claims below
//! measure both the inheritance and the effect, on the loopback, with the
//! server deliberately never reading.

use mpos_user_tests::Report;
use rustix::fd::OwnedFd;
use rustix::net::sockopt::{
    set_socket_recv_buffer_size, set_socket_send_buffer_size, socket_recv_buffer_size,
    socket_send_buffer_size,
};
use rustix::net::{
    AddressFamily, SendFlags, SocketAddrV4, SocketType, accept, bind, connect, getsockname, listen,
    send, socket,
};
use std::net::Ipv4Addr;

/// The two floors Linux clamps at, from `src/net/mod.rs`.
const MIN_SNDBUF: usize = 4608;
const MIN_RCVBUF: usize = 2304;
/// Twice `net.core.wmem_max`/`rmem_max`, the ceiling a negative ask reads back.
const CEILING: usize = 8 * 1024 * 1024;

/// The loopback, the only address a server can be reached at from inside this
/// machine.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// A loopback connection whose server never reads, with the listener's
/// `SO_RCVBUF` asked for `ask` either before or after `listen`. The client's
/// send buffer is asked to be small; Linux sizes a stream's send ring with it,
/// so the client's own queue stays small there. This kernel's stream send ring
/// is fixed (`TCP_TX_BYTES`), so the guest's absolute figures include its
/// 16 KiB queue - which is why the claims below compare the three asks and
/// never assert an absolute capacity.
fn unread_pair(ask: usize, after_listen: bool) -> Option<(OwnedFd, OwnedFd, OwnedFd)> {
    let listener = socket(AddressFamily::INET, SocketType::STREAM, None).ok()?;
    if !after_listen {
        set_socket_recv_buffer_size(&listener, ask).ok()?;
    }
    bind(&listener, &SocketAddrV4::new(LOOPBACK, 0)).ok()?;
    listen(&listener, 1).ok()?;
    if after_listen {
        set_socket_recv_buffer_size(&listener, ask).ok()?;
    }
    let port = SocketAddrV4::try_from(getsockname(&listener).ok()?)
        .ok()?
        .port();
    let client = socket(AddressFamily::INET, SocketType::STREAM, None).ok()?;
    set_socket_send_buffer_size(&client, 4096).ok()?;
    connect(&client, &SocketAddrV4::new(LOOPBACK, port)).ok()?;
    let server = accept(&listener).ok()?;
    Some((listener, server, client))
}

/// Push 4 KiB sends at `client` without blocking until it refuses, and report
/// how many bytes it took. The server is not reading, so the count is the
/// client's send queue plus the server's advertised window.
///
/// A refusal is retried for a bounded while: this kernel's stack processes the
/// loopback's ACKs on its poll ticks, so the first `EAGAIN` is the client's
/// queue full with the peer's ACKs still in flight - where Linux has already
/// run them inside the failing `send`. The window is closed for good once the
/// server's ring is full, so the retries stop after `IDLE` of them with no
/// progress; on a host that costs one short sleep per phase.
fn push_until_full(client: &OwnedFd) -> usize {
    const IDLE: usize = 32;
    let chunk = [b'x'; 4096];
    let mut total = 0;
    let mut idle = 0;
    while total < 8 * 1024 * 1024 {
        match send(client, &chunk, SendFlags::DONTWAIT) {
            Ok(n) if n > 0 => {
                total += n;
                idle = 0;
            }
            _ => {
                idle += 1;
                if idle > IDLE {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
    total
}

fn main() {
    println!("t-sockbuf");
    let mut r = Report::new();

    let dgram = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("a datagram socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("a datagram socket opens", true);

    // 1. The doubling, on both options. 2304 is exactly half the send floor, so
    //    the doubled figure is the floor itself and the claim needs no sysctl.
    r.check(
        "asking 2304 for SO_SNDBUF reads back 4608",
        set_socket_send_buffer_size(&dgram, 2304).is_ok()
            && socket_send_buffer_size(&dgram) == Ok(4608),
    );
    r.check(
        "asking 10000 for SO_SNDBUF reads back 20000",
        set_socket_send_buffer_size(&dgram, 10000).is_ok()
            && socket_send_buffer_size(&dgram) == Ok(20000),
    );
    r.check(
        "asking 100000 for SO_RCVBUF reads back 200000",
        set_socket_recv_buffer_size(&dgram, 100_000).is_ok()
            && socket_recv_buffer_size(&dgram) == Ok(200_000),
    );

    // 2. The two floors are different numbers, which is the point of measuring
    //    them apart: 100 asked for either is under both, and the answers are
    //    4608 and 2304.
    r.check(
        "the send floor is 4608: asking 100 reads 4608",
        set_socket_send_buffer_size(&dgram, 100).is_ok()
            && socket_send_buffer_size(&dgram) == Ok(MIN_SNDBUF),
    );
    r.check(
        "the receive floor is 2304: asking 100 reads 2304",
        set_socket_recv_buffer_size(&dgram, 100).is_ok()
            && socket_recv_buffer_size(&dgram) == Ok(MIN_RCVBUF),
    );
    //    And the doubling is still visible above half of it: 2048 asked for the
    //    receive side is 4096 back, not the floor.
    r.check(
        "asking 2048 for SO_RCVBUF reads back 4096",
        set_socket_recv_buffer_size(&dgram, 2048).is_ok()
            && socket_recv_buffer_size(&dgram) == Ok(4096),
    );

    // 3. A negative amount is the ceiling, not an error. `rustix` takes a
    //    `usize`, so this one request rides `libc` - the sign is the claim.
    let minus_one: libc::c_int = -1;
    // SAFETY: one integer the process owns, and an option whose value really is
    // an `int`.
    let rc = unsafe {
        libc::setsockopt(
            std::os::fd::AsRawFd::as_raw_fd(&dgram),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            &minus_one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    r.check("a negative SO_SNDBUF is accepted", rc == 0);
    r.check(
        "and it means the ceiling",
        socket_send_buffer_size(&dgram) == Ok(CEILING),
    );

    // 4. A fresh socket of each family reports its own family's default, and
    //    the datagram one is the larger - Linux reads `net.core.*mem_default`
    //    for a datagram socket and `tcp_wmem`/`tcp_rmem`'s first entries for a
    //    stream (`src/net/mod.rs`'s four constants).
    let fresh_dgram = socket(AddressFamily::INET, SocketType::DGRAM, None);
    let fresh_stream = socket(AddressFamily::INET, SocketType::STREAM, None);
    let (d, s) = match (fresh_dgram, fresh_stream) {
        (Ok(d), Ok(s)) => (d, s),
        _ => {
            r.check("both families open a fresh socket", false);
            r.finish();
        }
    };
    r.check("both families open a fresh socket", true);
    let dflt_d = socket_send_buffer_size(&d).unwrap_or(0);
    let dflt_s = socket_send_buffer_size(&s).unwrap_or(0);
    r.check(
        "a fresh datagram socket's send buffer is the larger of the two",
        dflt_d > dflt_s && dflt_s > 0,
    );
    println!("note: defaults sndbuf dgram={dflt_d} stream={dflt_s}");

    // 5. The doubling is the same rule on a stream socket: the option is a
    //    `SOL_SOCKET` one, and the family only changes the default it started
    //    from.
    r.check(
        "a stream socket doubles too: asking 10000 reads 20000",
        set_socket_send_buffer_size(&s, 10000).is_ok() && socket_send_buffer_size(&s) == Ok(20000),
    );

    // 6. A stream's SO_RCVBUF is its **window**, not only a number. Linux
    //    clones the listener's `sk_rcvbuf` onto the accepted socket
    //    (`inet_csk_clone_lock`), whether the option was set before or after
    //    `listen` - measured on a current host with
    //    `.workbuddy/probe_tcp_rcvbuf.c` (Linux 7.2.11): ask 2048 -> accepted
    //    4096, ask 32768 -> 65536, ask 262144 -> 524288.
    match unread_pair(2048, false) {
        Some((listener, server, client)) => {
            r.check(
                &format!(
                    "the accepted socket inherits the listener's SO_RCVBUF (got {:?})",
                    socket_recv_buffer_size(&server)
                ),
                socket_recv_buffer_size(&server) == Ok(4096),
            );
            drop((listener, server, client));
        }
        None => r.check("a loopback connection with a sized listener is made", false),
    }
    match unread_pair(32768, true) {
        Some((listener, server, client)) => {
            r.check(
                &format!(
                    "a change made after listen is inherited too (got {:?})",
                    socket_recv_buffer_size(&server)
                ),
                socket_recv_buffer_size(&server) == Ok(65536),
            );
            drop((listener, server, client));
        }
        None => r.check(
            "a loopback connection with a post-listen size is made",
            false,
        ),
    }

    // 7. The effect: an unread connection absorbs about what its window says.
    //    The three asks are the option's own shape (double, floor) and the
    //    comparisons are relative, because the absolute figure includes the
    //    client's send queue, which is not the subject.
    let pushed = |ask: usize| -> usize {
        match unread_pair(ask, true) {
            Some((listener, server, client)) => {
                let n = push_until_full(&client);
                drop((listener, server, client));
                n
            }
            None => 0,
        }
    };
    let small = pushed(2048);
    let large = pushed(32768);
    let beyond = pushed(262144);
    println!("note: unread TCP capacity ask 2048={small}, 32768={large}, 262144={beyond}");
    r.check(
        &format!("a small window fills sooner than a larger one (small={small}, large={large})"),
        large > small.saturating_mul(2),
    );
    r.check(
        &format!("a larger ask still buys a larger window (large={large}, beyond={beyond})"),
        beyond > large,
    );

    r.finish();
}
