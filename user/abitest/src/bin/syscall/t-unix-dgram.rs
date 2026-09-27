//! t-unix-dgram - the other half of AF_UNIX: names that receive whole
//! messages instead of byte streams.
//!
//! This is the shape `/dev/log` is - busybox syslog binds a datagram
//! socket and every logger on the machine sends it one message at a time,
//! no `listen`, no `accept`. Pinned here, per unix(7): a datagram socket
//! binds and receives at a name; `listen` on it is `EOPNOTSUPP` (there is
//! no accept queue to open - the message queue is already open); a send
//! with no name and no connected peer is `ENOTCONN`; a `sendto` by name
//! lands whole in the receiver's queue; `recvfrom` answers the sender's
//! bound name, or the empty one for a sender that never bound; a read
//! smaller than the message truncates that message and nothing after it;
//! a `connect` pins the destination for later writes and a second connect
//! answers `EISCONN`; naming a destination on a connected send is `EISCONN`
//! too; `SEQPACKET` opens - udevd's control-socket shape - and is served
//! as the stream this family carries until a pair connects it; a datagram
//! aimed at a
//! stream listener's name is `ECONNREFUSED`, because no queue of messages
//! lives behind it; and a full receive queue answers the sender `EAGAIN`
//! rather than dropping what was sent.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static RECV: &[u8] = b"/tmp/t-dgram.sock\0";
static RECV_T: &[u8] = b"/tmp/t-dgram.sock";
static SENDER_NAME: &[u8] = b"/tmp/t-dgram-from.sock\0";
static SENDER_T: &[u8] = b"/tmp/t-dgram-from.sock";
static FULL_NAME: &[u8] = b"/tmp/t-dgram-full.sock\0";
static FULL_T: &[u8] = b"/tmp/t-dgram-full.sock";
static STREAM_NAME: &[u8] = b"/tmp/t-dgram-stream.sock\0";
static STREAM_T: &[u8] = b"/tmp/t-dgram-stream.sock";

/// `sendto(fd, buf, len, 0, addr, addrlen)` with a unix address; `None`
/// sends to the connected peer.
fn sendto_un(fd: i32, buf: &[u8], addr: Option<(&lin::SockAddrUn, usize)>) -> isize {
    let (aptr, alen) = match addr {
        Some((a, len)) => (a as *const lin::SockAddrUn as usize, len),
        None => (0, 0),
    };
    lin::raw_sys6(
        lin::nr::SENDTO,
        fd as usize,
        buf.as_ptr() as usize,
        buf.len(),
        0,
        aptr,
        alen,
    )
}

