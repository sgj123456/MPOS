//! t-unix-seqpacket - what a *named* `AF_UNIX` `SOCK_SEQPACKET` connection
//! owes its callers: message boundaries.
//!
//! The kernel used to serve a named seqpacket connection as the byte stream
//! `SOCK_STREAM` carries, so two sends could be read as one blob and a short
//! read silently merged the rest of a message into the next one. Measured
//! whole on a current host first (`.workbuddy/probe_unix_seqpacket{,2}.c`,
//! Linux 7.2.11, over abstract names):
//!
//! * **One send is one message.** 3 then 5 bytes read back as 3 then 5, a
//!   `sendmsg` chain of 4+4+2 is one message of 10, and a zero-length send
//!   is a zero-length message the peer reads as 0 (not EOF).
//! * **A short read discards the rest of that message.** Ten bytes through a
//!   four-byte buffer answers 4, leaves nothing queued, and the next read is
//!   `EAGAIN` - the truncation belongs to the message, not to a byte stream.
//! * **`MSG_TRUNC` names the message's own length.** A four-byte room over ten
//!   answers 10 with `MSG_TRUNC` in `msg_flags` and consumes it.
//! * **`MSG_PEEK` looks and leaves it.** The peek answers the message's length
//!   (10 with `MSG_TRUNC` through four bytes) and the read after it is the same
//!   ten bytes.
//! * **A zero-length room is not a zero-length read.** `read(fd, buf, 0)` is a
//!   no-op that answers 0 and keeps the message, while `recvfrom`/`recvmsg`
//!   with a zero-length room are receives: 0, `MSG_TRUNC` for `recvmsg`, and
//!   the message is consumed (with `MSG_PEEK`, left).
//! * **The two ends must agree on their type.** Through a *path* name a stream
//!   dialing a seqpacket listener and a seqpacket dialing a stream one both
//!   answer `EPROTOTYPE`, as Linux does (an abstract name would miss the
//!   lookup and answer `ECONNREFUSED`; that half is the name table's own rule).
//! * **`SCM_RIGHTS` rides the message.** A take installs the record's
//!   descriptors, a peek installs its own set and leaves the record, and a
//!   zero-length data room still receives the control chain.
//! * **A destination given to `sendto` is dropped.** Linux clears `msg_name`
//!   before the shared datagram send looks at it, so on a connected seqpacket
//!   socket the peer is the whole answer: a live name, a dead name and an
//!   address that is only the family word all send to the peer (measured on
//!   Linux 7.2.11). A connected *datagram* socket, by contrast, honours the
//!   name - that half is `t-unix-dgram`'s claim.

use std::io::IoSlice;
use std::mem::MaybeUninit;

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::io::{Errno, IoSliceMut};
use rustix::io::{read, write};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketType, accept, bind, connect, listen,
    recv, recvfrom, recvmsg, sendmsg, sendto, socket,
};
use rustix::pipe::pipe;

static LISTEN: &str = "/tmp/t-seqpacket.sock";
static STREAM_LISTEN: &str = "/tmp/t-seqpacket-stream.sock";

/// A claim about a byte count, spelled so a failure says what arrived.
fn check_len(r: &mut Report, name: &str, got: Result<usize, Errno>, want: usize) {
    match got {
        Ok(n) if n == want => r.check(name, true),
        Ok(n) => r.check(&format!("{name} (got {n}, want {want})"), false),
        Err(e) => r.check(&format!("{name} (got {e}, want {want})"), false),
    }
}

fn check_errno<T: std::fmt::Debug>(r: &mut Report, name: &str, got: Result<T, Errno>, want: Errno) {
    match got {
        Ok(ok) => r.check(&format!("{name} (got Ok({ok:?}), want {want})"), false),
        Err(e) if e == want => r.check(name, true),
        Err(e) => r.check(&format!("{name} (got {e}, want {want})"), false),
    }
}

fn addr(path: &str) -> SocketAddrUnix {
    SocketAddrUnix::new(path).unwrap_or_else(|e| panic!("building {path}: {e}"))
}

