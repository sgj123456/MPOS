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

    let _ = lin::close(sp[0]);
    let _ = lin::close(sp[1]);
    let _ = lin::close(p[0]);
    let _ = lin::close(p[1]);
    let _ = lin::close(lfd as i32);
    let _ = lin::close(cfd as i32);
    let _ = lin::close(sfd as i32);
    let _ = lin::unlinkat(lin::AT_FDCWD, SOCK, 0);

    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-unix-scm: panicked\n")
}
