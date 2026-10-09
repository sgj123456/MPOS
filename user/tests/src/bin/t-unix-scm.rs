//! t-unix-scm - the descriptors and identities a unix socket carries.
//!
//! Pinned here, per unix(7) and sendmsg(2): an `SCM_RIGHTS` handover moves a
//! live descriptor from one socket end to the other (the received descriptor
//! reads the same open file the sender held), a bad descriptor is `EBADF` at the
//! send, a control buffer too small for a handover answers `MSG_CTRUNC` and
//! closes the unclaimed descriptor, `MSG_CMSG_CLOEXEC` marks what arrives
//! close-on-exec, `SO_PEERCRED` reports the peer's pid/uid/gid captured at
//! connect, and a reader that set `SO_PASSCRED` gets those same credentials back
//! as an `SCM_CREDENTIALS` on the next read.
//!
//! # The doors, and why this file has two
//!
//! Every door that is not a control message is `rustix`'s: `socketpair` and
//! `pipe` for the fixtures, `sockopt::{socket_peercred, socket_passcred,
//! set_socket_passcred, socket_type}` for the identities and the socket's own
//! type, `fcntl_getfd`/`dup` for the close-on-exec pairs, `event::epoll` for the
//! interest list, `io::{read, write}` for the bytes.
//!
//! The control-message door stays the *raw* one - `libc::sendmsg`/`libc::recvmsg`
//! with the `cmsghdr` fields read at the offsets the kernel writes them - and
//! that is the point of the program rather than a shortcut. Half its claims are
//! *numbers on the wire*: `msg_controllen == CMSG_SPACE(4)` when one descriptor
//! arrived, `cmsg_len == CMSG_LEN(12)` for a `struct ucred`-sized credential,
//! `msg_controllen == 0` when the kernel delivered no control at all, and
//! `MSG_CTRUNC` when the room offered was `sizeof(cmsghdr)` and nothing more.
//! `rustix`'s `SendAncillaryBuffer`/`RecvAncillaryBuffer` compute those numbers
//! internally and hand back only the parsed messages (an `AncillaryIter<OwnedFd>`,
//! a `UCred`), so a claim about the number cannot be made through them - and a
//! send through the crate read back through the raw door would be pinning two
//! different builders against each other. One door for the chain, then, and the
//! crate for everything that is not one.
//!
//! The chain's *header* is read field by field rather than through
//! `libc::cmsghdr`, because the two libcs disagree about that struct's shape
//! (musl's `msghdr` carries a `socklen_t` control length where glibc's carries a
//! `size_t`) while the kernel writes one layout regardless: a `size_t` at 0, the
//! level at 8, the type at 12, and the data at the 16 bytes `CMSG_ALIGN` rounds
//! the header up to.
//!
//! # The one divergence, measured rather than assumed
//!
//! The last section - the handover that carries no bytes at all - is this
//! kernel's own answer. Measured on a host Linux (`.workbuddy/probe_unix_scm.c`):
//! a data-less `SCM_RIGHTS` on a *stream* socketpair is *accepted* and then
//! queues nothing, so a `recvmsg` with no `MSG_DONTWAIT` parks there - the very
//! state the section's comment says this kernel's fix took it out of. The
//! delivery claims are therefore accepted under `MPOS_ON_HOST=1`, where the
//! section makes the non-blocking read and *prints* what the host answered
//! instead of parking the harness.
//!
//! # What a handover is *for*
//!
//! Two of the sections are shaped by a real caller rather than by the ABI. The
//! datagram pair is eudev's manager learning that a worker finished: the
//! credentials belong to the *message* (the pair was made before the fork, so
//! `SO_PEERCRED` would name the waiting manager, not the worker), and
//! `msg_controllen` must report the space actually used - leaving the caller's
//! capacity standing over an empty buffer makes `CMSG_NXTHDR` answer `EINVAL` and
//! udevd discard the reply. The three-step sequence at `SO_PEERCRED`,
//! `F_DUPFD_CLOEXEC` and `epoll_ctl` is `wl_client_create`, which is how a
//! compositor owns the socket it was handed.