/// `recvfrom` with a unix `src_addr`: the answer is the byte count, and
/// the filled address length through `srclen`.
fn recvfrom_un(
    fd: i32,
    buf: &mut [u8],
    flags: usize,
    src: &mut lin::SockAddrUn,
    srclen: &mut usize,
) -> isize {
    // `addrlen` is in-out: Linux reads the caller's capacity first, so the
    // size of the buffer has to be handed in every time, not the length the
    // previous call left behind (that is how a 25-byte name got truncated
    // to 3 once, and stayed truncated).
    *srclen = core::mem::size_of::<lin::SockAddrUn>();
    lin::raw_sys6(
        lin::nr::RECVFROM,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        flags,
        src as *mut lin::SockAddrUn as usize,
        srclen as *mut usize as usize,
    )
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-unix-dgram\n");
    let mut failed = false;

    let _ = lin::unlinkat(lin::AT_FDCWD, RECV, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, SENDER_NAME, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, FULL_NAME, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, STREAM_NAME, 0);

    // The three shapes of the path family open. SEQPACKET is udevd's
    // control-socket shape: a *named* one is served as the stream this
    // family carries - `listen` succeeds on it where a datagram answers
    // EOPNOTSUPP - and only a connected pair carries messages.
    let rs = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    lin::check_true(
        &mut failed,
        b"socket(AF_UNIX, SOCK_DGRAM) succeeds",
        rs >= 0,
    );
    let sp = lin::raw_sys(
        lin::nr::SOCKET,
        lin::AF_UNIX,
        5, /* SOCK_SEQPACKET */
        0,
    );
    lin::check_true(&mut failed, b"SOCK_SEQPACKET opens", sp >= 0);
    if sp >= 0 {
        lin::check_eq(
            &mut failed,
            b"a named SOCK_SEQPACKET socket is served as a stream: listen succeeds",
            lin::listen(sp as i32, 4),
            0,
        );
    }
    let cs = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    lin::check_true(&mut failed, b"a second datagram socket opens", cs >= 0);

    let (raddr, rlen) = lin::SockAddrUn::new(RECV_T);
    lin::check_eq(
        &mut failed,
        b"bind plants the receiver's name",
        lin::bind_un(rs, &raddr, rlen),
        0,
    );
    // `listen` is the stream call: a datagram socket has no accept queue
    // to open, and says so rather than pretending to build one.
    lin::check_eq(
        &mut failed,
        b"listen on a datagram socket answers EOPNOTSUPP",
        lin::listen(rs, 4),
        lin::EOPNOTSUPP,
    );

    // A send with no destination anywhere: no name on the call, no peer
    // pinned. The write door answers `ENOTCONN`.
    let w = lin::raw_sys(lin::nr::WRITE, cs as usize, b"x".as_ptr() as usize, 1);
    lin::check_eq(
        &mut failed,
        b"write to an unconnected datagram answers ENOTCONN",
        w,
        lin::ENOTCONN,
    );

    // The first delivery: an unbound sender sends to the name, and the
    // receiver takes the whole message - with the sender's *empty* name
    // back, because an unbound sender has none to report.
    let n = sendto_un(cs, b"hello", Some((&raddr, rlen)));
    lin::check_eq(&mut failed, b"sendto by name delivers whole", n, 5);
    let mut buf = [0u8; 16];
    let mut src = lin::SockAddrUn {
        sun_family: 0,
        sun_path: [0xAA; 108],
    };
    let mut srclen: usize = core::mem::size_of::<lin::SockAddrUn>();
    let r = recvfrom_un(rs, &mut buf, 0, &mut src, &mut srclen);
    lin::check_eq(&mut failed, b"recvfrom takes the message whole", r, 5);
    lin::check_true(
        &mut failed,
        b"the bytes are the bytes",
        &buf[..5] == b"hello",
    );
    lin::check_eq(
        &mut failed,
        b"the unnamed sender answers with the empty name",
        src.sun_family as isize,
        lin::AF_UNIX as isize,
    );
    lin::check_eq(
        &mut failed,
        b"and three bytes of it were the answer",
        srclen as isize,
        3,
    );

    // Truncation is per-message: the unread tail of one datagram is gone,
    // and the *next* message still stands whole behind it.
    let n = sendto_un(cs, b"123456789", Some((&raddr, rlen)));
    lin::check_eq(&mut failed, b"the nine-byte send reports nine", n, 9);
    let n = sendto_un(cs, b"next", Some((&raddr, rlen)));
    lin::check_eq(&mut failed, b"the four-byte send reports four", n, 4);
    let mut small = [0u8; 4];
    let r = recvfrom_un(rs, &mut small, 0, &mut src, &mut srclen);
    lin::check_eq(&mut failed, b"the small read takes what fits", r, 4);
    lin::check_true(
        &mut failed,
        b"and it is the message's front",
        &small == b"1234",
    );
    let r = recvfrom_un(rs, &mut buf, 0, &mut src, &mut srclen);
    lin::check_eq(
        &mut failed,
        b"the next message arrives whole after a truncation",
        r,
        4,
    );
    lin::check_true(
        &mut failed,
        b"the second message is intact",
        &buf[..4] == b"next",
    );

    // A bound sender is named by what it bound: that is the reply address
    // a logger's reader could send back to.
    let (saddr, slen_addr) = lin::SockAddrUn::new(SENDER_T);
    lin::check_eq(
        &mut failed,
        b"the sender binds its own name",
        lin::bind_un(cs, &saddr, slen_addr),
        0,
    );
    let n = sendto_un(cs, b"ping", Some((&raddr, rlen)));
    lin::check_eq(&mut failed, b"the named send lands", n, 4);
    let r = recvfrom_un(rs, &mut buf, 0, &mut src, &mut srclen);
    lin::check_eq(&mut failed, b"recvfrom takes the ping", r, 4);
    lin::check_eq(
        &mut failed,
        b"the answer carries the whole sender name",
        srclen as isize,
        (3 + SENDER_T.len()) as isize,
    );
    lin::check_true(
        &mut failed,
        b"and the name is the one that bound",
        &src.sun_path[..SENDER_T.len() - 1] == &SENDER_T[..SENDER_T.len() - 1],
    );
    if &src.sun_path[..SENDER_T.len() - 1] != &SENDER_T[..SENDER_T.len() - 1] {
        lin::puts(b"      src raw: fam=");
        let mut hex = [0u8; 2];
        for (i, b) in src.sun_path[..26].iter().enumerate() {
            hex[0] = b"0123456789abcdef"[(b >> 4) as usize];
            hex[1] = b"0123456789abcdef"[(b & 0xf) as usize];
            lin::puts(&hex);
            if i % 8 == 7 {
                lin::puts(b" ");
            }
        }
        lin::puts(b"\n");
    }

    // The connected datagram: `connect` pins the destination, the byte
    // doors aim at it, and a second connect - or a named sendto - is
    // `EISCONN`.
    let cs2 = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    lin::check_true(&mut failed, b"a connector socket opens", cs2 >= 0);
    lin::check_eq(
        &mut failed,
        b"connect pins the receiver's name",
        lin::connect_un(cs2, &raddr, rlen),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"a second connect answers EISCONN",
        lin::connect_un(cs2, &raddr, rlen),
        lin::EISCONN,
    );
    let w = lin::raw_sys(
        lin::nr::WRITE,
        cs2 as usize,
        b"via-peer".as_ptr() as usize,
        8,
    );
    lin::check_eq(
        &mut failed,
        b"write on a connected datagram lands whole",
        w,
        8,
    );
    let n = sendto_un(cs2, b"named", Some((&raddr, rlen)));
    lin::check_eq(
        &mut failed,
        b"a named sendto on a connected sender answers EISCONN",
        n,
        lin::EISCONN,
    );
    let r = recvfrom_un(rs, &mut buf, 0, &mut src, &mut srclen);
    lin::check_eq(&mut failed, b"the connected write reads back", r, 8);
    lin::check_true(
        &mut failed,
        b"with the connected sender's empty name",
        &buf[..8] == b"via-peer" && srclen == 3,
    );

    // A datagram aimed at a *stream* listener's name has no queue to land
    // in: the name answers, but not to this family's messages.
    let sl = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, 1 /* SOCK_STREAM */, 0) as i32;
    let (staddr, stlen) = lin::SockAddrUn::new(STREAM_T);
    lin::check_eq(
        &mut failed,
        b"a stream socket binds a name",
        lin::bind_un(sl, &staddr, stlen),
        0,
    );
    lin::check_eq(&mut failed, b"and listens", lin::listen(sl, 4), 0);
    let fresh = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    let n = sendto_un(fresh, b"nope", Some((&staddr, stlen)));
    lin::check_eq(
        &mut failed,
        b"a datagram to a stream name answers ECONNREFUSED",
        n,
        lin::ECONNREFUSED,
    );
    lin::check_eq(
        &mut failed,
        b"and a connect to it answers the same",
        lin::connect_un(fresh, &staddr, stlen),
        lin::ECONNREFUSED,
    );
    lin::close(sl);
    lin::close(fresh);

    // The queue has a bound: sixty-four messages stand in it, and the
    // sixty-fifth is refused to the sender rather than dropped. Draining
    // one makes room for another - the refusal is a full queue's answer,
    // not the name's.
    let (faddr, flen) = lin::SockAddrUn::new(FULL_T);
    let full_rs = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    let full_cs = lin::raw_sys(lin::nr::SOCKET, lin::AF_UNIX, lin::SOCK_DGRAM, 0) as i32;
    lin::check_eq(
        &mut failed,
        b"the cap receiver binds",
        lin::bind_un(full_rs, &faddr, flen),
        0,
    );
    let mut accepted = 0usize;
    for _ in 0..64 {
        if sendto_un(full_cs, b"m", Some((&faddr, flen))) == 1 {
            accepted += 1;
        }
    }
    lin::check_eq(
        &mut failed,
        b"the whole queue takes its messages",
        accepted as isize,
        64,
    );
    let n = sendto_un(full_cs, b"m", Some((&faddr, flen)));
    lin::check_eq(
        &mut failed,
        b"the sixty-fifth answers EAGAIN to the sender",
        n,
        lin::EAGAIN,
    );
    let r = recvfrom_un(full_rs, &mut buf, lin::MSG_DONTWAIT, &mut src, &mut srclen);
    lin::check_eq(&mut failed, b"a drained read makes room again", r, 1);
    let n = sendto_un(full_cs, b"m", Some((&faddr, flen)));
    lin::check_eq(&mut failed, b"and the next send lands", n, 1);

    // An empty queue and a nonblocking reader is EAGAIN too - the rule
    // that lets a poll loop step over the socket.
    let r = recvfrom_un(rs, &mut buf, lin::MSG_DONTWAIT, &mut src, &mut srclen);
    lin::check_eq(
        &mut failed,
        b"a DONTWAIT read of an empty queue answers EAGAIN",
        r,
        lin::EAGAIN,
    );

    // Cleanup: the names go, the descriptors go.
    let _ = lin::unlinkat(lin::AT_FDCWD, RECV, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, SENDER_NAME, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, FULL_NAME, 0);
    let _ = lin::unlinkat(lin::AT_FDCWD, STREAM_NAME, 0);
    lin::close(rs);
    lin::close(cs);
    lin::close(cs2);
    lin::close(full_rs);
    lin::close(full_cs);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-unix-dgram: panicked\n")
}
