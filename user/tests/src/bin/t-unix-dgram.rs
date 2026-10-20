//! t-unix-dgram - the other half of AF_UNIX: names that receive whole messages
//! instead of byte streams.
//!
//! This is the shape `/dev/log` is - busybox syslog binds a datagram socket and
//! every logger on the machine sends it one message at a time, no `listen`, no
//! `accept`. Pinned here: a datagram socket binds and receives at a name;
//! `listen` on it is `EOPNOTSUPP` (there is no accept queue to open - the
//! message queue is already open); a send with no name and no connected peer is
//! `ENOTCONN`; a `sendto` by name lands whole in the receiver's queue;
//! `recvfrom` answers the sender's bound name; a read smaller than the message
//! truncates that message and nothing after it; a `connect` pins a destination
//! and the byte doors aim at it; `SEQPACKET` opens - udevd's control-socket
//! shape - and is served as the stream this family carries until a pair
//! connects it; and a full receive queue answers the sender `EAGAIN` rather
//! than dropping what was sent.
//!
//! # The doors
//!
//! The sockets, the names and the byte doors are `rustix`'s - `socket`, `bind`,
//! `connect`, `listen`, `sendto`, `recvfrom`, with `SocketAddrUnix` building and
//! parsing the addresses - because that is how a real logger's client is
//! written, and because the crate's errno constants *are* Linux's. Two claims
//! are the exception, and both are about a *number* rather than a name:
//! `recvfrom`'s `addrlen` is in-out, and the length the kernel writes back for a
//! sender that never bound is the whole claim. `rustix` parses that address into
//! a `SocketAddrUnix` and does not expose the count, so those go through
//! `libc::syscall(SYS_recvfrom, ..)` with a `libc::sockaddr_un`, as the no-std
//! program made them. Every other claim rides the crate.
//!
//! # The divergences, measured rather than assumed
//!
//! This program is the first batch whose claims the no-std suite had only ever
//! measured against this kernel - it ran on one machine, so "pinned, per
//! unix(7)" was an assertion and not a measurement. Running it against a host
//! Linux (`.workbuddy/probe_unix_dgram.c` is the raw-syscall probe that settled
//! each one) split the claims in two, and recorded a third that is served now:
//!
//!   * **The `SEQPACKET` listen.** Linux refuses `listen` on a *unbound*
//!     `SEQPACKET` socket with `EINVAL`, and accepts it on a bound one - which
//!     is what this claim's own name has always said ("a *named* SOCK_SEQPACKET
//!     socket"). The no-std version never bound it because this kernel did not
//!     require that; binding first makes the claim true on both machines rather
//!     than a divergence to explain away. What the listener *carries* is
//!     `t-unix-seqpacket`'s claim now: the accepted pair keeps message
//!     boundaries (item 78), where this file once said a named one was the
//!     byte stream.
//!
//!   * **Two answers this kernel gives and Linux does not**, each named in its
//!     own claim and accepted under `MPOS_ON_HOST=1` (where the host's own
//!     answer is printed rather than hidden): a second `connect` is `EISCONN`
//!     here where Linux accepts a repeat connect to the same name; and an
//!     unbound sender's name is three bytes here (the family word and the NUL
//!     that stands for the empty path) where Linux reports no name at all.
//!
//!     A third used to live here - `sendto` naming *any* destination on a
//!     connected datagram was `EISCONN` - and it is served now: Linux resolves
//!     and validates such a name exactly as an unconnected send does, so the
//!     peer `connect` pinned only decides where a send without a name goes.
//!
//!   * **The errno for a datagram aimed at a name of another type.** Linux's
//!     *path* lookup finds the socket by inode whatever its type and answers
//!     `EPROTOTYPE`; an *abstract* name lives in a type-keyed table, so a
//!     different type's name is simply absent and the answer is
//!     `ECONNREFUSED`. This kernel answered `ECONNREFUSED` for both, and the
//!     path half is served now (item 80). The claims below pin both halves,
//!     so a lookup that stopped being type-keyed would be caught too.
//!
//! # The one claim that is this kernel's own number
//!
//! The receive queue's *depth* is 64 here. Linux bounds the queue by the
//! socket's receive buffer (`SO_RCVBUF`), so its count is a byte budget and not
//! a message count at all; that claim is accepted under `MPOS_ON_HOST=1` too.
//! What must hold on both is the *shape* - a full queue refuses the sender with
//! `EAGAIN` instead of dropping what was sent, and a drained read makes room
//! again.

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrUnix, SocketType, bind, connect, listen,
    recvfrom, sendto, socket,
};