use mpos_user_tests::Report;
use rustix::event::epoll;
use rustix::fd::{AsRawFd, FromRawFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::sockopt;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socketpair};

static SOCK: &str = "/tmp/t-unix-scm.sock";

/// `CMSG_ALIGN`, `CMSG_LEN`, `CMSG_SPACE`: the three macros a control chain is
/// measured with, spelled from `asm-generic/socket.h`'s definitions. `libc`
/// publishes neither these nor an agreed `cmsghdr` for both libcs, so the
/// arithmetic lives here - and it is arithmetic the claims below compare
/// against, which is why it is written out rather than reached for.
const CMSG_HDRLEN: usize = 16;

const fn cmsg_align(n: usize) -> usize {
    (n + 7) & !7
}
const fn cmsg_len(n: usize) -> usize {
    CMSG_HDRLEN + n
}
const fn cmsg_space(n: usize) -> usize {
    cmsg_align(CMSG_HDRLEN + n)
}

/// An 8-byte-aligned control area: a `cmsghdr` (16 bytes) then up to two
/// descriptor ints. The alignment is what the kernel's `CMSG_ALIGN` walks on.
#[repr(C, align(8))]
struct Ctrl {
    w: [usize; 4],
}

impl Ctrl {
    const fn new() -> Ctrl {
        Ctrl { w: [0; 4] }
    }

    fn ptr(&mut self) -> *mut u8 {
        self.w.as_mut_ptr().cast::<u8>()
    }

    /// Lay down one `SCM_RIGHTS` chain carrying `fd`, and answer the
    /// `msg_controllen` that covers it.
    fn one_fd(&mut self, fd: i32) -> usize {
        let b = self.ptr();
        // SAFETY: `w` is 32 bytes and every write below is inside it.
        unsafe {
            core::ptr::write_bytes(b, 0, 32);
            b.cast::<usize>().write_unaligned(cmsg_len(4));
            b.add(8).cast::<i32>().write_unaligned(libc::SOL_SOCKET);
            b.add(12).cast::<i32>().write_unaligned(libc::SCM_RIGHTS);
            b.add(16).cast::<i32>().write_unaligned(fd);
        }
        cmsg_space(4)
    }

    /// The chain's three header words, as the kernel wrote them.
    fn hdr(&self) -> (usize, i32, i32) {
        // SAFETY: the kernel wrote a `cmsghdr` here.
        unsafe {
            let b = self.w.as_ptr().cast::<u8>();
            (
                b.cast::<usize>().read_unaligned(),
                b.add(8).cast::<i32>().read_unaligned(),
                b.add(12).cast::<i32>().read_unaligned(),
            )
        }
    }

    /// The first word of the chain's data - one descriptor number.
    fn first_int(&self) -> i32 {
        // SAFETY: the data of a `cmsghdr` starts at `CMSG_HDRLEN`.
        unsafe {
            (self.w.as_ptr().cast::<u8>().add(CMSG_HDRLEN))
                .cast::<i32>()
                .read_unaligned()
        }
    }

    /// The first word of the chain's data, read as the `pid` of a `ucred`.
    fn first_pid(&self) -> u32 {
        // SAFETY: as above; `struct ucred` opens with a `pid_t`.
        unsafe {
            (self.w.as_ptr().cast::<u8>().add(CMSG_HDRLEN))
                .cast::<u32>()
                .read_unaligned()
        }
    }
}

/// A claim about a call whose success is the claim.
fn check_ok<T>(r: &mut Report, name: &str, got: Result<T, Errno>) {
    match got {
        Ok(_) => r.check(name, true),
        Err(err) => r.check(&format!("{name} (got {err})"), false),
    }
}

