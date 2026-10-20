//! t-msg-peek - what `MSG_PEEK` and `MSG_TRUNC` owe a caller on `AF_INET`.
//!
//! Both flags were refused on this path with `EOPNOTSUPP` (the honest answer
//! while peek-preserving delivery did not exist), and `t-sock-flags` pinned
//! that refusal. The contract below is measured on a current host first
//! (`.workbuddy/probe_msg_peek{,2,3,4}.c`, Linux 7.2.x), one claim per answer:
//!
//! * a **datagram** peek does not consume. A short buffer copies what fitted
//!   and leaves the whole message queued - the next full peek still reads
//!   every byte - and `MSG_TRUNC` answers the datagram's **own** length
//!   instead of the copy's. A zero-length peek answers 0 and takes nothing;
//!   with `MSG_TRUNC` it answers the datagram's length.
//! * a **stream** peek does not consume either, and a short buffer is just a
//!   short copy. `MSG_TRUNC` on a stream is *not* the datagram's reading: it
//!   answers the count the call would have copied and copies **nothing** -
//!   measured, 3 of 10 bytes answers 3, 100 of 10 answers 10, and the caller's
//!   buffer keeps every byte it came with. A non-peek stream receive with
//!   `MSG_TRUNC` consumes the bytes it counted and copies none of them.
//! * a zero-length **stream** receive is Linux's readiness question with a
//!   zero-byte answer: with data queued it answers 0 at once and takes
//!   nothing; with nothing queued it parks, and a signal that ends the park
//!   still answers 0 rather than `EINTR`. A zero-length **datagram** receive
//!   is the opposite in one respect: it consumes the datagram (without
//!   `MSG_PEEK`), which the queue's own answer shows.
//! * an empty queue answers `EAGAIN` under `MSG_DONTWAIT` and parks without it,
//!   and a signal ends *that* park with `EINTR` when the call has bytes to
//!   fill (the stream's zero-length shape above is the exception).
//! * `recvmsg` reports a datagram the buffer cut with `MSG_TRUNC` in
//!   `msg_flags` even when the caller did not ask for the flag, and says
//!   nothing for a stream - and it still names the sender in `msg_name` on a
//!   peek.
//!
//! The waits are made interruptible by a `SIGALRM` whose handler does not
//! restart the call, so a kernel that parks where it should not is a failing
//! claim rather than a hung sweep.
//!
//! One family is deliberately left out: a **raw** socket's peek, because the
//! host this contract was measured on has no `CAP_NET_RAW` and the probe could
//! not answer for it. That refusal is named in `docs/uapi/` §5.3 rather than
//! guessed here.

use std::io::IoSliceMut;
use std::net::Ipv4Addr;

use mpos_user_tests::Report;
use rustix::fd::{AsFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, ReturnFlags, SendFlags, SocketAddrV4,
    SocketType, accept, bind, connect, getsockname, listen, recvfrom, recvmsg, send, socket,
};

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// What one `recvfrom` answered: the bytes copied, the syscall's own return
/// (which `MSG_TRUNC` makes larger than the copy), the sender when there is
/// one, and the errno when it refused.
struct Datagram {
    copied: usize,
    answer: usize,
    from: Option<SocketAddrV4>,
    err: Option<i32>,
}

impl Datagram {
    fn refused(&self, errno: Errno) -> bool {
        self.err == Some(errno.raw_os_error())
    }
}

fn receive(fd: &impl AsFd, buf: &mut [u8], flags: RecvFlags) -> Datagram {
    match recvfrom(fd, buf, flags) {
        Ok((copied, answer, from)) => Datagram {
            copied,
            answer,
            from: from.and_then(|a| SocketAddrV4::try_from(a).ok()),
            err: None,
        },
        Err(e) => Datagram {
            copied: 0,
            answer: 0,
            from: None,
            err: Some(e.raw_os_error()),
        },
    }
}

/// `recvmsg`'s own answer: the syscall's count (which is the pre-truncation
/// length when `MSG_TRUNC` is set), the returned `msg_flags` word, and the
/// sender it named.
struct Message {
    answer: isize,
    flags: ReturnFlags,
    from_port: Option<u16>,
    err: Option<i32>,
}

fn receive_msg(fd: &impl AsFd, buf: &mut [u8], flags: RecvFlags) -> Message {
    let mut iov = [IoSliceMut::new(buf)];
    let mut control = RecvAncillaryBuffer::default();
    match recvmsg(fd, &mut iov, &mut control, flags) {
        Ok(m) => Message {
            answer: m.bytes as isize,
            flags: m.flags,
            from_port: m
                .address
                .and_then(|a| SocketAddrV4::try_from(a).ok())
                .map(|a| a.port()),
            err: None,
        },
        Err(e) => Message {
            answer: -1,
            flags: ReturnFlags::empty(),
            from_port: None,
            err: Some(e.raw_os_error()),
        },
    }
}

