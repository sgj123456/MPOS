//! t-recvmsg-iov - the data room a `recvmsg` scatters a message across.
//!
//! A `recvmsg` names a *chain* of segments, not one buffer, and this kernel
//! received into the first non-empty one only - so a caller with a 4+4+2 chain
//! got four bytes of a ten-byte message and the rest of the chain untouched.
//! The contract below was measured on a current host first
//! (`.workbuddy/probe_recvmsg_iov.c`, Linux 7.2.x), one claim per answer, and
//! the two families disagree in ways one sentence would have got wrong:
//!
//! * a **datagram** fills the chain in order and stops where the chain ends:
//!   12 bytes into `5+5+5` answer 12, 12 into `4+4+2` answer 10, copy the
//!   prefix and set `MSG_TRUNC` in `msg_flags` (with the flag asked for, the
//!   *return* is the datagram's own length, 12, while the copy stays 10). A
//!   zero-length chain - and an empty iovec array - answer 0 with the cut flag
//!   and **consume** the datagram.
//! * a **stream** has no message to cut: 20 bytes into `4+4+2` answer 10 with
//!   `msg_flags` clear and leave the other ten queued, and `MSG_TRUNC` counts
//!   ten while copying **none** of them. A zero-length chain answers 0 without
//!   consuming anything - on `AF_INET` and on `AF_UNIX` alike.
//! * a segment the kernel cannot write is `EFAULT`, and what happens to the
//!   message after that is the family's rule and not the fault's: a datagram
//!   has already been taken (measured: the queue is empty afterwards, and the
//!   next receive is `EAGAIN`), while a stream's bytes stay queued (measured:
//!   the next receive still reads all 12).
//! * an iovec *array* the kernel cannot read is `EFAULT`, and one longer than
//!   `UIO_MAXIOV` (1024) is `EMSGSIZE` - on `recvmsg` and on `sendmsg` alike,
//!   where this kernel answered `EINVAL` for the send side.
//! * `msg_name` is written the same way whatever the chain looks like.
//!
//! The calls go through the raw syscall rather than a wrapper, because two of
//! the claims are about pointers no wrapper will build: a chain whose second
//! segment points into a `PROT_NONE` page, and a chain array that is itself
//! unreadable. Every errno is `libc`'s.
//!
//! One shape stays as it was: a **raw** socket's zero-length receive, because
//! the host this contract was measured on holds no `CAP_NET_RAW`.

use std::net::Ipv4Addr;

use mpos_user_tests::Report;
use rustix::fd::{AsRawFd, OwnedFd, RawFd};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, SendFlags, SocketAddrV4, SocketType, accept, bind, connect, getsockname, listen,
    send, socket, socketpair,
};

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// What one `recvmsg` answered.
struct Answer {
    /// The syscall's return, or -1 on a refusal.
    n: isize,
    /// `msg_flags` as the kernel left it.
    flags: i32,
    err: Option<i32>,
    /// The sender's port, when `msg_name` was filled.
    from_port: Option<u16>,
}

impl Answer {
    fn refused(&self, errno: Errno) -> bool {
        self.err == Some(errno.raw_os_error())
    }

    fn cut(&self) -> bool {
        self.flags & libc::MSG_TRUNC != 0
    }
}

/// One `recvmsg` into a chain of segments the caller names.
///
/// `segs` is `(pointer, length)` per iovec, in the caller's order. `iov` and
/// `iovlen` can be overridden so a claim can hand the kernel an *array* it
/// cannot read (`unreadable_iov`) or one longer than `UIO_MAXIOV`.
fn recv_chain(fd: RawFd, segs: &[(*mut u8, usize)], flags: i32) -> Answer {
    let mut iov: Vec<libc::iovec> = segs
        .iter()
        .map(|&(base, len)| libc::iovec {
            iov_base: base as *mut libc::c_void,
            iov_len: len,
        })
        .collect();
    let mut name = [0u8; 32];
    let mut control = [0u8; 64];
    let mut hdr: libc::msghdr = unsafe { core::mem::zeroed() };
    hdr.msg_name = name.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_namelen = name.len() as u32;
    hdr.msg_iov = iov.as_mut_ptr();
    hdr.msg_iovlen = iov.len() as _;
    hdr.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_controllen = control.len() as _;
    let n = unsafe { libc::syscall(libc::SYS_recvmsg, fd, &mut hdr as *mut libc::msghdr, flags) }
        as isize;
    let err = if n < 0 {
        Some(unsafe { *libc::__errno_location() })
    } else {
        None
    };
    let from_port = if hdr.msg_namelen >= 8 {
        // `sockaddr_in`: family, port (network order), address.
        let port = u16::from_be_bytes([name[2], name[3]]);
        Some(port)
    } else {
        None
    };
    Answer {
        n,
        flags: hdr.msg_flags,
        err,
        from_port,
    }
}