/// A claim about a call whose *answer* is the claim, for the fixtures the rest
/// of the program is built on: if it did not hold there is nothing to test, so
/// the program says which one and stops.
fn take<T>(r: &mut Report, name: &str, got: Result<T, Errno>) -> T {
    match got {
        Ok(value) => {
            r.check(name, true);
            value
        }
        Err(err) => {
            r.check(&format!("{name} (got {err})"), false);
            println!("FAILED");
            std::process::exit(1);
        }
    }
}

/// One claim about a number a call answered - a byte count, a control length, a
/// descriptor - so a failure says what arrived instead.
fn check_is(r: &mut Report, name: &str, got: isize, want: isize) {
    if got == want {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got}, want {want})"), false);
    }
}

/// A raw call's answer with libc's `-1`/`errno` turned back into the negative
/// errno a bare syscall hands back, which is the shape every claim below is
/// written against. Called in the same expression as the call, because `errno`
/// is only valid until the next one.
fn as_raw(rc: isize) -> isize {
    if rc < 0 {
        -((std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) as isize)
    } else {
        rc
    }
}

/// `sendmsg(fd, &one data buffer, &[control], flags)`.
fn send_with_control(fd: i32, data: &[u8], ctrl: &mut Ctrl, clen: usize, flags: usize) -> isize {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.ptr().cast::<libc::c_void>();
    msg.msg_controllen = clen as _;
    // SAFETY: `msg` points at a live `iovec` and a live control buffer, both of
    // which outlive the call.
    unsafe { as_raw(libc::sendmsg(fd, &msg, flags as libc::c_int)) }
}

/// `recvmsg` into `buf`, answering the return, the flags the kernel set, and
/// the control length it wrote.
fn recv_with_control(
    fd: i32,
    buf: &mut [u8],
    ctrl: &mut Ctrl,
    clen: usize,
    flags: usize,
) -> (isize, i32, usize) {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.ptr().cast::<libc::c_void>();
    msg.msg_controllen = clen as _;
    // SAFETY: as above - the buffer, the iovec and the control area all outlive
    // the call, and the kernel writes only inside them.
    let rc = unsafe { as_raw(libc::recvmsg(fd, &mut msg, flags as libc::c_int)) };
    (rc, msg.msg_flags, msg.msg_controllen as usize)
}

/// A descriptor the kernel handed over, owned once it is taken.
///
/// # Safety
/// `fd` must be an open descriptor this process owns, and not owned elsewhere.
fn own(fd: i32) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn unix_pair(ty: SocketType) -> Result<(OwnedFd, OwnedFd), Errno> {
    socketpair(AddressFamily::UNIX, ty, SocketFlags::empty(), None)
}

/// The errno a call answered, as the negative return a raw syscall gives.
fn refusal(err: Errno) -> isize {
    -(err.raw_os_error() as isize)
}