/// A UDP pair on the loopback: `server` is bound (so it can be peeked at
/// before anything arrives), `client` is connected to it.
fn udp_pair() -> (OwnedFd, OwnedFd, SocketAddrV4) {
    let server = socket(AddressFamily::INET, SocketType::DGRAM, None).expect("udp server");
    bind(&server, &SocketAddrV4::new(LOOPBACK, 0)).expect("bind");
    let named = SocketAddrV4::try_from(getsockname(&server).expect("getsockname")).expect("v4");
    let client = socket(AddressFamily::INET, SocketType::DGRAM, None).expect("udp client");
    connect(&client, &named).expect("connect");
    (server, client, named)
}

/// A TCP pair on the loopback, with the listener kept so the last section can
/// ask what a listener's receive and peek answer.
fn tcp_pair() -> (OwnedFd, OwnedFd, OwnedFd) {
    let server = socket(AddressFamily::INET, SocketType::STREAM, None).expect("tcp server");
    bind(&server, &SocketAddrV4::new(LOOPBACK, 0)).expect("bind");
    let named = SocketAddrV4::try_from(getsockname(&server).expect("getsockname")).expect("v4");
    listen(&server, 1).expect("listen");
    let client = socket(AddressFamily::INET, SocketType::STREAM, None).expect("tcp client");
    connect(&client, &named).expect("connect");
    let accepted = accept(&server).expect("accept");
    (server, accepted, client)
}

/// One catchable signal, installed without `SA_RESTART` so a parked call is
/// ended rather than restarted - the shape every "this wait can be
/// interrupted" claim in the suite uses.
extern "C" fn wake_me(_: libc::c_int) {}

fn interrupt_after(seconds: u32) {
    unsafe {
        let mut act: libc::sigaction = core::mem::zeroed();
        act.sa_sigaction = wake_me as *const () as usize;
        act.sa_flags = 0;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGALRM, &act, core::ptr::null_mut());
        libc::alarm(seconds);
    }
}

fn cancel_interrupt() {
    unsafe { libc::alarm(0) };
}