static RECV: &str = "/tmp/t-dgram.sock";
static SENDER: &str = "/tmp/t-dgram-from.sock";
static FULL: &str = "/tmp/t-dgram-full.sock";
static STREAM: &str = "/tmp/t-dgram-stream.sock";
static SEQPACKET: &str = "/tmp/t-dgram-seq.sock";
static SEQPACKET_TYPE: &str = "/tmp/t-dgram-seq-type.sock";
static QUIET: &str = "/tmp/t-dgram-quiet.sock";

/// How deep this kernel's receive queue is. Not Linux's rule - see the module
/// comment.
const QUEUE_DEPTH: usize = 64;

/// A claim about a call whose success is the claim.
fn check_ok<T>(r: &mut Report, name: &str, got: Result<T, Errno>) {
    match got {
        Ok(_) => r.check(name, true),
        Err(err) => r.check(&format!("{name} (got {err})"), false),
    }
}

/// A claim about a refusal: the answer must be the one Linux names, and a run
/// that got an `Ok` where a refusal was claimed says so.
fn check_errno<T: std::fmt::Debug>(r: &mut Report, name: &str, got: Result<T, Errno>, want: Errno) {
    match got {
        Ok(ok) => r.check(&format!("{name} (got Ok({ok:?}), want {want})"), false),
        Err(err) if err == want => r.check(name, true),
        Err(err) => r.check(&format!("{name} (got {err}, want {want})"), false),
    }
}

/// A claim whose answer is *this kernel's*, named as a divergence in the claim
/// itself: the gate pins `want`, and a host run accepts the host's own answer
/// and prints it rather than reporting the divergence as a failure. The
/// divergence is never hidden behind the flag - it is in the claim's name and in
/// the module comment.
fn check_divergence<T: std::fmt::Debug>(
    r: &mut Report,
    name: &str,
    got: Result<T, Errno>,
    want: Errno,
) {
    match got {
        Err(err) if err == want => r.check(name, true),
        other => {
            if mpos_user_tests::host_run() {
                println!("note: {name}: the host answered {other:?}");
                r.check(name, true);
            } else {
                r.check(&format!("{name} (got {other:?}, want {want})"), false);
            }
        }
    }
}

/// A claim about a byte count, spelled so a failure says what arrived.
fn check_len(r: &mut Report, name: &str, got: Result<usize, Errno>, want: usize) {
    match got {
        Ok(n) if n == want => r.check(name, true),
        Ok(n) => r.check(&format!("{name} (got {n}, want {want})"), false),
        Err(err) => r.check(&format!("{name} (got {err}, want {want})"), false),
    }
}

fn unix(ty: SocketType) -> Result<OwnedFd, Errno> {
    socket(AddressFamily::UNIX, ty, None)
}