/// One `recvmsg` whose iovec *array* is not what the ordinary builder makes:
/// `array` is the pointer the kernel is given, `len` the count beside it.
fn recv_raw_iov(fd: RawFd, array: *mut libc::iovec, len: usize, flags: i32) -> Answer {
    let mut name = [0u8; 32];
    let mut hdr: libc::msghdr = unsafe { core::mem::zeroed() };
    hdr.msg_name = name.as_mut_ptr() as *mut libc::c_void;
    hdr.msg_namelen = name.len() as u32;
    hdr.msg_iov = array;
    hdr.msg_iovlen = len as _;
    let n = unsafe { libc::syscall(libc::SYS_recvmsg, fd, &mut hdr as *mut libc::msghdr, flags) }
        as isize;
    let err = if n < 0 {
        Some(unsafe { *libc::__errno_location() })
    } else {
        None
    };
    Answer {
        n,
        flags: hdr.msg_flags,
        err,
        from_port: None,
    }
}

/// A page no write can reach: `PROT_NONE`, so the kernel's own copy faults and
/// answers `EFAULT` rather than the syscall failing earlier for another reason.
fn guard() -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(p != libc::MAP_FAILED, "mmap PROT_NONE");
    p as *mut u8
}

/// A UDP pair on the loopback: `server` is bound, `client` connected to it.
fn udp_pair() -> (OwnedFd, OwnedFd) {
    let server = socket(AddressFamily::INET, SocketType::DGRAM, None).expect("udp server");
    bind(&server, &SocketAddrV4::new(LOOPBACK, 0)).expect("bind");
    let named = SocketAddrV4::try_from(getsockname(&server).expect("getsockname")).expect("v4");
    let client = socket(AddressFamily::INET, SocketType::DGRAM, None).expect("udp client");
    connect(&client, &named).expect("connect");
    (server, client)
}

/// A TCP pair on the loopback, the accepted end first.
fn tcp_pair() -> (OwnedFd, OwnedFd) {
    let listener = socket(AddressFamily::INET, SocketType::STREAM, None).expect("tcp server");
    bind(&listener, &SocketAddrV4::new(LOOPBACK, 0)).expect("bind");
    let named = SocketAddrV4::try_from(getsockname(&listener).expect("getsockname")).expect("v4");
    listen(&listener, 1).expect("listen");
    let client = socket(AddressFamily::INET, SocketType::STREAM, None).expect("tcp client");
    connect(&client, &named).expect("connect");
    let accepted = accept(&listener).expect("accept");
    (accepted, client)
}

/// The bytes a chain holds, concatenated: what "the message arrived" looks
/// like from the caller's side, without caring which segment each byte landed
/// in.
fn joined(segs: &[&[u8]]) -> Vec<u8> {
    segs.concat()
}

/// One catchable signal, installed without `SA_RESTART`, so a parked receive is
/// ended rather than restarted.
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

/// Wait until the stack has moved the datagram into the socket's queue: the
/// poll happens on the receive, so a claim that does not wait measures the
/// sender's timing rather than the kernel.
fn wait_readable(fd: RawFd) {
    for _ in 0..200 {
        if rustix::io::ioctl_fionread(unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) })
            .is_ok_and(|n| n > 0)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn queued(fd: RawFd) -> i32 {
    rustix::io::ioctl_fionread(unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) })
        .map(|n| n as i32)
        .unwrap_or(-1)
}

