//! t-unix-peek - what the AF_UNIX receive doors owe `MSG_PEEK` and `MSG_TRUNC`.
//!
//! Both flags used to be refused here (`EOPNOTSUPP`), and both answers are
//! Linux's rather than a shape this kernel found convenient. Measured whole on
//! a current host first (`.workbuddy/probe_unix_peek{,2,3,4,5,6}.c`, Linux
//! 7.2.x):
//!
//! * **A stream peek copies without taking.** A 4-byte look at ten queued bytes
//!   answers 4, fills the caller's buffer and leaves all ten for the next read;
//!   the look is repeatable, a zero-length look answers 0 without taking a
//!   byte, and a look at end-of-stream answers 0.
//! * **A stream's `MSG_TRUNC` is not a datagram's.** It changes nothing: the
//!   read copies what it would have copied, answers that count and consumes
//!   it. (On this kernel's TCP path the same flag is the count-only rule; the
//!   families differ, which is why this file pins the unix one.)
//! * **A datagram peek is a full copy of the queue's front.** A short look
//!   answers 4 and leaves the message whole; with `MSG_TRUNC` the *return* is
//!   the message's own length (10) and the copy stays 4; `recvmsg` sets
//!   `MSG_TRUNC` in `msg_flags` whether or not the caller asked. A
//!   zero-length peek answers 0 without consuming, while a zero-length
//!   non-peek receive **does** consume.
//! * **A consumed `SCM_RIGHTS` record is delivered again after a peek.**
//!   Linux installs the descriptors a peeked send carried and leaves the
//!   record queued, so the following read installs a second set - both live.
//!
//! Everything here is `rustix`'s except the sender-name claim, whose
//! `struct sockaddr_un` decode the crate does not offer - so that section is
//! `libc`, and says so.

use std::io::IoSlice;
use std::mem::MaybeUninit;

use mpos_user_tests::Report;
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::{Errno, IoSliceMut};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, RecvMsg,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recv, recvmsg,
    send, sendmsg, socket, socketpair,
};

const AGAIN: Errno = Errno::AGAIN;