/// A bound and listening socket at `path`, or a fixture claim that ends the
/// run - nothing below is answerable without one.
fn listener(r: &mut Report, path: &str, typ: SocketType, what: &str) -> OwnedFd {
    match socket(AddressFamily::UNIX, typ, None) {
        Ok(fd) => {
            r.check(what, true);
            bind(&fd, &addr(path)).unwrap_or_else(|e| panic!("bind {path}: {e}"));
            listen(&fd, 4).unwrap_or_else(|e| panic!("listen {path}: {e}"));
            fd
        }
        Err(e) => {
            r.check(&format!("{what} ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    }
}

/// One connected pair over a named seqpacket listener: the client socket and
/// the accepted server socket.
fn pair(r: &mut Report, path: &str) -> (OwnedFd, OwnedFd) {
    let l = listener(
        r,
        path,
        SocketType::SEQPACKET,
        "a named seqpacket socket listens",
    );
    let client = match socket(AddressFamily::UNIX, SocketType::SEQPACKET, None) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("the client opens ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    };
    match connect(&client, &addr(path)) {
        Ok(()) => r.check("a seqpacket client connects to it", true),
        Err(e) => {
            r.check(&format!("a seqpacket client connects to it ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    }
    let server = match accept(&l) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("and the listener accepts it ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    };
    r.check("and the listener accepts it", true);
    (client, server)
}

/// One `recv` reduced to its return count: a `MSG_TRUNC` receive answers the
/// message's own length, which is the number the claims are about.
fn rcv(fd: &OwnedFd, buf: &mut [u8], flags: RecvFlags) -> Result<usize, Errno> {
    recv(fd, buf, flags).map(|(_, full)| full)
}

/// `sendto` with an address the safe wrapper cannot spell, for the two lengths
/// the connected send never interprets but the syscall layer still checks: a
/// `sockaddr_un` that is only the family word, and one longer than
/// `sockaddr_storage`.
fn sendto_raw(fd: &OwnedFd, data: &[u8], addr: *const u8, addrlen: usize) -> Result<isize, Errno> {
    // SAFETY: every argument is a live pointer to a buffer this frame owns.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sendto,
            fd.as_raw_fd() as libc::c_long,
            data.as_ptr() as libc::c_long,
            data.len() as libc::c_long,
            0 as libc::c_long,
            addr as libc::c_long,
            addrlen as libc::c_long,
        ) as isize
    };
    if rc < 0 {
        Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    } else {
        Ok(rc)
    }
}

/// A connected seqpacket send with an address that is only the family word -
/// a `sockaddr_un` whose body is empty, which every door that reads one refuses
/// with `EINVAL`.
fn sendto_family_only(fd: &OwnedFd, data: &[u8]) -> Result<isize, Errno> {
    let sa = libc::sockaddr_un {
        sun_family: libc::AF_UNIX as libc::sa_family_t,
        sun_path: [0; 108],
    };
    let len = core::mem::offset_of!(libc::sockaddr_un, sun_path);
    sendto_raw(fd, data, &sa as *const libc::sockaddr_un as *const u8, len)
}

fn main() {
    println!("t-unix-seqpacket");
    let mut r = Report::new();
    for path in [LISTEN, STREAM_LISTEN] {
        let _ = rustix::fs::unlink(path);
    }

    // ---------------------------------------------------------------------
    // One send is one message, and the chain a `sendmsg` hands over is one
    // message too.
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        check_len(
            &mut r,
            "the first send is three bytes",
            write(&c, b"abc"),
            3,
        );
        check_len(
            &mut r,
            "the second send is five bytes",
            write(&c, b"defgh"),
            5,
        );
        let mut big = [0u8; 64];
        let first = read(&s, &mut big);
        r.check(
            &format!("a read answers the first message alone (got {first:?})"),
            first == Ok(3) && &big[..3] == b"abc",
        );
        let second = read(&s, &mut big);
        r.check(
            &format!("and the next read answers the second (got {second:?})"),
            second == Ok(5) && &big[..5] == b"defgh",
        );

        let mut control = SendAncillaryBuffer::default();
        let chained = sendmsg(
            &c,
            &[
                std::io::IoSlice::new(b"abcd"),
                std::io::IoSlice::new(b"efgh"),
                std::io::IoSlice::new(b"ij"),
            ],
            &mut control,
            SendFlags::empty(),
        );
        check_len(&mut r, "a sendmsg chain of 4+4+2 is sent", chained, 10);
        let got = read(&s, &mut big);
        r.check(
            &format!("and reads back as one message of ten (got {got:?})"),
            got == Ok(10) && &big[..10] == b"abcdefghij",
        );
        check_errno(
            &mut r,
            "with nothing left behind it",
            rcv(&s, &mut big, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // The destination of a send on a connected seqpacket socket is dropped:
    // neither a live name, a dead name nor a family-only address is looked at,
    // and every one of them reaches the peer as one message.
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        check_len(
            &mut r,
            "a sendto naming the listener still lands at the peer",
            sendto(&c, b"live", SendFlags::empty(), &addr(LISTEN)),
            4,
        );
        let mut big = [0u8; 32];
        let first = read(&s, &mut big);
        r.check(
            &format!("and reads back whole (got {first:?})"),
            first == Ok(4) && &big[..4] == b"live",
        );
        // The name is not resolved at all: a path nobody holds is no reason to
        // refuse, where the same call on a *datagram* socket is `ENOENT`.
        let dead = addr("/tmp/t-seqpacket-nowhere.sock");
        check_len(
            &mut r,
            "a sendto naming a dead path still lands at the peer",
            sendto(&c, b"dead", SendFlags::empty(), &dead),
            4,
        );
        let second = read(&s, &mut big);
        r.check(
            &format!("and that one reads back whole too (got {second:?})"),
            second == Ok(4) && &big[..4] == b"dead",
        );
        // Nor is it validated: a `sockaddr_un` whose body is empty is `EINVAL`
        // for every door that reads it, but this send never reads it.
        let rc = sendto_family_only(&c, b"family");
        r.check(
            &format!("a family-only address sends (got {rc:?})"),
            rc == Ok(6),
        );
        let third = read(&s, &mut big);
        r.check(
            &format!("and that message arrives as one (got {third:?})"),
            third == Ok(6) && &big[..6] == b"family",
        );
        // Only the syscall layer's copy is checked: a length beyond
        // `sockaddr_storage` is refused before the socket can drop it.
        let mut long = [0u8; 129];
        long[0] = libc::AF_UNIX as u8;
        check_errno(
            &mut r,
            "a 129-byte address is EINVAL even though the name would be dropped",
            sendto_raw(&c, b"long", long.as_ptr(), long.len()),
            Errno::INVAL,
        );
        check_errno(
            &mut r,
            "with nothing left in the queue",
            rcv(&s, &mut big, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // A short read truncates *the message*: the rest is discarded, not merged
    // into the next read. `MSG_TRUNC` makes the return the message's length.
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        check_len(
            &mut r,
            "a ten-byte message is sent",
            write(&c, b"0123456789"),
            10,
        );
        let mut small = [0u8; 4];
        check_len(
            &mut r,
            "a four-byte read answers four",
            read(&s, &mut small),
            4,
        );
        check_errno(
            &mut r,
            "and the rest of that message is gone, not queued",
            rcv(&s, &mut small, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        check_len(
            &mut r,
            "a second ten-byte message is sent",
            write(&c, b"0123456789"),
            10,
        );
        let mut iov = [IoSliceMut::new(&mut small)];
        let mut control = RecvAncillaryBuffer::default();
        let got = recvmsg(&s, &mut iov, &mut control, RecvFlags::TRUNC);
        match got {
            Ok(msg) => {
                r.check(
                    &format!(
                        "MSG_TRUNC answers the message's own length through four bytes (got {})",
                        msg.bytes
                    ),
                    msg.bytes == 10,
                );
                r.check(
                    "and names the cut in msg_flags",
                    msg.flags.contains(rustix::net::ReturnFlags::TRUNC),
                );
            }
            Err(e) => {
                r.check(&format!("MSG_TRUNC answers ten (got {e})"), false);
                r.check("and names the cut in msg_flags", false);
            }
        }
        check_errno(
            &mut r,
            "while consuming that message",
            rcv(&s, &mut small, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // MSG_PEEK looks at the front and leaves it, boundaries included.
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        check_len(
            &mut r,
            "a message to look at is sent",
            write(&c, b"0123456789"),
            10,
        );
        let mut four = [0u8; 4];
        let mut iov = [IoSliceMut::new(&mut four)];
        let mut control = RecvAncillaryBuffer::default();
        let peeked = recvmsg(
            &s,
            &mut iov,
            &mut control,
            RecvFlags::PEEK | RecvFlags::TRUNC,
        );
        r.check(
            &format!("a peek with MSG_TRUNC answers the message's length (got {peeked:?})"),
            peeked.as_ref().is_ok_and(|m| m.bytes == 10),
        );
        r.check(
            "and reports the cut",
            peeked
                .as_ref()
                .is_ok_and(|m| m.flags.contains(rustix::net::ReturnFlags::TRUNC)),
        );
        let mut big = [0u8; 16];
        let took = read(&s, &mut big);
        r.check(
            &format!("the read after the peek is the same ten bytes (got {took:?})"),
            took == Ok(10) && &big[..10] == b"0123456789",
        );
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // Zero-length messages and zero-length rooms are different things:
    // a zero-length *send* is a message, a zero-length *read* is a no-op,
    // and a zero-length recvfrom/recvmsg is a receive that consumes.
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        check_len(&mut r, "a zero-length send is accepted", write(&c, b""), 0);
        let mut buf = [0u8; 8];
        check_len(
            &mut r,
            "and reads back as a zero-length message",
            read(&s, &mut buf),
            0,
        );
        check_errno(
            &mut r,
            "with nothing else queued behind it",
            rcv(&s, &mut buf, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );

        check_len(
            &mut r,
            "a real message follows",
            write(&c, b"0123456789"),
            10,
        );
        let mut empty: [u8; 0] = [];
        check_len(
            &mut r,
            "a zero-length read answers zero",
            read(&s, &mut empty),
            0,
        );
        check_len(
            &mut r,
            "and does not consume the message",
            read(&s, &mut buf),
            8,
        );
        check_errno(
            &mut r,
            "the four bytes it could not hold are gone",
            rcv(&s, &mut buf, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );

        check_len(
            &mut r,
            "another message is sent",
            write(&c, b"0123456789"),
            10,
        );
        let mut empty: [u8; 0] = [];
        check_len(
            &mut r,
            "a zero-length recvfrom answers zero",
            recvfrom(&s, &mut empty, RecvFlags::empty()).map(|(copied, _held, _from)| copied),
            0,
        );
        check_errno(
            &mut r,
            "and consumes the message",
            rcv(&s, &mut buf, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // `SCM_RIGHTS` rides the message: a take installs the record's
    // descriptor, a peek installs its own set and leaves the record for the
    // take, and a zero-length data room still receives the control data.
    // Measured on a current host (`.workbuddy/probe_unix_seqpacket3.c`).
    // ---------------------------------------------------------------------
    {
        let (c, s) = pair(&mut r, LISTEN);
        let (reader, writer) = pipe().expect("a pipe for the handover");
        write(&writer, b"q").expect("a byte in the pipe");
        let fds = [reader.as_fd()];
        let mut space = [MaybeUninit::<u8>::uninit(); 64];
        let mut control = SendAncillaryBuffer::new(&mut space);
        r.check(
            "the control buffer took the descriptor",
            control.push(SendAncillaryMessage::ScmRights(&fds)),
        );
        check_len(
            &mut r,
            "one byte with a descriptor rides one sendmsg",
            sendmsg(&c, &[IoSlice::new(b"x")], &mut control, SendFlags::empty()),
            1,
        );

        let mut one = [0u8; 8];
        let (peeked, mut peek_fds): (Result<rustix::net::RecvMsg, Errno>, Vec<OwnedFd>) = {
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iovs = [IoSliceMut::new(&mut one)];
            let got = recvmsg(&s, &mut iovs, &mut control, RecvFlags::PEEK);
            let mut fds = Vec::new();
            for msg in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fd) = msg {
                    fds.extend(fd);
                }
            }
            (got, fds)
        };
        r.check(
            "the peek answers the message's one byte",
            peeked.as_ref().is_ok_and(|m| m.bytes == 1),
        );
        r.check(
            "and installs the descriptor the send carried",
            peek_fds.len() == 1,
        );

        let mut one = [0u8; 8];
        let (took, take_fds): (Result<rustix::net::RecvMsg, Errno>, Vec<OwnedFd>) = {
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iovs = [IoSliceMut::new(&mut one)];
            let got = recvmsg(&s, &mut iovs, &mut control, RecvFlags::empty());
            let mut fds = Vec::new();
            for msg in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fd) = msg {
                    fds.extend(fd);
                }
            }
            (got, fds)
        };
        r.check(
            "the take after the peek is the same byte",
            took.as_ref().is_ok_and(|m| m.bytes == 1),
        );
        r.check(
            "and installs a second, distinct descriptor",
            take_fds.len() == 1 && peek_fds.len() == 1,
        );
        if let Some(fd) = take_fds.first() {
            let mut b = [0u8; 1];
            r.check(
                "the installed descriptor is live",
                read(fd, &mut b) == Ok(1) && b[0] == b'q',
            );
        } else {
            r.check("the installed descriptor is live", false);
        }

        // A zero-length data room is still a receive for the control chain:
        // Linux answers 0, installs the descriptor and consumes the message.
        write(&writer, b"r").expect("a second byte in the pipe");
        let mut control = SendAncillaryBuffer::new(&mut space);
        r.check(
            "a second control buffer took the descriptor",
            control.push(SendAncillaryMessage::ScmRights(&fds)),
        );
        check_len(
            &mut r,
            "a zero-length sendmsg still carries it",
            sendmsg(&c, &[], &mut control, SendFlags::empty()),
            0,
        );
        let mut empty: [u8; 0] = [];
        let (got, got_fds): (Result<rustix::net::RecvMsg, Errno>, Vec<OwnedFd>) = {
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iovs = [IoSliceMut::new(&mut empty)];
            let got = recvmsg(&s, &mut iovs, &mut control, RecvFlags::empty());
            let mut fds = Vec::new();
            for msg in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fd) = msg {
                    fds.extend(fd);
                }
            }
            (got, fds)
        };
        r.check(
            "a zero-length room answers zero with the descriptor",
            got.as_ref().is_ok_and(|m| m.bytes == 0) && got_fds.len() == 1,
        );
        check_errno(
            &mut r,
            "and the message is consumed",
            rcv(&s, &mut one, RecvFlags::DONTWAIT),
            Errno::AGAIN,
        );
        drop((peek_fds, take_fds, got_fds));
        let _ = rustix::fs::unlink(LISTEN);
    }

    // ---------------------------------------------------------------------
    // The type disagreement: both directions are EPROTOTYPE.
    // ---------------------------------------------------------------------
    {
        let l = listener(
            &mut r,
            LISTEN,
            SocketType::SEQPACKET,
            "a seqpacket listener for the mismatch",
        );
        let stream = socket(AddressFamily::UNIX, SocketType::STREAM, None).expect("stream socket");
        check_errno(
            &mut r,
            "a stream dialing a seqpacket listener is EPROTOTYPE",
            connect(&stream, &addr(LISTEN)),
            Errno::PROTOTYPE,
        );
        drop(stream);
        drop(l);
        let _ = rustix::fs::unlink(LISTEN);

        let l = listener(
            &mut r,
            STREAM_LISTEN,
            SocketType::STREAM,
            "a stream listener for the mismatch",
        );
        let sp =
            socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).expect("seqpacket socket");
        check_errno(
            &mut r,
            "a seqpacket dialing a stream listener is EPROTOTYPE",
            connect(&sp, &addr(STREAM_LISTEN)),
            Errno::PROTOTYPE,
        );
        drop(sp);
        drop(l);
        let _ = rustix::fs::unlink(STREAM_LISTEN);
    }

    r.finish()
}