/// A socket the claims after it are built on. If it does not open, they would be
/// about a descriptor that is not there: the program names it and stops, the way
/// `t-unix-rustix` does.
fn opens(r: &mut Report, name: &str, ty: SocketType) -> OwnedFd {
    match unix(ty) {
        Ok(fd) => {
            r.check(name, true);
            fd
        }
        Err(err) => {
            r.check(&format!("{name} (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    }
}

fn addr(path: &str) -> SocketAddrUnix {
    SocketAddrUnix::new(path).unwrap_or_else(|e| panic!("building {path}: {e}"))
}

/// Read one message and answer the bytes the kernel copied and who sent it. The
/// crate hands back both counts - what reached the buffer and what the datagram
/// held - and without `MSG_TRUNC` the two are the same, so the claim is made
/// against the first.
fn recv_from(
    fd: BorrowedFd<'_>,
    buf: &mut [u8],
    flags: RecvFlags,
) -> Result<(usize, Option<SocketAddrUnix>), Errno> {
    let (copied, _held, from) = recvfrom(fd, buf, flags)?;
    Ok((copied, from.and_then(|a| SocketAddrUnix::try_from(a).ok())))
}

/// `recvfrom` through the raw door, for the one thing `rustix` does not hand
/// back: the `addrlen` the kernel wrote. The length is in-out - Linux reads the
/// caller's capacity first - so the buffer's own size is handed in every time,
/// and the answer is the count.
fn recvfrom_addrlen(
    fd: BorrowedFd<'_>,
    buf: &mut [u8],
    src: &mut libc::sockaddr_un,
) -> (libc::c_long, usize) {
    let mut len: usize = core::mem::size_of::<libc::sockaddr_un>();
    let len_ptr: *mut usize = &raw mut len;
    // SAFETY: every argument is a live pointer to a buffer this frame owns, and
    // the only things the kernel writes through them are `len` and `src`.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_recvfrom,
            fd.as_raw_fd() as libc::c_long,
            buf.as_mut_ptr() as libc::c_long,
            buf.len() as libc::c_long,
            0 as libc::c_long,
            src as *mut libc::sockaddr_un as libc::c_long,
            len_ptr as libc::c_long,
        )
    };
    (rc, len)
}

/// An empty `sockaddr_un` to hand the raw door, with a sentinel path so a short
/// answer is visible.
fn empty_src() -> libc::sockaddr_un {
    libc::sockaddr_un {
        sun_family: 0,
        sun_path: [0x7f; 108],
    }
}

/// The name a `recvfrom` answered is the empty one, whether the crate spells it
/// as an empty path or as the nameless abstract address Linux sends.
fn is_empty_name(from: Option<&SocketAddrUnix>) -> bool {
    from.is_none_or(|a| a.path().is_none_or(|p| p.to_str() == Ok("")))
}

fn main() {
    println!("t-unix-dgram");
    let mut r = Report::new();

    for path in [RECV, SENDER, FULL, STREAM, SEQPACKET, SEQPACKET_TYPE, QUIET] {
        let _ = rustix::fs::unlink(path);
    }

    // The three shapes of the path family open. SEQPACKET is udevd's
    // control-socket shape: a *named* one listens where a datagram answers
    // EOPNOTSUPP, and the accepted pair it hands out carries message
    // boundaries (`t-unix-seqpacket`). The name is bound first
    // because that is the claim (Linux refuses `listen` on an unbound one) and
    // because the no-std version's listen-on-an-unbound-socket was a claim about
    // this kernel, not about Linux.
    let rs = opens(
        &mut r,
        "socket(AF_UNIX, SOCK_DGRAM) succeeds",
        SocketType::DGRAM,
    );
    match unix(SocketType::SEQPACKET) {
        Ok(sp) => {
            r.check("SOCK_SEQPACKET opens", true);
            check_ok(
                &mut r,
                "a SEQPACKET socket binds a name",
                bind(&sp, &addr(SEQPACKET)),
            );
            check_ok(
                &mut r,
                "a named SOCK_SEQPACKET socket listens",
                listen(&sp, 4),
            );
        }
        Err(err) => r.check(&format!("SOCK_SEQPACKET opens (got {err})"), false),
    }
    let cs = opens(&mut r, "a second datagram socket opens", SocketType::DGRAM);

    let raddr = addr(RECV);
    check_ok(&mut r, "bind plants the receiver's name", bind(&rs, &raddr));
    // `listen` is the stream call: a datagram socket has no accept queue to
    // open, and says so rather than pretending to build one.
    check_errno(
        &mut r,
        "listen on a datagram socket answers EOPNOTSUPP",
        listen(&rs, 4),
        Errno::OPNOTSUPP,
    );

    // A send with no destination anywhere: no name on the call, no peer pinned.
    // The write door answers `ENOTCONN`.
    check_errno(
        &mut r,
        "write to an unconnected datagram answers ENOTCONN",
        rustix::io::write(&cs, b"x"),
        Errno::NOTCONN,
    );

    // The first delivery: an unbound sender sends to the name, and the receiver
    // takes the whole message - with the sender's empty name back, because an
    // unbound sender has none to report.
    let mut buf = [0u8; 16];
    check_len(
        &mut r,
        "sendto by name delivers whole",
        sendto(&cs, b"hello", SendFlags::empty(), &raddr),
        5,
    );
    let (n, from) =
        recv_from(rs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check("recvfrom takes the message whole", n == 5);
    r.check("the bytes are the bytes", &buf[..5] == b"hello");
    r.check(
        "the unnamed sender answers with the empty name",
        is_empty_name(from.as_ref()),
    );

    // The same answer read for its *length*, on a second delivery: the count is
    // the raw door's answer, and the first message was spent on the parsed one.
    check_len(
        &mut r,
        "the unnamed sender's delivery is repeated",
        sendto(&cs, b"hello", SendFlags::empty(), &raddr),
        5,
    );
    let mut src = empty_src();
    let (rc, srclen) = recvfrom_addrlen(rs.as_fd(), &mut buf, &mut src);
    let unnamed_len = rc == 5 && srclen == 3 && src.sun_family as i32 == libc::AF_UNIX;
    if mpos_user_tests::host_run() {
        println!("note: the host answered rc={rc} addrlen={srclen} for an unbound sender");
    }
    r.check(
        "and three bytes of it were the answer (divergence: Linux reports no name at all)",
        unnamed_len || mpos_user_tests::host_run(),
    );

    // Truncation is per-message: the unread tail of one datagram is gone, and
    // the *next* message still stands whole behind it.
    check_len(
        &mut r,
        "the nine-byte send reports nine",
        sendto(&cs, b"123456789", SendFlags::empty(), &raddr),
        9,
    );
    check_len(
        &mut r,
        "the four-byte send reports four",
        sendto(&cs, b"next", SendFlags::empty(), &raddr),
        4,
    );
    let mut small = [0u8; 4];
    let (n, _) =
        recv_from(rs.as_fd(), &mut small, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check("the small read takes what fits", n == 4);
    r.check("and it is the message's front", &small == b"1234");
    let (n, _) =
        recv_from(rs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check("the next message arrives whole after a truncation", n == 4);
    r.check("the second message is intact", &buf[..4] == b"next");

    // A bound sender is named by what it bound: that is the reply address a
    // logger's reader could send back to.
    let saddr = addr(SENDER);
    check_ok(&mut r, "the sender binds its own name", bind(&cs, &saddr));
    check_len(
        &mut r,
        "the named send lands",
        sendto(&cs, b"ping", SendFlags::empty(), &raddr),
        4,
    );
    let (n, from) =
        recv_from(rs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check("recvfrom takes the ping", n == 4);
    r.check(
        "and the name is the one that bound",
        from.as_ref()
            .and_then(|a| a.path())
            .is_some_and(|p| p.to_str() == Ok(SENDER)),
    );
    // And the length that name occupies on the wire: the family word, the path,
    // and its NUL - the same on both machines.
    check_len(
        &mut r,
        "the named sender's delivery is repeated",
        sendto(&cs, b"ping", SendFlags::empty(), &raddr),
        4,
    );
    let (rc, srclen) = recvfrom_addrlen(rs.as_fd(), &mut buf, &mut src);
    r.check(
        "the answer carries the whole sender name",
        rc == 4 && srclen == 3 + SENDER.len(),
    );

    // The connected datagram: `connect` pins the destination and the byte doors
    // aim at it.
    let cs2 = opens(&mut r, "a connector socket opens", SocketType::DGRAM);
    check_ok(
        &mut r,
        "connect pins the receiver's name",
        connect(&cs2, &raddr),
    );
    check_divergence(
        &mut r,
        "a second connect answers EISCONN (divergence: Linux accepts a repeat connect)",
        connect(&cs2, &raddr),
        Errno::ISCONN,
    );
    check_len(
        &mut r,
        "write on a connected datagram lands whole",
        rustix::io::write(&cs2, b"via-peer"),
        8,
    );
    // A name given to a send on a connected datagram is honoured, not refused:
    // Linux resolves and validates it exactly as an unconnected send would, so
    // the peer `connect` pinned only decides where a send *without* a name goes.
    check_len(
        &mut r,
        "a named sendto on a connected sender lands",
        sendto(&cs2, b"named", SendFlags::empty(), &raddr),
        5,
    );
    let (n, from) =
        recv_from(rs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check("the connected write reads back", n == 8);
    r.check(
        "with the connected sender's empty name",
        &buf[..8] == b"via-peer" && is_empty_name(from.as_ref()),
    );
    let (n, _) =
        recv_from(rs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check(
        "and the named send arrives at that name",
        n == 5 && &buf[..5] == b"named",
    );
    // The name wins over the connected peer: this sender is bound at SENDER,
    // and a send naming that address lands in *its* queue, not the peer's.
    check_len(
        &mut r,
        "a named sendto on a connected sender reaches another socket",
        sendto(&cs2, b"elsewhere", SendFlags::empty(), &saddr),
        9,
    );
    let (n, _) =
        recv_from(cs.as_fd(), &mut buf, RecvFlags::empty()).unwrap_or_else(|e| panic!("{e}"));
    r.check(
        "the other socket holds the message",
        n == 9 && &buf[..9] == b"elsewhere",
    );
    let empty = recv_from(rs.as_fd(), &mut buf, RecvFlags::DONTWAIT);
    r.check(
        "and the connected peer's queue did not",
        matches!(empty, Err(Errno::AGAIN)),
    );
    // The name is resolved, so a path nobody holds is the VFS's answer.
    let nowhere = addr("/tmp/t-dgram-nowhere.sock");
    check_errno(
        &mut r,
        "a named sendto on a connected sender to a dead name answers ENOENT",
        sendto(&cs2, b"x", SendFlags::empty(), &nowhere),
        Errno::NOENT,
    );

    // A datagram aimed at a *path* naming a live socket of another type is a
    // type disagreement, not a dead name: Linux's path lookup finds the socket
    // by inode whatever its type and answers `EPROTOTYPE` (measured 91 for a
    // stream and for a sequenced-packet listener, send and connect alike).
    let sl = unix(SocketType::STREAM).expect("a stream socket");
    let staddr = addr(STREAM);
    check_ok(&mut r, "a stream socket binds a name", bind(&sl, &staddr));
    check_ok(&mut r, "and listens", listen(&sl, 4));
    let fresh = unix(SocketType::DGRAM).expect("a fresh datagram socket");
    check_errno(
        &mut r,
        "a datagram sent to a stream name answers EPROTOTYPE",
        sendto(&fresh, b"nope", SendFlags::empty(), &staddr),
        Errno::PROTOTYPE,
    );
    check_errno(
        &mut r,
        "and a connect to it answers the same",
        connect(&fresh, &staddr),
        Errno::PROTOTYPE,
    );

    // The other type a path may name. A sequenced-packet listener carries
    // messages, so "no queue to land in" would be the wrong reason for a
    // refusal - the type still disagrees, and Linux still says `EPROTOTYPE`.
    let spl = unix(SocketType::SEQPACKET).expect("a seqpacket socket");
    let spaddr = addr(SEQPACKET_TYPE);
    check_ok(
        &mut r,
        "a seqpacket socket binds a name",
        bind(&spl, &spaddr),
    );
    check_ok(&mut r, "and listens", listen(&spl, 4));
    check_errno(
        &mut r,
        "a datagram sent to a seqpacket name answers EPROTOTYPE",
        sendto(&fresh, b"nope", SendFlags::empty(), &spaddr),
        Errno::PROTOTYPE,
    );
    check_errno(
        &mut r,
        "and a connect to it answers the same",
        connect(&fresh, &spaddr),
        Errno::PROTOTYPE,
    );

    // An **abstract** name is not found that way: the name table is keyed by
    // type, so a stream listener's abstract name is simply absent for a
    // datagram lookup and the answer is the dead-name one (`ECONNREFUSED`).
    let al = unix(SocketType::STREAM).expect("an abstract stream socket");
    let aaddr = SocketAddrUnix::new_abstract_name(b"t-dgram-abs-stream")
        .expect("an abstract name fits its field");
    check_ok(&mut r, "an abstract stream socket binds", bind(&al, &aaddr));
    check_ok(&mut r, "and listens", listen(&al, 4));
    check_errno(
        &mut r,
        "a datagram sent to an abstract stream name answers ECONNREFUSED",
        sendto(&fresh, b"nope", SendFlags::empty(), &aaddr),
        Errno::CONNREFUSED,
    );
    check_errno(
        &mut r,
        "and a connect to it answers the same",
        connect(&fresh, &aaddr),
        Errno::CONNREFUSED,
    );
    drop(al);
    drop(spl);
    drop(sl);
    drop(fresh);

    // The queue has a bound, and the message past it is refused to the sender
    // rather than dropped. Draining one makes room for another - the refusal is
    // a full queue's answer, not the name's.
    let faddr = addr(FULL);
    let full_rs = unix(SocketType::DGRAM).expect("the cap receiver");
    let full_cs = unix(SocketType::DGRAM).expect("the cap sender");
    check_ok(&mut r, "the cap receiver binds", bind(&full_rs, &faddr));
    let mut accepted = 0usize;
    for _ in 0..QUEUE_DEPTH {
        if sendto(&full_cs, b"m", SendFlags::empty(), &faddr) == Ok(1) {
            accepted += 1;
        }
    }
    let over = sendto(&full_cs, b"m", SendFlags::empty(), &faddr);
    if mpos_user_tests::host_run() {
        println!(
            "note: this kernel's queue depth is {QUEUE_DEPTH}; the host bounds the queue by its \
             socket buffer and took {accepted} before answering {over:?}"
        );
    }
    r.check(
        "the whole queue takes its messages",
        accepted == QUEUE_DEPTH || mpos_user_tests::host_run(),
    );
    r.check(
        "the sixty-fifth answers EAGAIN to the sender",
        matches!(over, Err(Errno::AGAIN)) || mpos_user_tests::host_run(),
    );
    let (n, _) =
        recv_from(full_rs.as_fd(), &mut buf, RecvFlags::DONTWAIT).unwrap_or_else(|e| panic!("{e}"));
    r.check("a drained read makes room again", n == 1);
    check_len(
        &mut r,
        "and the next send lands",
        sendto(&full_cs, b"m", SendFlags::empty(), &faddr),
        1,
    );

    // An empty queue and a nonblocking reader is EAGAIN too - the rule that lets
    // a poll loop step over the socket. A receiver of its own, bound here and
    // never sent to, so a divergence above cannot leave a message behind for
    // the receiver the claim is about.
    let qaddr = addr(QUIET);
    let quiet = unix(SocketType::DGRAM).expect("a fresh receiver");
    check_ok(
        &mut r,
        "a fresh receiver binds a name",
        bind(&quiet, &qaddr),
    );
    check_errno(
        &mut r,
        "a DONTWAIT read of an empty queue answers EAGAIN",
        recv_from(quiet.as_fd(), &mut buf, RecvFlags::DONTWAIT),
        Errno::AGAIN,
    );

    // Cleanup: the names go, the descriptors go.
    for path in [RECV, SENDER, FULL, STREAM, SEQPACKET, SEQPACKET_TYPE, QUIET] {
        let _ = rustix::fs::unlink(path);
    }
    drop(rs);
    drop(cs);
    drop(cs2);
    drop(full_rs);
    drop(full_cs);
    drop(quiet);

    r.finish()
}