/// One socket pair of the type under test, or a fixture claim that ends the
/// run: nothing below is answerable without it.
fn pair(r: &mut Report, typ: SocketType, what: &str) -> (OwnedFd, OwnedFd) {
    match socketpair(AddressFamily::UNIX, typ, SocketFlags::empty(), None) {
        Ok((a, b)) => {
            r.check(what, true);
            (a, b)
        }
        Err(e) => {
            r.check(&format!("{what} ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    }
}

/// One unconnected socket of the type under test. The sender-name section
/// needs this rather than [`pair`]: a `socketpair` end is connected, and this
/// kernel honestly refuses `sendto` with a destination on a connected
/// datagram (`EISCONN`, a divergence `t-unix-dgram` already pins) where Linux
/// takes the address. A connected pair would test that refusal instead of the
/// name a peek reports.
fn unconnected(r: &mut Report, typ: SocketType, what: &str) -> OwnedFd {
    match socket(AddressFamily::UNIX, typ, None) {
        Ok(fd) => {
            r.check(what, true);
            fd
        }
        Err(e) => {
            r.check(&format!("{what} ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    }
}

/// One `recv`, reduced to the count it answered: `rustix`'s wrapper hands back
/// the return-flags word beside it, and every claim here is about the count.
fn rcv(fd: &OwnedFd, buf: &mut [u8], flags: RecvFlags) -> Result<usize, Errno> {
    // The second half is the kernel's own return value, which a `MSG_TRUNC`
    // datagram receive makes larger than the copy; the first is the copy.
    recv(fd, buf, flags).map(|(_, full)| full)
}

/// One receive, reported as the number of bytes it answered.
fn check_len(r: &mut Report, name: &str, got: Result<usize, Errno>, want: usize) {
    match got {
        Ok(n) if n == want => r.check(name, true),
        Ok(n) => r.check(&format!("{name} (got {n}, want {want})"), false),
        Err(e) => r.check(&format!("{name} ({e})"), false),
    }
}

/// One receive expected to refuse, reported with the errno it refused with.
fn check_errno(r: &mut Report, name: &str, got: Result<usize, Errno>, want: Errno) {
    match got {
        Err(e) if e == want => r.check(name, true),
        Err(e) => r.check(&format!("{name} (got {e}, want {want})"), false),
        Ok(n) => r.check(&format!("{name} (got {n} bytes, want {want})"), false),
    }
}

/// One receive through `recvmsg`, answering its result and the descriptors the
/// control chain carried.
fn recv_with_fds(
    fd: &OwnedFd,
    body: &mut [u8],
    space: &mut [MaybeUninit<u8>],
    flags: RecvFlags,
) -> (Result<RecvMsg, Errno>, Vec<OwnedFd>) {
    let mut control = RecvAncillaryBuffer::new(space);
    let mut iovs = [IoSliceMut::new(body)];
    let got = recvmsg(fd, &mut iovs, &mut control, flags);
    let mut fds = Vec::new();
    for msg in control.drain() {
        if let RecvAncillaryMessage::ScmRights(got) = msg {
            fds.extend(got);
        }
    }
    (got, fds)
}

fn main() {
    println!("t-unix-peek");
    let mut r = Report::new();

    // ---------------------------------------------------------------------
    // A stream: the look does not take, and `MSG_TRUNC` changes nothing.
    // ---------------------------------------------------------------------
    let (a, b) = pair(
        &mut r,
        SocketType::STREAM,
        "an AF_UNIX SOCK_STREAM pair opens",
    );
    check_len(
        &mut r,
        "a ten-byte stream send answers 10",
        send(&a, b"abcdefghij", SendFlags::empty()),
        10,
    );
    let mut buf = [0u8; 10];
    check_len(
        &mut r,
        "a 4-byte stream peek of 10 answers 4",
        rcv(&b, &mut buf[..4], RecvFlags::PEEK),
        4,
    );
    r.check("the peek copied the front 'abcd'", &buf[..4] == b"abcd");
    check_len(
        &mut r,
        "the peek left all ten for the read",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );
    r.check("the read returned the whole stream", &buf == b"abcdefghij");

    check_len(
        &mut r,
        "the second stream send answers 10",
        send(&a, b"0123456789", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a zero-length stream peek answers 0",
        rcv(&b, &mut [], RecvFlags::PEEK),
        0,
    );
    check_len(
        &mut r,
        "the zero-length peek took nothing",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    check_len(
        &mut r,
        "a third stream send answers 10",
        send(&a, b"ABCDEFGHIJ", SendFlags::empty()),
        10,
    );
    let mut trunc = [0xAAu8; 4];
    check_len(
        &mut r,
        "a stream peek|TRUNC answers the 4 it would copy",
        rcv(&b, &mut trunc, RecvFlags::PEEK | RecvFlags::TRUNC),
        4,
    );
    r.check(
        "and it copied those four bytes anyway (this family ignores the flag)",
        &trunc == b"ABCD",
    );
    check_len(
        &mut r,
        "the peek|TRUNC left all ten",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    check_len(
        &mut r,
        "a fourth stream send answers 10",
        send(&a, b"klmnopqrst", SendFlags::empty()),
        10,
    );
    trunc = [0xAAu8; 4];
    check_len(
        &mut r,
        "a stream read|TRUNC answers 4",
        rcv(&b, &mut trunc, RecvFlags::TRUNC),
        4,
    );
    r.check("and it copied the four it consumed", &trunc == b"klmn");
    check_len(
        &mut r,
        "the read|TRUNC consumed exactly four",
        rcv(&b, &mut buf, RecvFlags::empty()),
        6,
    );
    r.check("the remaining bytes are 'opqrst'", &buf[..6] == b"opqrst");

    // End of stream: the look is still an answer, zero, and not a park.
    drop(a);
    check_len(
        &mut r,
        "a stream peek at end-of-stream answers 0",
        rcv(&b, &mut buf, RecvFlags::PEEK),
        0,
    );

    // ---------------------------------------------------------------------
    // A datagram: a peek leaves the message, and `MSG_TRUNC` names its length.
    // ---------------------------------------------------------------------
    let (a, b) = pair(
        &mut r,
        SocketType::DGRAM,
        "an AF_UNIX SOCK_DGRAM pair opens",
    );
    check_len(
        &mut r,
        "a ten-byte datagram send answers 10",
        send(&a, b"abcdefghij", SendFlags::empty()),
        10,
    );
    let mut buf = [0u8; 10];
    check_len(
        &mut r,
        "a 4-byte datagram peek of 10 answers 4",
        rcv(&b, &mut buf[..4], RecvFlags::PEEK),
        4,
    );
    r.check("the datagram peek copied 'abcd'", &buf[..4] == b"abcd");
    check_len(
        &mut r,
        "the datagram peek left the message whole",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    check_len(
        &mut r,
        "another datagram send answers 10",
        send(&a, b"0123456789", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a datagram peek|TRUNC of a 10-byte message answers 10",
        rcv(&b, &mut buf[..4], RecvFlags::PEEK | RecvFlags::TRUNC),
        10,
    );
    check_len(
        &mut r,
        "the peek|TRUNC still left the message",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    check_len(
        &mut r,
        "a zero-length datagram send answers 10 again",
        send(&a, b"ABCDEFGHIJ", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a zero-length datagram peek answers 0",
        rcv(&b, &mut [], RecvFlags::PEEK),
        0,
    );
    check_len(
        &mut r,
        "the zero-length peek left the message",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    check_len(
        &mut r,
        "a datagram send for the consuming read answers 10",
        send(&a, b"qrstuvwxyz", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a zero-length non-peek datagram receive answers 0",
        rcv(&b, &mut [], RecvFlags::empty()),
        0,
    );
    check_errno(
        &mut r,
        "a zero-length datagram receive did consume the message",
        rcv(&b, &mut buf, RecvFlags::DONTWAIT),
        AGAIN,
    );

    check_len(
        &mut r,
        "a datagram send for read|TRUNC answers 10",
        send(&a, b"1234567890", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a datagram read|TRUNC of 4 answers the message's 10",
        rcv(&b, &mut buf[..4], RecvFlags::TRUNC),
        10,
    );
    check_errno(
        &mut r,
        "the datagram read|TRUNC consumed the message",
        rcv(&b, &mut buf, RecvFlags::DONTWAIT),
        AGAIN,
    );

    // `recvmsg` reports the cut in `msg_flags` on a peek, unasked.
    check_len(
        &mut r,
        "a datagram send for recvmsg answers 10",
        send(&a, b"abcdefghij", SendFlags::empty()),
        10,
    );
    let mut body = [0u8; 4];
    let mut space = [MaybeUninit::<u8>::uninit(); 64];
    let (got, _) = recv_with_fds(&b, &mut body, &mut space, RecvFlags::PEEK);
    match got {
        Ok(msg) => {
            r.check("a datagram recvmsg peek answers 4", msg.bytes == 4);
            r.check(
                "the peek's msg_flags carries MSG_TRUNC unasked",
                msg.flags.contains(rustix::net::ReturnFlags::TRUNC),
            );
        }
        Err(e) => {
            r.check(&format!("a datagram recvmsg peek answers 4 ({e})"), false);
            r.check("the peek's msg_flags carries MSG_TRUNC unasked", false);
        }
    }
    let (got, _) = recv_with_fds(&b, &mut body, &mut space, RecvFlags::empty());
    match got {
        Ok(msg) => {
            r.check("the recvmsg after the peek answers 4", msg.bytes == 4);
            r.check(
                "a consuming recvmsg's msg_flags carries MSG_TRUNC",
                msg.flags.contains(rustix::net::ReturnFlags::TRUNC),
            );
        }
        Err(e) => r.check(&format!("the recvmsg after the peek ({e})"), false),
    }
    check_errno(
        &mut r,
        "the consuming recvmsg took the whole message",
        rcv(&b, &mut buf, RecvFlags::DONTWAIT),
        AGAIN,
    );

    // A sequenced-packet pair is served as the datagram this family makes it.
    let (a, b) = pair(
        &mut r,
        SocketType::SEQPACKET,
        "an AF_UNIX SOCK_SEQPACKET pair opens",
    );
    check_len(
        &mut r,
        "a seqpacket send answers 10",
        send(&a, b"abcdefghij", SendFlags::empty()),
        10,
    );
    check_len(
        &mut r,
        "a 4-byte seqpacket peek answers 4",
        rcv(&b, &mut buf[..4], RecvFlags::PEEK),
        4,
    );
    check_len(
        &mut r,
        "the seqpacket peek left the message",
        rcv(&b, &mut buf, RecvFlags::empty()),
        10,
    );

    // ---------------------------------------------------------------------
    // SCM_RIGHTS: a peek installs the descriptors and leaves the record.
    // ---------------------------------------------------------------------
    let (a, b) = pair(
        &mut r,
        SocketType::STREAM,
        "a stream pair for the handover opens",
    );
    let devnull = match open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("open /dev/null for the handover ({e})"), false);
            println!("FAILED");
            std::process::exit(1)
        }
    };
    let fds = [devnull.as_fd()];
    let mut send_space = [MaybeUninit::<u8>::uninit(); 64];
    let mut control = SendAncillaryBuffer::new(&mut send_space);
    r.check(
        "the control buffer took the descriptor",
        control.push(SendAncillaryMessage::ScmRights(&fds)),
    );
    check_len(
        &mut r,
        "one byte with a descriptor rides one sendmsg",
        sendmsg(&a, &[IoSlice::new(b"x")], &mut control, SendFlags::empty()),
        1,
    );

    let mut one = [0u8; 8];
    let (peeked, peek_fds) = recv_with_fds(&b, &mut one, &mut send_space, RecvFlags::PEEK);
    match &peeked {
        Ok(msg) => r.check("the peeked recvmsg answers its one byte", msg.bytes == 1),
        Err(e) => r.check(
            &format!("the peeked recvmsg answers its one byte ({e})"),
            false,
        ),
    }
    r.check("the peek delivered the descriptor", peek_fds.len() == 1);
    r.check("the peek copied the byte", one[0] == b'x');

    let (read, read_fds) = recv_with_fds(&b, &mut one, &mut send_space, RecvFlags::empty());
    match &read {
        Ok(msg) => r.check(
            "the read after the peek answers its one byte",
            msg.bytes == 1,
        ),
        Err(e) => r.check(
            &format!("the read after the peek answers its one byte ({e})"),
            false,
        ),
    }
    r.check(
        "the read delivered the record a second time",
        read_fds.len() == 1,
    );
    drop((peeked, peek_fds, read, read_fds));

    // ---------------------------------------------------------------------
    // A datagram peek names the sender, like the consuming receive does.
    // ---------------------------------------------------------------------
    let a = unconnected(&mut r, SocketType::DGRAM, "a sender datagram socket opens");
    let b = unconnected(
        &mut r,
        SocketType::DGRAM,
        "a receiver datagram socket opens",
    );
    r.check(
        "the sender binds an abstract name",
        bind_abstract(&a, b"mpos-peek-a").is_ok(),
    );
    r.check(
        "the receiver binds an abstract name",
        bind_abstract(&b, b"mpos-peek-b").is_ok(),
    );
    let sent = unsafe {
        libc::sendto(
            a.as_raw_fd(),
            b"named".as_ptr() as *const libc::c_void,
            5,
            0,
            &abstract_addr(b"mpos-peek-b") as *const libc::sockaddr_un as *const libc::sockaddr,
            abstract_len(b"mpos-peek-b"),
        )
    };
    r.check("a named datagram send answers 5", sent == 5);

    let (n, from) = recvfrom_with_name(&b, 3, libc::MSG_PEEK | libc::MSG_DONTWAIT);
    r.check("a named datagram peek answers 3", n == 3);
    r.check(
        "the peek names the sender's abstract address",
        from.as_deref() == Some(&b"mpos-peek-a"[..]),
    );
    let (n, from) = recvfrom_with_name(&b, 5, libc::MSG_DONTWAIT);
    r.check("the consuming receive answers 5", n == 5);
    r.check(
        "and it names the same sender",
        from.as_deref() == Some(&b"mpos-peek-a"[..]),
    );

    r.finish()
}

/// One AF_UNIX bind of an abstract name, through `libc` because that is the
/// call whose `sockaddr_un` length the address claim below is about.
fn bind_abstract(fd: &OwnedFd, name: &[u8]) -> Result<(), i32> {
    let addr = abstract_addr(name);
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            abstract_len(name),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

fn abstract_addr(name: &[u8]) -> libc::sockaddr_un {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // An abstract name starts at the first byte *after* the family, with NUL
    // at `sun_path[0]`; the bytes are length-delimited, not NUL-terminated.
    addr.sun_path[0] = 0;
    for (i, byte) in name.iter().enumerate() {
        addr.sun_path[1 + i] = *byte as libc::c_char;
    }
    addr
}

fn abstract_len(name: &[u8]) -> libc::socklen_t {
    let base = std::mem::offset_of!(libc::sockaddr_un, sun_path) as libc::socklen_t;
    base + 1 + name.len() as libc::socklen_t
}

/// One `recvfrom` whose source address is decoded as an abstract AF_UNIX name.
/// Answers the count and the name, empty when the call failed or the address
/// was not abstract.
fn recvfrom_with_name(fd: &OwnedFd, len: usize, flags: i32) -> (isize, Option<Vec<u8>>) {
    let mut buf = [0u8; 16];
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut alen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let n = unsafe {
        libc::recvfrom(
            fd.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            len,
            flags,
            &mut addr as *mut libc::sockaddr_un as *mut libc::sockaddr,
            &mut alen,
        )
    };
    let base = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let name = if n >= 0
        && alen as usize > base
        && addr.sun_family == libc::AF_UNIX as libc::sa_family_t
        && addr.sun_path[0] == 0
    {
        let bytes = &addr.sun_path[1..(alen as usize - base)];
        Some(bytes.iter().map(|c| *c as u8).collect())
    } else {
        None
    };
    (n, name)
}