fn main() {
    println!("t-recvmsg-iov");
    let mut r = Report::new();
    let bad = guard();

    // ------------------------------------------------------------------ UDP
    let (server, client) = udp_pair();
    let fd = server.as_raw_fd();
    r.check(
        "a 12-byte datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    // The sender's name is read once the send has given the socket one: an
    // unbound datagram socket has no port until its first send assigns it, and
    // this kernel's `connect` does not bind (Linux's does, which is why the
    // host run passed with the name read first).
    let client_port = SocketAddrV4::try_from(getsockname(&client).expect("client name"))
        .expect("v4")
        .port();
    wait_readable(fd);
    let (mut a, mut b, mut c) = ([0u8; 5], [0u8; 5], [0u8; 5]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), a.len()),
            (b.as_mut_ptr(), b.len()),
            (c.as_mut_ptr(), c.len()),
        ],
        0,
    );
    r.check(
        "into 5+5+5 it answers 12 and fills every segment",
        answer.err.is_none() && answer.n == 12 && joined(&[&a, &b, &c[..2]]) == b"ABCDEFGHIJKL",
    );
    r.check("and no cut is reported", !answer.cut());
    r.check(
        "and msg_name still names the sender",
        answer.from_port == Some(client_port),
    );

    r.check(
        "a second datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let (mut a, mut b, mut c) = ([0u8; 4], [0u8; 4], [0u8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), a.len()),
            (b.as_mut_ptr(), b.len()),
            (c.as_mut_ptr(), c.len()),
        ],
        0,
    );
    r.check(
        "into 4+4+2 it answers 10 and scatters the prefix through the chain",
        answer.err.is_none() && answer.n == 10 && &a == b"ABCD" && &b == b"EFGH" && &c == b"IJ",
    );
    r.check(
        "and says the datagram was cut, without being asked",
        answer.cut(),
    );
    r.check("and the rest of that datagram is gone", queued(fd) == 0);

    r.check(
        "a third datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let (mut a, mut b, mut c) = ([0u8; 4], [0u8; 4], [0u8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), a.len()),
            (b.as_mut_ptr(), b.len()),
            (c.as_mut_ptr(), c.len()),
        ],
        libc::MSG_TRUNC,
    );
    r.check(
        "the same chain with MSG_TRUNC answers the datagram's own length (12)",
        answer.err.is_none() && answer.n == 12 && answer.cut(),
    );
    r.check(
        "while still copying only what fitted (10)",
        joined(&[&a, &b, &c]) == b"ABCDEFGHIJ",
    );

    r.check(
        "a fourth datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let (mut a, mut b) = ([0u8; 0], [0u8; 0]);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 0), (b.as_mut_ptr(), 0)], 0);
    r.check(
        "a zero-length chain answers 0 and reports the cut",
        answer.err.is_none() && answer.n == 0 && answer.cut(),
    );
    r.check("and consumes the datagram", queued(fd) == 0);

    r.check(
        "a fifth datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let answer = recv_chain(fd, &[], 0);
    r.check(
        "an empty iovec array answers 0 too",
        answer.err.is_none() && answer.n == 0 && answer.cut(),
    );
    r.check("and consumes it as well", queued(fd) == 0);

    r.check(
        "a sixth datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let mut zero = [0u8; 0];
    let answer = recv_chain(fd, &[(zero.as_mut_ptr(), 0)], libc::MSG_TRUNC);
    r.check(
        "a single zero-length segment with MSG_TRUNC answers 12",
        answer.err.is_none() && answer.n == 12,
    );

    r.check(
        "a seventh datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let (mut a, mut b) = ([0u8; 4], [0u8; 4]);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 4), (b.as_mut_ptr(), 0), (bad, 4)], 0);
    r.check(
        "a chain whose later segment cannot be written is EFAULT",
        answer.refused(Errno::FAULT),
    );
    r.check(
        "and the datagram is gone: a datagram's fault comes after the take",
        queued(fd) == 0,
    );

    r.check(
        "an eighth datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let answer = recv_raw_iov(fd, bad as *mut libc::iovec, 1, 0);
    r.check(
        "an iovec array the kernel cannot read is EFAULT",
        answer.refused(Errno::FAULT),
    );
    let answer = recv_chain(fd, &[(bad, 4096)], 0);
    r.check(
        "and a single segment that cannot be written is EFAULT too",
        answer.refused(Errno::FAULT),
    );

    r.check(
        "a ninth datagram is sent",
        send(&client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    wait_readable(fd);
    let mut many: Vec<libc::iovec> = (0..1025)
        .map(|_| libc::iovec {
            iov_base: bad as *mut libc::c_void,
            iov_len: 0,
        })
        .collect();
    let answer = recv_raw_iov(fd, many.as_mut_ptr(), many.len(), 0);
    r.check(
        "an iovec array longer than UIO_MAXIOV is EMSGSIZE",
        answer.refused(Errno::MSGSIZE),
    );

    let (mut a, mut b) = ([0u8; 5], [0u8; 5]);
    let answer = recv_chain(
        fd,
        &[(a.as_mut_ptr(), 5), (b.as_mut_ptr(), 5)],
        libc::MSG_PEEK,
    );
    r.check(
        "a 5+5 peek answers 10 and reports the cut",
        answer.err.is_none() && answer.n == 10 && answer.cut(),
    );
    r.check("and leaves the whole datagram queued", queued(fd) == 12);

    // ------------------------------------------------------------------ TCP
    let (accepted, tcp_client) = tcp_pair();
    let fd = accepted.as_raw_fd();
    r.check(
        "12 bytes are written to a stream",
        send(&tcp_client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    let (mut a, mut b, mut c) = ([0u8; 5], [0u8; 5], [0u8; 5]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 5),
            (b.as_mut_ptr(), 5),
            (c.as_mut_ptr(), 5),
        ],
        0,
    );
    r.check(
        "a stream into 5+5+5 answers 12 and fills them",
        answer.err.is_none() && answer.n == 12 && joined(&[&a, &b, &c[..2]]) == b"ABCDEFGHIJKL",
    );

    r.check(
        "20 bytes are written",
        send(&tcp_client, b"ABCDEFGHIJKLMNOPQRST", SendFlags::empty()).is_ok_and(|n| n == 20),
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (mut a, mut b, mut c) = ([0u8; 4], [0u8; 4], [0u8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 4),
            (b.as_mut_ptr(), 4),
            (c.as_mut_ptr(), 2),
        ],
        0,
    );
    r.check(
        "into 4+4+2 it answers 10 and fills them",
        answer.err.is_none() && answer.n == 10 && joined(&[&a, &b, &c]) == b"ABCDEFGHIJ",
    );
    r.check(
        "and reports no cut: a stream has no message boundary",
        !answer.cut(),
    );
    r.check("and the other ten bytes stay queued", queued(fd) == 10);

    let (mut a, mut b) = ([0u8; 0], [0u8; 0]);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 0), (b.as_mut_ptr(), 0)], 0);
    r.check(
        "a zero-length chain on a stream answers 0",
        answer.err.is_none() && answer.n == 0,
    );
    r.check("and takes nothing", queued(fd) == 10);

    let (mut a, mut b, mut c) = ([0xEEu8; 4], [0xEEu8; 4], [0xEEu8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 4),
            (b.as_mut_ptr(), 4),
            (c.as_mut_ptr(), 2),
        ],
        libc::MSG_TRUNC,
    );
    r.check(
        "a stream chain with MSG_TRUNC answers the count it would have copied (10)",
        answer.err.is_none() && answer.n == 10 && !answer.cut(),
    );
    r.check(
        "while copying none of it",
        a.iter().all(|&x| x == 0xEE)
            && b.iter().all(|&x| x == 0xEE)
            && c.iter().all(|&x| x == 0xEE),
    );
    r.check("and consuming the ten it counted", queued(fd) == 0);

    r.check(
        "12 bytes are written again",
        send(&tcp_client, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (mut a, mut b) = ([0u8; 4], [0u8; 4]);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 4), (b.as_mut_ptr(), 0), (bad, 8)], 0);
    r.check(
        "a stream chain with an unwritable segment is EFAULT",
        answer.refused(Errno::FAULT),
    );
    r.check(
        "and a stream's fault leaves its bytes queued",
        queued(fd) == 12,
    );

    let (mut a, mut b) = ([0u8; 5], [0u8; 5]);
    let answer = recv_chain(
        fd,
        &[(a.as_mut_ptr(), 5), (b.as_mut_ptr(), 5)],
        libc::MSG_PEEK,
    );
    r.check(
        "a stream peek through a 5+5 chain answers 10",
        answer.err.is_none() && answer.n == 10 && joined(&[&a, &b]) == b"ABCDEFGHIJ",
    );
    r.check("and leaves all 12 queued", queued(fd) == 12);

    // ---------------------------------------------------------- AF_UNIX stream
    let (left, right) = socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        rustix::net::SocketFlags::empty(),
        None,
    )
    .expect("unix pair");
    let fd = left.as_raw_fd();
    r.check(
        "12 bytes are written to an AF_UNIX stream",
        send(&right, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    let (mut a, mut b, mut c) = ([0u8; 5], [0u8; 5], [0u8; 5]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 5),
            (b.as_mut_ptr(), 5),
            (c.as_mut_ptr(), 5),
        ],
        0,
    );
    r.check(
        "an AF_UNIX stream into 5+5+5 answers 12 and fills them",
        answer.err.is_none() && answer.n == 12 && joined(&[&a, &b, &c[..2]]) == b"ABCDEFGHIJKL",
    );
    r.check(
        "12 bytes are written again",
        send(&right, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    let (mut a, mut b, mut c) = ([0u8; 4], [0u8; 4], [0u8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 4),
            (b.as_mut_ptr(), 4),
            (c.as_mut_ptr(), 2),
        ],
        0,
    );
    r.check(
        "into 4+4+2 it answers 10 and leaves the two",
        answer.err.is_none() && answer.n == 10 && queued(fd) == 2,
    );
    let (mut a, mut b) = ([0u8; 0], [0u8; 0]);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 0), (b.as_mut_ptr(), 0)], 0);
    r.check(
        "a zero-length chain on an AF_UNIX stream answers 0 and takes nothing",
        answer.err.is_none() && answer.n == 0 && queued(fd) == 2,
    );
    // The queue is drained first: with bytes still queued a zero-length receive
    // is an immediate 0 (the claim above), so a park here would measure the
    // wrong shape.
    let mut drain = [0u8; 4];
    let answer = recv_chain(fd, &[(drain.as_mut_ptr(), 4)], 0);
    r.check(
        "and the two it left are still there to read",
        answer.err.is_none() && answer.n == 2 && &drain[..2] == b"KL",
    );
    interrupt_after(1);
    let answer = recv_chain(fd, &[(a.as_mut_ptr(), 0)], 0);
    cancel_interrupt();
    r.check(
        "and a parked zero-length one is ended by a signal with EINTR",
        answer.refused(Errno::INTR),
    );

    // --------------------------------------------------------- AF_UNIX datagram
    let (left, right) = socketpair(
        AddressFamily::UNIX,
        SocketType::DGRAM,
        rustix::net::SocketFlags::empty(),
        None,
    )
    .expect("unix dgram pair");
    let fd = left.as_raw_fd();
    r.check(
        "12 bytes are sent on an AF_UNIX datagram pair",
        send(&right, b"ABCDEFGHIJKL", SendFlags::empty()).is_ok_and(|n| n == 12),
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (mut a, mut b, mut c) = ([0u8; 4], [0u8; 4], [0u8; 2]);
    let answer = recv_chain(
        fd,
        &[
            (a.as_mut_ptr(), 4),
            (b.as_mut_ptr(), 4),
            (c.as_mut_ptr(), 2),
        ],
        0,
    );
    r.check(
        "a datagram into 4+4+2 answers 10, scatters it, and reports the cut",
        answer.err.is_none()
            && answer.n == 10
            && answer.cut()
            && joined(&[&a, &b, &c]) == b"ABCDEFGHIJ",
    );
    r.check("and the datagram is consumed", queued(fd) == 0);

    // ------------------------------------------------------------ sendmsg cap
    r.check("a sendmsg with 1025 iovecs is EMSGSIZE too", {
        let mut many: Vec<libc::iovec> = (0..1025)
            .map(|_| libc::iovec {
                iov_base: bad as *mut libc::c_void,
                iov_len: 0,
            })
            .collect();
        let mut hdr: libc::msghdr = unsafe { core::mem::zeroed() };
        hdr.msg_iov = many.as_mut_ptr();
        hdr.msg_iovlen = many.len() as _;
        let n = unsafe {
            libc::syscall(
                libc::SYS_sendmsg,
                right.as_raw_fd(),
                &mut hdr as *mut libc::msghdr,
                0,
            )
        };
        n < 0 && unsafe { *libc::__errno_location() } == libc::EMSGSIZE
    });

    r.finish();
}