fn main() {
    println!("t-unix-scm");
    let mut r = Report::new();
    let own_pid = std::process::id();

    // --- SCM_RIGHTS over a socketpair: hand a live pipe end across ---
    let (sp0, sp1) = take(
        &mut r,
        "socketpair(AF_UNIX, SOCK_STREAM) succeeds",
        unix_pair(SocketType::STREAM),
    );
    let (r_end, w_end) = take(
        &mut r,
        "pipe2 for the handover succeeds",
        rustix::pipe::pipe(),
    );

    // Ship the read end of the pipe plus one marker byte.
    let mut cbuf = Ctrl::new();
    let clen = cbuf.one_fd(r_end.as_raw_fd());
    check_is(
        &mut r,
        "sendmsg with SCM_RIGHTS sends the byte",
        send_with_control(sp0.as_raw_fd(), b"x", &mut cbuf, clen, 0),
        1,
    );

    // Read it back on the far end: the byte, and one received descriptor.
    let mut rbuf = [0u8; 8];
    let mut rctrl = Ctrl::new();
    let (got, _flags, rlen) =
        recv_with_control(sp1.as_raw_fd(), &mut rbuf, &mut rctrl, cmsg_space(4), 0);
    check_is(&mut r, "recvmsg reads the marker byte", got, 1);
    check_is(
        &mut r,
        "recvmsg reports one fd of control data",
        rlen as isize,
        cmsg_space(4) as isize,
    );
    let (_, level, ctype) = rctrl.hdr();
    r.check(
        "the control message is SCM_RIGHTS",
        ctype == libc::SCM_RIGHTS && level == libc::SOL_SOCKET,
    );
    let rfd = rctrl.first_int();
    r.check("a descriptor number came back", rfd >= 0);
    let handed = own(rfd);
    // The received descriptor names the SAME open pipe end: write to the far
    // half, read through what was handed over.
    let _ = rustix::io::write(&w_end, b"HELLO");
    let mut got_buf = [0u8; 8];
    let n = rustix::io::read(&handed, &mut got_buf).unwrap_or(0);
    check_is(
        &mut r,
        "the passed descriptor reads the sender's pipe",
        n as isize,
        5,
    );
    r.check(
        "and the bytes are the ones written",
        &got_buf[..5] == b"HELLO",
    );

    // --- MSG_CMSG_CLOEXEC: a received descriptor can arrive close-on-exec ---
    let mut cbuf2 = Ctrl::new();
    let clen2 = cbuf2.one_fd(r_end.as_raw_fd());
    let _ = send_with_control(sp0.as_raw_fd(), b"x", &mut cbuf2, clen2, 0);
    {
        let mut rbuf = [0u8; 8];
        let mut ctrl = Ctrl::new();
        let (got, _f, _l) = recv_with_control(
            sp1.as_raw_fd(),
            &mut rbuf,
            &mut ctrl,
            cmsg_space(4),
            libc::MSG_CMSG_CLOEXEC as usize,
        );
        check_is(&mut r, "MSG_CMSG_CLOEXEC recv reads the byte", got, 1);
        let fd = own(ctrl.first_int());
        let flags = rustix::io::fcntl_getfd(&fd);
        r.check(
            "MSG_CMSG_CLOEXEC set FD_CLOEXEC on the received fd",
            flags.is_ok_and(|f| f.contains(rustix::io::FdFlags::CLOEXEC)),
        );
    }

    // --- MSG_CTRUNC: a too-small control buffer drops the handover ---
    let mut cbuf3 = Ctrl::new();
    let clen3 = cbuf3.one_fd(r_end.as_raw_fd());
    let _ = send_with_control(sp0.as_raw_fd(), b"x", &mut cbuf3, clen3, 0);
    {
        let mut rbuf = [0u8; 8];
        let mut ctrl = Ctrl::new();
        // Room for the header only - not the one descriptor that would fit.
        let (got, flags, rlen) =
            recv_with_control(sp1.as_raw_fd(), &mut rbuf, &mut ctrl, CMSG_HDRLEN, 0);
        check_is(&mut r, "a truncated read still returns the byte", got, 1);
        r.check(
            "a too-small control buffer sets MSG_CTRUNC",
            flags & libc::MSG_CTRUNC != 0,
        );
        check_is(&mut r, "and reports no control delivered", rlen as isize, 0);
    }

    // --- EBADF: a descriptor the sender does not hold is refused at send ---
    let mut cbad = Ctrl::new();
    let cblen = cbad.one_fd(999);
    check_is(
        &mut r,
        "sending an unknown descriptor is EBADF",
        send_with_control(sp0.as_raw_fd(), b"x", &mut cbad, cblen, 0),
        refusal(Errno::BADF),
    );

    // --- SO_PEERCRED + SO_PASSCRED across a real connect ---
    let _ = rustix::fs::unlink(SOCK);
    let listener = take(
        &mut r,
        "the listener socket opens",
        rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None),
    );
    let addr = SocketAddrUnix::new(SOCK).unwrap_or_else(|e| panic!("{e}"));
    check_ok(&mut r, "bind succeeds", rustix::net::bind(&listener, &addr));
    check_ok(&mut r, "listen succeeds", rustix::net::listen(&listener, 4));
    let client = take(
        &mut r,
        "the client socket opens",
        rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None),
    );
    check_ok(
        &mut r,
        "connect succeeds",
        rustix::net::connect(&client, &addr),
    );
    let accepted = take(
        &mut r,
        "accept returns the connected end",
        rustix::net::accept(&listener),
    );

    // The single process here is both peers, so every credential reads back as
    // its own pid and root's zero ids - proof the ids were captured at connect
    // and not left zero.
    match sockopt::socket_peercred(&accepted) {
        Ok(cred) => {
            r.check("getsockopt(SO_PEERCRED) succeeds on the accepted end", true);
            check_is(
                &mut r,
                "SO_PEERCRED reports the peer pid",
                cred.pid.as_raw_pid() as isize,
                own_pid as isize,
            );
            r.check(
                "SO_PEERCRED reports root's uid and gid",
                cred.uid.as_raw() == 0 && cred.gid.as_raw() == 0,
            );
        }
        Err(err) => r.check(
            &format!("getsockopt(SO_PEERCRED) succeeds on the accepted end (got {err})"),
            false,
        ),
    }
    match sockopt::socket_peercred(&client) {
        Ok(cred) => {
            r.check("getsockopt(SO_PEERCRED) succeeds on the client end", true);
            check_is(
                &mut r,
                "the client sees the listener's pid too",
                cred.pid.as_raw_pid() as isize,
                own_pid as isize,
            );
        }
        Err(err) => r.check(
            &format!("getsockopt(SO_PEERCRED) succeeds on the client end (got {err})"),
            false,
        ),
    }

    // SO_PASSCRED turns those same credentials into a per-message delivery.
    check_ok(
        &mut r,
        "setsockopt(SO_PASSCRED) succeeds",
        sockopt::set_socket_passcred(&accepted, true),
    );
    let _ = rustix::io::write(&client, b"p");
    {
        let mut rbuf = [0u8; 8];
        let mut ctrl = Ctrl::new();
        let (got, _f, _l) = recv_with_control(
            accepted.as_raw_fd(),
            &mut rbuf,
            &mut ctrl,
            cmsg_space(12),
            0,
        );
        check_is(&mut r, "the passcred read returns the byte", got, 1);
        let (_, level, ctype) = ctrl.hdr();
        r.check(
            "an SCM_CREDENTIALS message arrives",
            ctype == libc::SCM_CREDENTIALS && level == libc::SOL_SOCKET,
        );
        check_is(
            &mut r,
            "SCM_CREDENTIALS carries the peer pid",
            ctrl.first_pid() as isize,
            own_pid as isize,
        );
    }

    // --- the same credentials on a *datagram* pair, one per message ---
    {
        let (d0, d1) = take(
            &mut r,
            "a SOCK_DGRAM socketpair opens",
            unix_pair(SocketType::DGRAM),
        );
        check_ok(
            &mut r,
            "SO_PASSCRED on the reader succeeds",
            sockopt::set_socket_passcred(&d0, true),
        );
        let mut pay = [0u8; 8];
        let mut dctrl = Ctrl::new();
        let (empty, _f, _l) = recv_with_control(
            d0.as_raw_fd(),
            &mut pay,
            &mut dctrl,
            cmsg_space(12),
            libc::MSG_DONTWAIT as usize,
        );
        check_is(
            &mut r,
            "an empty non-blocking datagram read is EAGAIN",
            empty,
            refusal(Errno::AGAIN),
        );
        check_is(
            &mut r,
            "the peer's write lands as one datagram",
            rustix::io::write(&d1, b"worker!!").map_or(-1, |n| n as isize),
            8,
        );
        let (got, _f, used) = recv_with_control(
            d0.as_raw_fd(),
            &mut pay,
            &mut dctrl,
            cmsg_space(12),
            libc::MSG_DONTWAIT as usize,
        );
        check_is(&mut r, "the passcred datagram read returns it", got, 8);
        let (dlen, level, ctype) = dctrl.hdr();
        r.check(
            "the message carries SCM_CREDENTIALS",
            level == libc::SOL_SOCKET && ctype == libc::SCM_CREDENTIALS,
        );
        check_is(
            &mut r,
            "its cmsg_len is CMSG_LEN(sizeof(struct ucred))",
            dlen as isize,
            cmsg_len(12) as isize,
        );
        check_is(
            &mut r,
            "those are the *sender's* credentials, not the socket's",
            dctrl.first_pid() as isize,
            own_pid as isize,
        );
        check_is(
            &mut r,
            "msg_controllen reports the space used",
            used as isize,
            cmsg_space(12) as isize,
        );

        // A reader that never asked gets the bytes and an *empty* chain: the
        // capacity it offered is not an answer from the kernel, and leaving it
        // standing is the `EINVAL` above.
        let (n0, n1) = take(
            &mut r,
            "a second SOCK_DGRAM pair opens without SO_PASSCRED",
            unix_pair(SocketType::DGRAM),
        );
        let mut nctrl = Ctrl::new();
        check_is(
            &mut r,
            "the peer's write lands",
            rustix::io::write(&n1, b"no-cred!").map_or(-1, |n| n as isize),
            8,
        );
        let (got, _f, nlen) = recv_with_control(
            n0.as_raw_fd(),
            &mut pay,
            &mut nctrl,
            cmsg_space(12),
            libc::MSG_DONTWAIT as usize,
        );
        check_is(
            &mut r,
            "a reader without SO_PASSCRED still gets the datagram",
            got,
            8,
        );
        check_is(
            &mut r,
            "and its control chain is empty, not unfilled",
            nlen as isize,
            0,
        );
    }

    // --- the pair asked the way a compositor's launcher asks it ---
    //
    // `wl_client_create` takes one half of a `socketpair` and, before it will
    // own the descriptor, does three things to it: dups it close-on-exec, puts
    // the duplicate into an `epoll` interest list, and reads `SO_PEERCRED` off
    // the original. A pair that is not in the network registry failed the last of
    // those with `ENOTSOCK`, and weston lost every client it launched - so this
    // is that exact three-step sequence, in that order.
    match sockopt::socket_peercred(&sp0) {
        Ok(cred) => {
            r.check("SO_PEERCRED succeeds on a socketpair end", true);
            check_is(
                &mut r,
                "a socketpair reports its creator's pid as the peer",
                cred.pid.as_raw_pid() as isize,
                own_pid as isize,
            );
        }
        Err(err) => r.check(
            &format!("SO_PEERCRED succeeds on a socketpair end (got {err})"),
            false,
        ),
    }
    // `F_DUPFD_CLOEXEC`, the one libwayland uses to keep the duplicate out of a
    // child's way.
    match rustix::io::dup(&sp0) {
        Ok(dup) => {
            r.check("the end dups close-on-exec", true);
            match epoll::create(epoll::CreateFlags::empty()) {
                Ok(epfd) => {
                    r.check("an interest list is made", true);
                    check_ok(
                        &mut r,
                        "epoll watches the dup of a socketpair end",
                        epoll::add(
                            &epfd,
                            &dup,
                            epoll::EventData::new_u64(0),
                            epoll::EventFlags::IN,
                        ),
                    );
                }
                Err(err) => r.check(&format!("an interest list is made (got {err})"), false),
            }
        }
        Err(err) => r.check(&format!("the end dups close-on-exec (got {err})"), false),
    }

    // And the two knobs the same caller reads beside it: what the channel is, and
    // whether `SO_PASSCRED` round-trips through the pair rather than falling
    // through to a registry that does not know it.
    match sockopt::socket_type(&sp1) {
        Ok(t) => {
            r.check("SO_TYPE on a socketpair reads SOCK_STREAM", true);
            r.check(
                "the pair calls itself a stream socket",
                t == SocketType::STREAM,
            );
        }
        Err(err) => r.check(
            &format!("SO_TYPE on a socketpair reads SOCK_STREAM (got {err})"),
            false,
        ),
    }
    check_ok(
        &mut r,
        "setsockopt(SO_PASSCRED) succeeds on a socketpair end",
        sockopt::set_socket_passcred(&sp1, true),
    );
    match sockopt::socket_passcred(&sp1) {
        Ok(on) => {
            r.check("SO_PASSCRED reads back on a socketpair end", true);
            r.check("the pair remembers the flag", on);
        }
        Err(err) => r.check(
            &format!("SO_PASSCRED reads back on a socketpair end (got {err})"),
            false,
        ),
    }

    let _ = rustix::fs::unlink(SOCK);

    // --- one control chain per send, not one per read ---
    //
    // A stream has no message boundaries of its own, so handing a descriptor
    // back with *its* bytes means knowing where each send ended. Two one-byte
    // sends, each carrying a different pipe, are read by a caller with room for
    // both at once: they must still arrive as two deliveries, and which pipe came
    // with which byte is what shows the pairing is real rather than whatever
    // order the kernel happened to pop its queue in.
    {
        let (s0, s1) = take(
            &mut r,
            "a stream pair for the boundary check",
            unix_pair(SocketType::STREAM),
        );
        let (first_r, first_w) = take(&mut r, "the first pipe exists", rustix::pipe::pipe());
        let (second_r, second_w) = take(&mut r, "the second pipe exists", rustix::pipe::pipe());
        let _ = rustix::io::write(&first_w, b"ONE");
        let _ = rustix::io::write(&second_w, b"TWO");

        // Two separate payloads rather than one overwritten: a test that mutates
        // one buffer between the sends proves nothing about what the second send
        // actually carried.
        let mut cb = Ctrl::new();
        let clen_a = cb.one_fd(first_r.as_raw_fd());
        check_is(
            &mut r,
            "the first byte goes with the first pipe",
            send_with_control(s0.as_raw_fd(), b"A", &mut cb, clen_a, 0),
            1,
        );
        let clen_b = cb.one_fd(second_r.as_raw_fd());
        check_is(
            &mut r,
            "and the second byte with the second",
            send_with_control(s0.as_raw_fd(), b"B", &mut cb, clen_b, 0),
            1,
        );

        // Room for far more than one send's worth, so a kernel without boundaries
        // would happily take both in one go.
        let mut got = [0u8; 16];
        let mut rctrl = Ctrl::new();
        for (i, want) in [(0u8, b'A'), (1u8, b'B')] {
            let (n, _f, _l) = recv_with_control(
                s1.as_raw_fd(),
                &mut got,
                &mut rctrl,
                core::mem::size_of::<Ctrl>(),
                0,
            );
            // One byte, not two: the read stopped at the boundary the descriptor
            // it carried belonged to.
            check_is(&mut r, "a read stops at one send's end", n, 1);
            r.check("and the byte is the one that send wrote", got[0] == want);
            let (_, level, ctype) = rctrl.hdr();
            r.check(
                "with an SCM_RIGHTS chain of its own",
                level == libc::SOL_SOCKET && ctype == libc::SCM_RIGHTS,
            );
            let fd = own(rctrl.first_int());
            let mut probe = [0u8; 4];
            let n = rustix::io::read(&fd, &mut probe).unwrap_or(0);
            check_is(
                &mut r,
                "and it is the descriptor that went with it",
                n as isize,
                3,
            );
            let expect: &[u8] = if i == 0 { b"ONE" } else { b"TWO" };
            r.check("named by what it holds", &probe[..3] == expect);
        }
    }

    // --- a handover that carries no bytes at all ---
    //
    // `sendmsg` with descriptors and an empty iovec is a legal message, and the
    // record it queues ends exactly where the stream already is: it is due the
    // moment it is queued, and no byte will ever arrive *with* it. A read that
    // waited for one would wait forever with the descriptors already in hand, so
    // the read answers with the record and a zero-byte return instead. The check
    // that matters is that this returns at all - before the fix it parked the
    // machine until the harness gave up.
    {
        let (bare0, bare1) = take(
            &mut r,
            "socketpair for the data-less handover succeeds",
            unix_pair(SocketType::STREAM),
        );
        let (q_r, q_w) = take(&mut r, "the pipe it will carry opens", rustix::pipe::pipe());
        check_is(
            &mut r,
            "a marker lands in the pipe before it is handed over",
            rustix::io::write(&q_w, b"Z").map_or(-1, |n| n as isize),
            1,
        );

        let mut cbuf = Ctrl::new();
        let clen = cbuf.one_fd(q_r.as_raw_fd());
        check_is(
            &mut r,
            "sendmsg with descriptors and no bytes is accepted",
            send_with_control(bare0.as_raw_fd(), b"", &mut cbuf, clen, 0),
            0,
        );

        let mut rbuf = [0u8; 8];
        let mut ctrl = Ctrl::new();
        if mpos_user_tests::host_run() {
            // Measured on a host Linux (`.workbuddy/probe_unix_scm.c`): a
            // data-less `SCM_RIGHTS` on a *stream* socketpair is accepted and
            // then queues nothing - a `recvmsg` with no `MSG_DONTWAIT` parks
            // there, which is exactly the state this kernel's fix took it out of.
            // So the delivery claims below are this kernel's own and a host run
            // does not make them; it makes the non-blocking read instead, so the
            // divergence is printed rather than hidden.
            let (again, _f, _l) = recv_with_control(
                bare1.as_raw_fd(),
                &mut rbuf,
                &mut ctrl,
                core::mem::size_of::<Ctrl>(),
                libc::MSG_DONTWAIT as usize,
            );
            println!(
                "note: the host queues nothing for a data-less SCM_RIGHTS and answers {again} \
                 with the descriptor unclaimed; this kernel delivers it"
            );
        } else {
            // Deliberately *no* MSG_DONTWAIT and a non-empty buffer: this is the
            // call that would park, so a hang here is the failure the check names.
            let (got, _f, _l) = recv_with_control(
                bare1.as_raw_fd(),
                &mut rbuf,
                &mut ctrl,
                core::mem::size_of::<Ctrl>(),
                0,
            );
            check_is(
                &mut r,
                "an empty read delivers the descriptors instead of parking",
                got,
                0,
            );
            let (_, level, ctype) = ctrl.hdr();
            r.check(
                "and it really is the SCM_RIGHTS record",
                level == libc::SOL_SOCKET && ctype == libc::SCM_RIGHTS,
            );
            let fd = own(ctrl.first_int());
            let mut probe = [0u8; 4];
            check_is(
                &mut r,
                "and it is the pipe end that was sent",
                rustix::io::read(&fd, &mut probe).map_or(-1, |n| n as isize),
                1,
            );
            r.check("holding the byte written before it", probe[0] == b'Z');

            // The record is one-shot: the next read has nothing to report and no
            // bytes to report it with, so the stream is simply empty.
            let mut rctrl2 = Ctrl::new();
            let (again, _f, _l) = recv_with_control(
                bare1.as_raw_fd(),
                &mut rbuf,
                &mut rctrl2,
                core::mem::size_of::<Ctrl>(),
                libc::MSG_DONTWAIT as usize,
            );
            check_is(
                &mut r,
                "a second read has no record of its own and does not park",
                again,
                refusal(Errno::AGAIN),
            );
        }
    }

    r.finish()
}