fn main() {
    println!("t-msg-peek");
    let mut r = Report::new();

    // ------------------------------------------------------------------ UDP
    let (server, client, _named) = udp_pair();
    r.check(
        "a 6-byte datagram is sent",
        send(&client, b"abcdef", SendFlags::empty()).is_ok_and(|n| n == 6),
    );
    // The sender's name, read once the send has given the socket one - it is
    // the port both `recvfrom` and `recvmsg` have to name on a peek.
    let client_port = SocketAddrV4::try_from(getsockname(&client).expect("client name"))
        .expect("v4")
        .port();
    // The stack advances on a poll, so the queue is waited for before it is
    // asked anything: a peek with no wait would measure the sender's timing.
    let mut buf = [0u8; 16];
    let mut got = None;
    for _ in 0..100 {
        let answer = receive(&server, &mut buf, RecvFlags::PEEK | RecvFlags::DONTWAIT);
        if answer.err.is_none() {
            got = Some(answer);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let answer = got.expect("the datagram arrives before any peek claim");
    r.check(
        "peek 16 answers the whole datagram",
        answer.answer == 6 && answer.copied == 6 && &buf[..6] == b"abcdef",
    );
    // A passing line stays diffable: the ports differ on every run and on every
    // machine, so they ride in the label only when the claim fails (`t-proc-modes`
    // spells a value that is not a constant the same way).
    let named = answer.from.map(|a| a.port()) == Some(client_port);
    r.check(
        if named {
            "and names the sender on the peek".to_string()
        } else {
            format!(
                "and names the sender on the peek (got {:?}, want {client_port})",
                answer.from
            )
        }
        .as_str(),
        named,
    );
    r.check(
        "and the queue still holds it",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 6),
    );

    let mut short = [0u8; 16];
    let a = receive(
        &server,
        &mut short[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "peek 3 answers 3 and the first three bytes",
        a.answer == 3 && &short[..3] == b"abc",
    );
    let a = receive(
        &server,
        &mut short[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "a second three-byte peek answers the same three",
        a.answer == 3 && &short[..3] == b"abc",
    );
    let mut full = [0u8; 16];
    let a = receive(&server, &mut full, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    r.check(
        "so a full peek after it reads all six bytes",
        a.answer == 6 && &full[..6] == b"abcdef",
    );
    let a = receive(
        &server,
        &mut short[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "peek 3 with MSG_TRUNC answers the datagram's own length (6)",
        a.answer == 6 && a.copied == 3 && &short[..3] == b"abc",
    );
    let a = receive(
        &server,
        &mut short[..0],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "a zero-length peek answers 0",
        a.err.is_none() && a.answer == 0,
    );
    let a = receive(
        &server,
        &mut short[..0],
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "and with MSG_TRUNC it answers the six bytes it did not copy",
        a.err.is_none() && a.answer == 6,
    );
    let a = receive(&server, &mut full, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    r.check(
        "every one of those peeks left the datagram whole",
        a.answer == 6 && &full[..6] == b"abcdef",
    );

    let a = receive(&server, &mut short[..3], RecvFlags::DONTWAIT);
    r.check(
        "a three-byte recv (no peek) answers 3",
        a.err.is_none() && a.answer == 3 && &short[..3] == b"abc",
    );
    r.check(
        "and consumes the whole datagram, not three bytes of it",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 0),
    );
    let a = receive(&server, &mut full, RecvFlags::PEEK | RecvFlags::DONTWAIT);
    r.check(
        "a peek on the empty queue with MSG_DONTWAIT is EAGAIN",
        a.refused(Errno::AGAIN),
    );
    interrupt_after(1);
    let a = receive(&server, &mut full, RecvFlags::PEEK);
    cancel_interrupt();
    r.check(
        "a blocking peek on the empty queue is ended by a signal (EINTR)",
        a.refused(Errno::INTR),
    );

    r.check(
        "the datagram is sent again",
        send(&client, b"abcdef", SendFlags::empty()).is_ok_and(|n| n == 6),
    );
    let mut queue = None;
    for _ in 0..100 {
        let a = receive(
            &server,
            &mut short[..3],
            RecvFlags::DONTWAIT | RecvFlags::TRUNC,
        );
        if a.err.is_none() {
            queue = Some(a);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let a = queue.expect("the second datagram arrives");
    r.check(
        "a three-byte recv with MSG_TRUNC answers 6 and copies 3",
        a.answer == 6 && &short[..3] == b"abc",
    );
    r.check(
        "and that receive consumed the datagram",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 0),
    );

    // `recvmsg`: the same read, plus the two things only it can say.
    r.check(
        "the datagram is sent a third time",
        send(&client, b"abcdef", SendFlags::empty()).is_ok_and(|n| n == 6),
    );
    let mut msg_buf = [0u8; 16];
    let mut m = None;
    for _ in 0..100 {
        let answer = receive_msg(
            &server,
            &mut msg_buf[..3],
            RecvFlags::PEEK | RecvFlags::DONTWAIT,
        );
        if answer.err.is_none() {
            m = Some(answer);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let m = m.expect("the third datagram arrives");
    r.check(
        "recvmsg peeks three bytes and answers 3",
        m.answer == 3 && &msg_buf[..3] == b"abc",
    );
    r.check(
        "and its msg_flags carry MSG_TRUNC, which recvfrom cannot report",
        m.flags.contains(ReturnFlags::TRUNC),
    );
    let named = m.from_port == Some(client_port);
    r.check(
        if named {
            "and it names the sender on the peek".to_string()
        } else {
            format!(
                "and it names the sender on the peek (got {:?}, want {client_port})",
                m.from_port
            )
        }
        .as_str(),
        named,
    );
    r.check(
        "the peek left the datagram queued",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 6),
    );
    let m = receive_msg(
        &server,
        &mut msg_buf[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "recvmsg peek with MSG_TRUNC answers the datagram's own length (6)",
        m.answer == 6 && m.flags.contains(ReturnFlags::TRUNC) && &msg_buf[..3] == b"abc",
    );
    r.check(
        "and the datagram is still queued",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 6),
    );
    let m = receive_msg(&server, &mut msg_buf[..3], RecvFlags::DONTWAIT);
    r.check(
        "a three-byte recvmsg (no peek) answers 3",
        m.answer == 3 && &msg_buf[..3] == b"abc",
    );
    r.check(
        "and consumed it, with msg_flags still naming the cut",
        rustix::io::ioctl_fionread(&server).is_ok_and(|n| n == 0)
            && m.flags.contains(ReturnFlags::TRUNC),
    );
    drop((server, client));

    // ------------------------------------------------------------------ TCP
    let (listener, server, client) = tcp_pair();
    r.check(
        "a 10-byte stream is sent",
        send(&client, b"0123456789", SendFlags::empty()).is_ok_and(|n| n == 10),
    );
    let mut buf = [0u8; 32];
    let mut first = None;
    for _ in 0..100 {
        let a = receive(
            &server,
            &mut buf[..3],
            RecvFlags::PEEK | RecvFlags::DONTWAIT,
        );
        if a.err.is_none() {
            first = Some(a);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let a = first.expect("the stream data arrives");
    r.check(
        "a stream peek answers the three bytes asked for",
        a.answer == 3 && &buf[..3] == b"012",
    );
    let a = receive(
        &server,
        &mut buf[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "and peeking again answers the same three",
        a.answer == 3 && &buf[..3] == b"012",
    );
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "a wide peek reads what is queued (10), not more",
        a.answer == 10 && &buf[..10] == b"0123456789",
    );

    // The stream's `MSG_TRUNC` reading, and the one a four-byte-early copy
    // would pass by accident: the buffer is seeded and must stay seeded.
    let mut seeded = [0xAAu8; 32];
    let a = receive(
        &server,
        &mut seeded[..3],
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "a stream peek with MSG_TRUNC answers 3 and copies nothing",
        a.answer == 3 && seeded == [0xAAu8; 32],
    );
    let a = receive(
        &server,
        &mut seeded,
        RecvFlags::PEEK | RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "and a wide one answers 10 and copies nothing",
        a.answer == 10 && seeded == [0xAAu8; 32],
    );
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "neither MSG_TRUNC peek consumed or cut the stream",
        a.answer == 10 && &buf[..10] == b"0123456789",
    );

    let mut seeded = [0xAAu8; 32];
    let a = receive(
        &server,
        &mut seeded[..3],
        RecvFlags::DONTWAIT | RecvFlags::TRUNC,
    );
    r.check(
        "a stream recv with MSG_TRUNC answers 3 and copies nothing",
        a.answer == 3 && seeded == [0xAAu8; 32],
    );
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "but it consumed the three it counted (7 left)",
        a.answer == 7 && &buf[..7] == b"3456789",
    );

    let a = receive(
        &server,
        &mut buf[..0],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "a zero-length stream peek answers 0",
        a.err.is_none() && a.answer == 0,
    );
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "and takes nothing (the seven bytes are still there)",
        a.answer == 7 && &buf[..7] == b"3456789",
    );
    let a = receive(&server, &mut buf[..0], RecvFlags::DONTWAIT);
    r.check(
        "a zero-length stream recv answers 0 too",
        a.err.is_none() && a.answer == 0,
    );
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "and it does not consume either",
        a.answer == 7 && &buf[..7] == b"3456789",
    );

    let _ = receive(&server, &mut buf[..32], RecvFlags::DONTWAIT);
    let a = receive(
        &server,
        &mut buf[..32],
        RecvFlags::PEEK | RecvFlags::DONTWAIT,
    );
    r.check(
        "a peek on an empty stream with MSG_DONTWAIT is EAGAIN",
        a.refused(Errno::AGAIN),
    );
    interrupt_after(1);
    let a = receive(&server, &mut buf[..0], RecvFlags::PEEK);
    cancel_interrupt();
    r.check(
        "a blocking zero-length stream peek is ended by a signal, and answers 0",
        a.err.is_none() && a.answer == 0,
    );

    // A stream's msg_flags stay clear: there is no message boundary to cut.
    r.check(
        "five more bytes are sent",
        send(&client, b"abcde", SendFlags::empty()).is_ok_and(|n| n == 5),
    );
    let mut msg_buf = [0u8; 16];
    let mut m = None;
    for _ in 0..100 {
        let answer = receive_msg(
            &server,
            &mut msg_buf[..3],
            RecvFlags::PEEK | RecvFlags::DONTWAIT,
        );
        if answer.err.is_none() {
            m = Some(answer);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let m = m.expect("the stream data arrives");
    r.check(
        "recvmsg peeks a stream without naming a sender or a truncation",
        m.answer == 3 && !m.flags.contains(ReturnFlags::TRUNC) && m.from_port.is_none(),
    );
    let _ = receive(&server, &mut msg_buf, RecvFlags::DONTWAIT);

    // EOF: the peer closes, and a peek answers end of stream.
    drop(client);
    let mut eof = None;
    for _ in 0..100 {
        let a = receive(
            &server,
            &mut buf[..4],
            RecvFlags::PEEK | RecvFlags::DONTWAIT,
        );
        if a.err.is_none() && a.answer == 0 {
            eof = Some(a);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    r.check(
        "a peek at a closed stream answers 0 (end of stream)",
        eof.is_some_and(|a| a.err.is_none() && a.answer == 0),
    );

    // A listener has no data of its own, and both doors say so.
    let a = receive(&listener, &mut buf[..8], RecvFlags::empty());
    r.check(
        "a listener's recvfrom is ENOTCONN",
        a.refused(Errno::NOTCONN),
    );
    let a = receive(&listener, &mut buf[..8], RecvFlags::PEEK);
    r.check("and so is its peek", a.refused(Errno::NOTCONN));

    r.finish();
}
