//! t-unix-scm - the descriptors and identities a unix socket carries.
//!
//! Pinned here, per unix(7) and sendmsg(2): an `SCM_RIGHTS` handover moves a
//! live descriptor from one socket end to the other (the received descriptor
//! reads the same open file the sender held), a bad descriptor is `EBADF` at
//! the send, a control buffer too small for a handover answers `MSG_CTRUNC`
//! and closes the unclaimed descriptor, `MSG_CMSG_CLOEXEC` marks what arrives
//! close-on-exec, `SO_PEERCRED` reports the peer's pid/uid/gid captured at
//! connect, and a reader that set `SO_PASSCRED` gets those same credentials
//! back as an `SCM_CREDENTIALS` on the next read.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static SOCK: &[u8] = b"/tmp/t-unix-scm.sock\0";
static SOCK_TRIM: &[u8] = b"/tmp/t-unix-scm.sock";

const EBADF: isize = -9;
const F_GETFD: i32 = 1;
const FD_CLOEXEC: usize = 1;
/// `CMSG_HDRLEN`: a bare cmsghdr, no data - a control buffer this small cannot
/// hold a descriptor, so the next handover is truncated.
const CMSG_HDRLEN: usize = 16;

/// An 8-byte-aligned control area: a `cmsghdr` (16 bytes) then up to two
/// descriptor ints. The alignment is what `CMSG_ALIGN` walks on.
#[repr(C, align(8))]
struct Cbuf {
    w: [usize; 4],
}

impl Cbuf {
    const fn new() -> Cbuf {
        Cbuf { w: [0; 4] }
    }
    fn ptr(&mut self) -> usize {
        self.w.as_mut_ptr() as usize
    }
    /// Lay down one `SCM_RIGHTS` message carrying `fd`.
    unsafe fn one_fd(&mut self, fd: i32) -> usize {
        unsafe {
            let b = self.ptr() as *mut u8;
            core::ptr::write_bytes(b, 0, 32);
            (b as *mut usize).write_unaligned(lin::cmsg_len(4));
            (b.add(8) as *mut i32).write_unaligned(lin::SOL_SOCKET as i32);
            (b.add(12) as *mut i32).write_unaligned(lin::SCM_RIGHTS);
            (b.add(16) as *mut i32).write_unaligned(fd);
            lin::cmsg_space(4)
        }
    }
}

/// `sendmsg(fd, &one data byte, &[control], flags)` shaped for these checks.
fn send_with_control(fd: i32, data: &[u8], cbuf: &mut Cbuf, clen: usize, flags: usize) -> isize {
    let mut iov = [lin::IoVec {
        base: data.as_ptr() as usize,
        len: data.len(),
    }];
    let mut msg = lin::MsgHdr::default();
    msg.msg_iov = iov.as_mut_ptr() as usize;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.ptr();
    msg.msg_controllen = clen;
    lin::sendmsg(fd, &mut msg, flags)
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-unix-scm\n");
    let mut failed = false;

    // --- SCM_RIGHTS over a socketpair: hand a live pipe end across ---
    let mut sp = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"socketpair(AF_UNIX, SOCK_STREAM) succeeds",
        lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut sp),
        0,
    );
    let mut p = [-1i32; 2];
    lin::check_eq(
        &mut failed,
        b"pipe2 for the handover succeeds",
        lin::pipe2(&mut p, 0),
        0,
    );

    // Ship the read end of the pipe (p[0]) plus one marker byte.
    let mut cbuf = Cbuf::new();
    let clen = unsafe { cbuf.one_fd(p[0]) };
    let r = send_with_control(sp[0], b"x", &mut cbuf, clen, 0);
    lin::check_eq(&mut failed, b"sendmsg with SCM_RIGHTS sends the byte", r, 1);

    // Read it back on the far end: the byte, and one received descriptor.
    let mut rbuf = [0u8; 8];
    let mut rcbuf = Cbuf::new();
    {
        let mut iov = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = rcbuf.ptr();
        msg.msg_controllen = lin::cmsg_space(4);
        let got = lin::recvmsg(sp[1], &mut msg, 0);
        lin::check_eq(&mut failed, b"recvmsg reads the marker byte", got, 1);
        lin::check_eq(
            &mut failed,
            b"recvmsg reports one fd of control data",
            msg.msg_controllen as isize,
            lin::cmsg_space(4) as isize,
        );
    }
    let hdr = rcbuf.w.as_ptr() as *const lin::Cmsghdr;
    unsafe {
        lin::check_true(
            &mut failed,
            b"the control message is SCM_RIGHTS",
            (*hdr).cmsg_type == lin::SCM_RIGHTS && (*hdr).cmsg_level == lin::SOL_SOCKET as i32,
        );
    }
    let rf = unsafe { *((rcbuf.w.as_ptr() as *const u8).add(16) as *const i32) };
    lin::check_true(&mut failed, b"a descriptor number came back", rf >= 0);
    // The received descriptor names the SAME open pipe end: write to the far
    // half, read through what was handed over.
    let _ = lin::write(p[1], b"HELLO");
    let mut got = [0u8; 8];
    let n = lin::read(rf, &mut got);
    lin::check_eq(
        &mut failed,
        b"the passed descriptor reads the sender's pipe",
        n,
        5,
    );
    lin::check_true(
        &mut failed,
        b"and the bytes are the ones written",
        &got[..5] == b"HELLO",
    );

    // --- MSG_CMSG_CLOEXEC: a received descriptor can arrive close-on-exec ---
    let mut cbuf2 = Cbuf::new();
    let clen2 = unsafe { cbuf2.one_fd(p[0]) };
    let _ = send_with_control(sp[0], b"x", &mut cbuf2, clen2, 0);
    {
        let mut rbuf = [0u8; 8];
        let mut rcbuf = Cbuf::new();
        let mut iov = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = rcbuf.ptr();
        msg.msg_controllen = lin::cmsg_space(4);
        let got = lin::recvmsg(sp[1], &mut msg, lin::MSG_CMSG_CLOEXEC);
        lin::check_eq(&mut failed, b"MSG_CMSG_CLOEXEC recv reads the byte", got, 1);
        let fd = unsafe { *((rcbuf.w.as_ptr() as *const u8).add(16) as *const i32) };
        let fl = lin::fcntl(fd, F_GETFD, 0);
        lin::check_true(
            &mut failed,
            b"MSG_CMSG_CLOEXEC set FD_CLOEXEC on the received fd",
            fl >= 0 && (fl as usize & FD_CLOEXEC) != 0,
        );
        let _ = lin::close(fd);
    }

    // --- MSG_CTRUNC: a too-small control buffer drops the handover ---
    let mut cbuf3 = Cbuf::new();
    let clen3 = unsafe { cbuf3.one_fd(p[0]) };
    let _ = send_with_control(sp[0], b"x", &mut cbuf3, clen3, 0);
    {
        let mut rbuf = [0u8; 8];
        let mut rcbuf = Cbuf::new();
        let mut iov = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = rcbuf.ptr();
        // Room for the header only - not the one descriptor that would fit.
        msg.msg_controllen = CMSG_HDRLEN;
        let got = lin::recvmsg(sp[1], &mut msg, 0);
        lin::check_eq(
            &mut failed,
            b"a truncated read still returns the byte",
            got,
            1,
        );
        lin::check_true(
            &mut failed,
            b"a too-small control buffer sets MSG_CTRUNC",
            msg.msg_flags & lin::MSG_CTRUNC != 0,
        );
        lin::check_eq(
            &mut failed,
            b"and reports no control delivered",
            msg.msg_controllen as isize,
            0,
        );
    }

    // --- EBADF: a descriptor the sender does not hold is refused at send ---
    let mut cbad = Cbuf::new();
    let cblen = unsafe { cbad.one_fd(999) };
    let r = send_with_control(sp[0], b"x", &mut cbad, cblen, 0);
    lin::check_eq(
        &mut failed,
        b"sending an unknown descriptor is EBADF",
        r,
        EBADF,
    );

    // --- SO_PEERCRED + SO_PASSCRED across a real connect ---
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK, 0);
    let lfd = lin::socket(lin::AF_UNIX, lin::SOCK_STREAM, 0);
    lin::check_true(&mut failed, b"the listener socket opens", lfd >= 0);
    let (addr, alen) = lin::SockAddrUn::new(SOCK_TRIM);
    lin::check_eq(
        &mut failed,
        b"bind succeeds",
        lin::bind_un(lfd as i32, &addr, alen),
        0,
    );
    lin::check_eq(
        &mut failed,
        b"listen succeeds",
        lin::listen(lfd as i32, 4),
        0,
    );
    let cfd = lin::socket(lin::AF_UNIX, lin::SOCK_STREAM, 0);
    lin::check_true(&mut failed, b"the client socket opens", cfd >= 0);
    lin::check_eq(
        &mut failed,
        b"connect succeeds",
        lin::connect_un(cfd as i32, &addr, alen),
        0,
    );
    let sfd = lin::accept4_un(lfd as i32, core::ptr::null_mut(), core::ptr::null_mut(), 0);
    lin::check_true(&mut failed, b"accept returns the connected end", sfd >= 0);

    // The single process here is both peers, so every credential reads back as
    // its own pid and root's zero ids - proof the ids were captured at connect
    // and not left zero.
    let own = lin::getpid();
    let mut cred = [0u32; 3];
    lin::check_eq(
        &mut failed,
        b"getsockopt(SO_PEERCRED) succeeds on the accepted end",
        lin::getsockopt_cred(sfd as i32, &mut cred),
        0,
    );
    lin::check_true(
        &mut failed,
        b"SO_PEERCRED reports the peer pid",
        cred[0] as isize == own,
    );
    lin::check_true(
        &mut failed,
        b"SO_PEERCRED reports root's uid and gid",
        cred[1] == 0 && cred[2] == 0,
    );
    let mut ccred = [0u32; 3];
    lin::check_eq(
        &mut failed,
        b"getsockopt(SO_PEERCRED) succeeds on the client end",
        lin::getsockopt_cred(cfd as i32, &mut ccred),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the client sees the listener's pid too",
        ccred[0] as isize == own,
    );

    // SO_PASSCRED turns those same credentials into a per-message delivery.
    let one = 1i32;
    lin::check_eq(
        &mut failed,
        b"setsockopt(SO_PASSCRED) succeeds",
        lin::setsockopt(sfd as i32, lin::SOL_SOCKET, lin::SO_PASSCRED, &one),
        0,
    );
    let _ = lin::write(cfd as i32, b"p");
    {
        let mut rbuf = [0u8; 8];
        let mut rcbuf = Cbuf::new();
        let mut iov = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = rcbuf.ptr();
        msg.msg_controllen = lin::cmsg_space(12);
        let got = lin::recvmsg(sfd as i32, &mut msg, 0);
        lin::check_eq(&mut failed, b"the passcred read returns the byte", got, 1);
        let h = unsafe { &*(rcbuf.w.as_ptr() as *const lin::Cmsghdr) };
        lin::check_true(
            &mut failed,
            b"an SCM_CREDENTIALS message arrives",
            h.cmsg_type == lin::SCM_CREDENTIALS && h.cmsg_level == lin::SOL_SOCKET as i32,
        );
        let pid = unsafe { *((rcbuf.w.as_ptr() as *const u8).add(16) as *const u32) };
        lin::check_true(
            &mut failed,
            b"SCM_CREDENTIALS carries the peer pid",
            pid as isize == own,
        );
    }

    // --- the same credentials on a *datagram* pair, one per message ---
    //
    // eudev's manager does not learn that a worker finished from its exit
    // status. Each worker writes one datagram on a `SOCK_DGRAM` socketpair, and
    // the manager - which set `SO_PASSCRED` on its read end - takes the
    // sender's pid out of the `SCM_CREDENTIALS` the kernel attached. Two things
    // follow, and neither is carried by the stream case above.
    //
    // The credentials belong to the *message*: the pair was made before the
    // fork, so the socket's own peer is the manager, and answering
    // `SO_PEERCRED` here would tell every worker's reply that it came from the
    // process waiting for it. And `msg_controllen` must report the space the
    // kernel actually used - leave the caller's capacity standing over an empty
    // buffer and `CMSG_NXTHDR` answers `EINVAL`, udevd discards the reply, and
    // the event it was answering stays "running": every child device behind it
    // (`/dev/input/event0`, among them) waits for a parent that already
    // finished, which is the shape "the compositor starts but has no keyboard"
    // takes.
    {
        let mut dp = [0i32; 2];
        lin::check_eq(
            &mut failed,
            b"a SOCK_DGRAM socketpair opens",
            lin::socketpair(lin::AF_UNIX, lin::SOCK_DGRAM, 0, &mut dp),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"SO_PASSCRED on the reader succeeds",
            lin::setsockopt(dp[0], lin::SOL_SOCKET, lin::SO_PASSCRED, &one),
            0,
        );
        let mut pay = [0u8; 8];
        let mut diov = [lin::IoVec {
            base: pay.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut dcbuf = Cbuf::new();
        let mut dmsg = lin::MsgHdr::default();
        dmsg.msg_iov = diov.as_mut_ptr() as usize;
        dmsg.msg_iovlen = 1;
        dmsg.msg_control = dcbuf.ptr();
        dmsg.msg_controllen = lin::cmsg_space(12);
        lin::check_eq(
            &mut failed,
            b"an empty non-blocking datagram read is EAGAIN",
            lin::recvmsg(dp[0], &mut dmsg, lin::MSG_DONTWAIT),
            lin::EAGAIN,
        );
        lin::check_eq(
            &mut failed,
            b"the peer's write lands as one datagram",
            lin::write(dp[1], b"worker!!"),
            8,
        );
        lin::check_eq(
            &mut failed,
            b"the passcred datagram read returns it",
            lin::recvmsg(dp[0], &mut dmsg, lin::MSG_DONTWAIT),
            8,
        );
        let dh = unsafe { &*(dcbuf.w.as_ptr() as *const lin::Cmsghdr) };
        lin::check_true(
            &mut failed,
            b"the message carries SCM_CREDENTIALS",
            dh.cmsg_level == lin::SOL_SOCKET as i32 && dh.cmsg_type == lin::SCM_CREDENTIALS,
        );
        lin::check_true(
            &mut failed,
            b"its cmsg_len is CMSG_LEN(sizeof(struct ucred))",
            dh.cmsg_len == lin::cmsg_len(12),
        );
        let dpid = unsafe { *((dcbuf.w.as_ptr() as *const u8).add(16) as *const u32) };
        lin::check_true(
            &mut failed,
            b"those are the *sender's* credentials, not the socket's",
            dpid as isize == own,
        );
        lin::check_eq(
            &mut failed,
            b"msg_controllen reports the space used",
            dmsg.msg_controllen as isize,
            lin::cmsg_space(12) as isize,
        );
        // A reader that never asked gets the bytes and an *empty* chain: the
        // capacity it offered is not an answer from the kernel, and leaving it
        // standing is the `EINVAL` above.
        let mut np = [0i32; 2];
        lin::check_eq(
            &mut failed,
            b"a second SOCK_DGRAM pair opens without SO_PASSCRED",
            lin::socketpair(lin::AF_UNIX, lin::SOCK_DGRAM, 0, &mut np),
            0,
        );
        let mut niov = [lin::IoVec {
            base: pay.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut ncbuf = Cbuf::new();
        let mut nmsg = lin::MsgHdr::default();
        nmsg.msg_iov = niov.as_mut_ptr() as usize;
        nmsg.msg_iovlen = 1;
        nmsg.msg_control = ncbuf.ptr();
        nmsg.msg_controllen = lin::cmsg_space(12);
        lin::check_eq(
            &mut failed,
            b"the peer's write lands",
            lin::write(np[1], b"no-cred!"),
            8,
        );
        lin::check_eq(
            &mut failed,
            b"a reader without SO_PASSCRED still gets the datagram",
            lin::recvmsg(np[0], &mut nmsg, lin::MSG_DONTWAIT),
            8,
        );
        lin::check_eq(
            &mut failed,
            b"and its control chain is empty, not unfilled",
            nmsg.msg_controllen as isize,
            0,
        );
        let _ = lin::close(np[0]);
        let _ = lin::close(np[1]);
        let _ = lin::close(dp[0]);
        let _ = lin::close(dp[1]);
    }

    // --- the pair asked the way a compositor's launcher asks it ---
    //
    // `wl_client_create` takes one half of a `socketpair` and, before it will
    // own the descriptor, does three things to it: dups it close-on-exec, puts
    // the duplicate into an `epoll` interest list, and reads `SO_PEERCRED` off
    // the original. A pair that is not in the network registry failed the last
    // of those with `ENOTSOCK`, and weston lost every client it launches - so
    // this is that exact three-step sequence, in that order.
    let mut spcred = [0u32; 3];
    lin::check_eq(
        &mut failed,
        b"SO_PEERCRED succeeds on a socketpair end",
        lin::getsockopt_cred(sp[0], &mut spcred),
        0,
    );
    lin::check_true(
        &mut failed,
        b"a socketpair reports its creator's pid as the peer",
        spcred[0] as isize == own,
    );
    // `F_DUPFD_CLOEXEC`, the one libwayland uses to keep the duplicate out of
    // a child's way.
    let dup = lin::fcntl(sp[0], 1030, 0);
    lin::check_true(&mut failed, b"the end dups close-on-exec", dup >= 0);
    let epfd = lin::epoll_create1(0) as i32;
    lin::check_true(&mut failed, b"an interest list is made", epfd >= 0);
    let mut ev = lin::EpollEvent {
        events: lin::EPOLLIN,
        data: 0,
    };
    lin::check_eq(
        &mut failed,
        b"epoll watches the dup of a socketpair end",
        lin::epoll_ctl(epfd, lin::EPOLL_CTL_ADD, dup as i32, &mut ev),
        0,
    );

    // And the two knobs the same caller reads beside it: what the channel is,
    // and whether `SO_PASSCRED` round-trips through the pair rather than
    // falling through to a registry that does not know it.
    let mut typ = 0i32;
    lin::check_eq(
        &mut failed,
        b"SO_TYPE on a socketpair reads SOCK_STREAM",
        lin::getsockopt_int(sp[1], lin::SOL_SOCKET, lin::SO_TYPE, &mut typ),
        0,
    );
    lin::check_true(
        &mut failed,
        b"the pair calls itself a stream socket",
        typ == lin::SOCK_STREAM as i32,
    );
    let on = 1i32;
    lin::check_eq(
        &mut failed,
        b"setsockopt(SO_PASSCRED) succeeds on a socketpair end",
        lin::setsockopt(sp[1], lin::SOL_SOCKET, lin::SO_PASSCRED, &on),
        0,
    );
    let mut back = 0i32;
    lin::check_eq(
        &mut failed,
        b"SO_PASSCRED reads back on a socketpair end",
        lin::getsockopt_int(sp[1], lin::SOL_SOCKET, lin::SO_PASSCRED, &mut back),
        0,
    );
    lin::check_true(&mut failed, b"the pair remembers the flag", back == 1);

    let _ = lin::close(sp[0]);
    let _ = lin::close(sp[1]);
    let _ = lin::close(p[0]);
    let _ = lin::close(p[1]);
    let _ = lin::close(lfd as i32);
    let _ = lin::close(cfd as i32);
    let _ = lin::close(sfd as i32);
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK, 0);

    // --- one control chain per send, not one per read ---
    //
    // A stream has no message boundaries of its own, so handing a descriptor
    // back with *its* bytes means knowing where each send ended. Two one-byte
    // sends, each carrying a different pipe, are read by a caller with room for
    // both at once: they must still arrive as two deliveries, and which pipe came
    // with which byte is what shows the pairing is real rather than whatever
    // order the kernel happened to pop its queue in.
    {
        let mut sp = [0i32; 2];
        lin::check_eq(
            &mut failed,
            b"a stream pair for the boundary check",
            lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut sp),
            0,
        );
        let mut first = [0i32; 2];
        let mut second = [0i32; 2];
        lin::check_eq(
            &mut failed,
            b"the first pipe exists",
            lin::pipe2(&mut first, 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"the second pipe exists",
            lin::pipe2(&mut second, 0),
            0,
        );
        let _ = lin::write(first[1], b"ONE");
        let _ = lin::write(second[1], b"TWO");

        let mut cb = Cbuf::new();
        // Two separate payloads rather than one overwritten: the iovec is a
        // pointer, and a test that mutates through it proves nothing about what
        // the second send actually carried.
        let mut pay_a = [b'A'];
        let mut iov_a = [lin::IoVec {
            base: pay_a.as_mut_ptr() as usize,
            len: 1,
        }];
        let mut sm = lin::MsgHdr::default();
        sm.msg_iov = iov_a.as_mut_ptr() as usize;
        sm.msg_iovlen = 1;
        sm.msg_control = cb.ptr();
        sm.msg_controllen = unsafe { cb.one_fd(first[0]) };
        lin::check_eq(
            &mut failed,
            b"the first byte goes with the first pipe",
            lin::sendmsg(sp[0], &mut sm, 0),
            1,
        );
        let mut pay_b = [b'B'];
        let mut iov_b = [lin::IoVec {
            base: pay_b.as_mut_ptr() as usize,
            len: 1,
        }];
        sm.msg_iov = iov_b.as_mut_ptr() as usize;
        sm.msg_controllen = unsafe { cb.one_fd(second[0]) };
        lin::check_eq(
            &mut failed,
            b"and the second byte with the second",
            lin::sendmsg(sp[0], &mut sm, 0),
            1,
        );

        // Room for far more than one send's worth, so a kernel without
        // boundaries would happily take both in one go.
        let mut got = [0u8; 16];
        let mut rbuf = [0usize; 4];
        for (i, want) in [(0u8, b'A'), (1u8, b'B')] {
            let mut riov = [lin::IoVec {
                base: got.as_mut_ptr() as usize,
                len: 16,
            }];
            let mut rm = lin::MsgHdr::default();
            rm.msg_iov = riov.as_mut_ptr() as usize;
            rm.msg_iovlen = 1;
            rm.msg_control = rbuf.as_mut_ptr() as usize;
            rm.msg_controllen = core::mem::size_of_val(&rbuf);
            let n = lin::recvmsg(sp[1], &mut rm, 0);
            // One byte, not two: the read stopped at the boundary the descriptor
            // it carried belonged to.
            lin::check_eq(&mut failed, b"a read stops at one send's end", n, 1);
            lin::check_true(
                &mut failed,
                b"and the byte is the one that send wrote",
                got[0] == want,
            );
            let h = unsafe { &*(rbuf.as_ptr() as *const lin::Cmsghdr) };
            lin::check_true(
                &mut failed,
                b"with an SCM_RIGHTS chain of its own",
                h.cmsg_level == lin::SOL_SOCKET as i32 && h.cmsg_type == lin::SCM_RIGHTS,
            );
            let fd = unsafe { *((rbuf.as_ptr() as *const u8).add(16) as *const i32) };
            let mut probe = [0u8; 4];
            let r = lin::read(fd, &mut probe);
            lin::check_eq(
                &mut failed,
                b"and it is the descriptor that went with it",
                r,
                3,
            );
            let expect: &[u8] = if i == 0 { b"ONE" } else { b"TWO" };
            lin::check_true(
                &mut failed,
                b"named by what it holds",
                &probe[..3] == expect,
            );
            let _ = lin::close(fd);
        }

        let _ = lin::close(sp[0]);
        let _ = lin::close(sp[1]);
        let _ = lin::close(first[0]);
        let _ = lin::close(first[1]);
        let _ = lin::close(second[0]);
        let _ = lin::close(second[1]);
    }

    // --- a handover that carries no bytes at all ---
    //
    // `sendmsg` with descriptors and an empty iovec is a legal message, and the
    // record it queues ends exactly where the stream already is: it is due the
    // moment it is queued, and no byte will ever arrive *with* it. A read that
    // waited for one would wait forever with the descriptors already in hand,
    // so the read answers with the record and a zero-byte return instead. The
    // check that matters is that this returns at all - before the fix it parked
    // the machine until the harness gave up.
    {
        let mut bare = [-1i32; 2];
        lin::check_eq(
            &mut failed,
            b"socketpair for the data-less handover succeeds",
            lin::socketpair(lin::AF_UNIX, lin::SOCK_STREAM, 0, &mut bare),
            0,
        );
        let mut q = [-1i32; 2];
        lin::check_eq(
            &mut failed,
            b"the pipe it will carry opens",
            lin::pipe2(&mut q, 0),
            0,
        );
        lin::check_eq(
            &mut failed,
            b"a marker lands in the pipe before it is handed over",
            lin::write(q[1], b"Z"),
            1,
        );

        let mut cbuf = Cbuf::new();
        let clen = unsafe { cbuf.one_fd(q[0]) };
        let r = send_with_control(bare[0], b"", &mut cbuf, clen, 0);
        lin::check_eq(
            &mut failed,
            b"sendmsg with descriptors and no bytes is accepted",
            r,
            0,
        );

        let mut rbuf = [0u8; 8];
        let mut rcbuf = Cbuf::new();
        let mut iov = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg = lin::MsgHdr::default();
        msg.msg_iov = iov.as_mut_ptr() as usize;
        msg.msg_iovlen = 1;
        msg.msg_control = rcbuf.ptr();
        msg.msg_controllen = core::mem::size_of_val(&rcbuf);
        // Deliberately *no* MSG_DONTWAIT and a non-empty buffer: this is the
        // call that would park, so a hang here is the failure the check names.
        let got = lin::recvmsg(bare[1], &mut msg, 0);
        lin::check_eq(
            &mut failed,
            b"an empty read delivers the descriptors instead of parking",
            got,
            0,
        );
        let h = unsafe { &*(rcbuf.ptr() as *const lin::Cmsghdr) };
        lin::check_true(
            &mut failed,
            b"and it really is the SCM_RIGHTS record",
            h.cmsg_level == lin::SOL_SOCKET as i32 && h.cmsg_type == lin::SCM_RIGHTS,
        );
        let fd = unsafe { *((rcbuf.ptr() as *const u8).add(16) as *const i32) };
        lin::check_true(&mut failed, b"a descriptor number came back", fd >= 0);
        let mut probe = [0u8; 4];
        lin::check_eq(
            &mut failed,
            b"and it is the pipe end that was sent",
            lin::read(fd, &mut probe),
            1,
        );
        lin::check_true(
            &mut failed,
            b"holding the byte written before it",
            probe[0] == b'Z',
        );

        // The record is one-shot: the next read has nothing to report and no
        // bytes to report it with, so the stream is simply empty.
        let mut iov2 = [lin::IoVec {
            base: rbuf.as_mut_ptr() as usize,
            len: 8,
        }];
        let mut msg2 = lin::MsgHdr::default();
        msg2.msg_iov = iov2.as_mut_ptr() as usize;
        msg2.msg_iovlen = 1;
        msg2.msg_control = rcbuf.ptr();
        msg2.msg_controllen = core::mem::size_of_val(&rcbuf);
        let again = lin::recvmsg(bare[1], &mut msg2, lin::MSG_DONTWAIT);
        lin::check_eq(
            &mut failed,
            b"a second read has no record of its own and does not park",
            again,
            lin::EAGAIN,
        );

        let _ = lin::close(fd);
        let _ = lin::close(bare[0]);
        let _ = lin::close(bare[1]);
        let _ = lin::close(q[1]);
    }

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-unix-scm: panicked\n")
}
