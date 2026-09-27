//! t-udev-worker - the socket shape udevd's worker channel needs.
//!
//! The reason this test exists is a line in the kernel log of every boot of the
//! desktop image:
//!
//! ```text
//! udevd[9]: failed to receive message: Invalid argument
//! ```
//!
//! repeated for every message a worker sends, which is why no device ever gets
//! an `ID_INPUT` property, why libinput passes the keyboard by, and why weston
//! has been started here with `require-input=false` - a compositor with a
//! display and no input. The string comes out of `udevd.c`'s worker receive, and
//! the neighbouring strings in the same binary (`ignoring worker message with
//! invalid size`, `ignoring worker message without valid PID`) say what happens
//! *after* a receive that worked: so the failure is the `recvmsg(2)` itself, on
//! the `SOCK_SEQPACKET` socketpair the manager makes before it forks any worker.
//!
//! That call has a shape, and a test that copies anything else passes without
//! covering it: no `msg_name`, one iovec the size of the worker's message
//! struct, a control buffer of `CMSG_SPACE(sizeof(struct ucred))`, and
//! `SO_PASSCRED` set on the receiving end so the kernel attaches the sender's
//! credentials. The pair is created with `SOCK_CLOEXEC|SOCK_NONBLOCK` folded
//! into the type word, which is how every modern caller writes it, and the type
//! itself is neither `SOCK_STREAM` nor `SOCK_DGRAM` - the two shapes a socket
//! layer is most likely to have to hand.
//!
//! Each case below is one part of that shape, and each names itself on failure,
//! because "udevd stops working" is a long way from "a seqpacket receive with no
//! `msg_name` answered `EINVAL`".

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const SOCK_SEQPACKET: usize = 5;
/// `POLLIN`, as `poll` numbers it: a descriptor is worth a wake when something
/// stands in its queue.
const POLLIN: i16 = 0x0001;
/// The two flag bits a caller may fold into the type word of `socketpair`.
const SOCK_NONBLOCK: usize = 0o4000;
const SOCK_CLOEXEC: usize = 0o2000000;
/// `sizeof(struct ucred)`.
const UCRED: usize = 12;
/// `MSG_TRUNC`, in the flags word a receive leaves behind: the datagram was
/// longer than the caller's buffer and the rest of it is gone.
const MSG_TRUNC: usize = 0x20;
/// The worker message the real caller sizes its iovec to. The contents do not
/// matter to the kernel; the *length* is what the receive is measured against.
const MSG_LEN: usize = 24;

/// Receive one message the way the manager does: no peer name asked for, a
/// caller-sized iovec, and room for the credentials. Returns the receive's
/// result with what the kernel said about the control block afterwards.
fn worker_recv(fd: i32, buf: &mut [u8], ctl: &mut [u8]) -> (isize, usize, usize) {
    let mut iov = lin::Iovec {
        base: buf.as_mut_ptr(),
        len: buf.len(),
    };
    let mut msg = lin::MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad: 0,
        msg_iov: core::ptr::addr_of_mut!(iov) as usize,
        msg_iovlen: 1,
        msg_control: ctl.as_mut_ptr() as usize,
        msg_controllen: ctl.len(),
        msg_flags: 0,
    };
    let rc = lin::recvmsg(fd, &mut msg, 0);
    (rc, msg.msg_controllen, msg.msg_flags)
}

/// Send one message from the other end, with nothing attached to it.
fn worker_send(fd: i32, data: &[u8]) -> isize {
    let mut iov = lin::Iovec {
        base: data.as_ptr() as *mut u8,
        len: data.len(),
    };
    let mut msg = lin::MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad: 0,
        msg_iov: core::ptr::addr_of_mut!(iov) as usize,
        msg_iovlen: 1,
        msg_control: 0,
        msg_controllen: 0,
        msg_flags: 0,
    };
    lin::sendmsg(fd, &mut msg, 0)
}

/// The one control record a `SO_PASSCRED` receiver must be handed: level, type
/// and length all say "the sender's credentials".
fn sole_credentials(ctl: &[u8], controllen: usize) -> bool {
    if controllen < lin::cmsg_space(UCRED) {
        return false;
    }
    let head = unsafe { &*(ctl.as_ptr() as *const lin::Cmsghdr) };
    if head.cmsg_len != lin::cmsg_len(UCRED)
        || head.cmsg_level != lin::SOL_SOCKET as i32
        || head.cmsg_type != lin::SCM_CREDENTIALS
    {
        return false;
    }
    // struct ucred { pid_t pid; uid_t uid; gid_t gid; } straight after the
    // header's padding - the kernel's own pid and uid 0 here, because the
    // sender is this process and this process runs as root.
    let cred = unsafe { &*(ctl.as_ptr().add(lin::cmsg_align(16)) as *const [u32; 3]) };
    cred[1] == 0 && cred[2] == 0
}

#[unsafe(no_mangle)]
pub extern "C" fn mpos_user_main(_argc: i32, _argv: *const *const u8) -> ! {
    lin::puts(b"t-udev-worker\n");
    let mut failed = false;

    // The pair as the manager makes it: seqpacket, with the two flag bits folded
    // into the type the way `socketpair` is called in every modern program.
    let mut sv = [0i32; 2];
    let opened = lin::socketpair(
        lin::AF_UNIX,
        SOCK_SEQPACKET | SOCK_NONBLOCK | SOCK_CLOEXEC,
        0,
        &mut sv,
    );
    lin::check_eq(
        &mut failed,
        b"a seqpacket pair opens with CLOEXEC|NONBLOCK folded into the type",
        opened,
        0,
    );
    if opened != 0 {
        lin::done(failed);
    }

    // SO_PASSCRED is what makes the kernel attach the sender's credentials to
    // every message this end receives; without it the worker protocol has no way
    // to tell a real worker from another process that guessed the socket.
    lin::check_eq(
        &mut failed,
        b"SO_PASSCRED is accepted on the receiving end",
        lin::setsockopt_int(sv[0], lin::SOL_SOCKET, lin::SO_PASSCRED, 1),
        0,
    );

    // The message the worker would send: one fixed-size struct, no control data.
    let payload = [0xa5u8; MSG_LEN];
    lin::check_eq(
        &mut failed,
        b"the worker end's message is queued whole",
        worker_send(sv[1], &payload),
        MSG_LEN as isize,
    );

    let mut buf = [0u8; 64];
    let mut ctl = [0u8; lin::cmsg_space(UCRED) + 8];
    let (rc, controllen, flags) = worker_recv(sv[0], &mut buf, &mut ctl);
    lin::check_eq(
        &mut failed,
        b"the manager's recvmsg delivers it (EINVAL here is the boot-log bug)",
        rc,
        MSG_LEN as isize,
    );
    if rc != MSG_LEN as isize {
        lin::puts(b"      recvmsg answered ");
        lin::puts_isize(rc);
        lin::puts(b"\n");
    }
    lin::check_true(
        &mut failed,
        b"and the sender's credentials came attached to it",
        rc == MSG_LEN as isize && sole_credentials(&ctl, controllen),
    );
    lin::check_true(
        &mut failed,
        b"with nothing truncated about the delivery",
        flags & (lin::MSG_CTRUNC | MSG_TRUNC) == 0,
    );
    lin::check_true(
        &mut failed,
        b"and the bytes themselves intact",
        rc == MSG_LEN as isize && buf[..MSG_LEN] == payload[..],
    );

    // A second message, received by a caller that asked for no control room at
    // all. Linux still delivers the bytes and simply drops the credentials the
    // receiver could not have held: a kernel that answered `EINVAL` for an
    // absent control buffer would work for udevd and break everything else.
    let _ = worker_send(sv[1], &payload);
    let mut bare = lin::MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad: 0,
        msg_iov: 0,
        msg_iovlen: 0,
        msg_control: 0,
        msg_controllen: 0,
        msg_flags: 0,
    };
    let mut iov = lin::Iovec {
        base: buf.as_mut_ptr(),
        len: buf.len(),
    };
    bare.msg_iov = core::ptr::addr_of_mut!(iov) as usize;
    bare.msg_iovlen = 1;
    lin::check_eq(
        &mut failed,
        b"a receive with no control buffer still returns the message",
        lin::recvmsg(sv[0], &mut bare, 0),
        MSG_LEN as isize,
    );

    // An iovec shorter than the message: a datagram is delivered *truncated*
    // rather than refused, with `MSG_TRUNC` to say so, and the next message is
    // still whole. A worker struct that grows on one side of the pair and not
    // the other is an ordinary thing, and this is what the manager sees then.
    let _ = worker_send(sv[1], &payload);
    let mut short_buf = [0u8; 8];
    let mut short_ctl = [0u8; lin::cmsg_space(UCRED) + 8];
    let (rc, _, flags) = worker_recv(sv[0], &mut short_buf, &mut short_ctl);
    lin::check_eq(
        &mut failed,
        b"a short iovec gets the front of the datagram, not an error",
        rc,
        short_buf.len() as isize,
    );
    lin::check_true(
        &mut failed,
        b"and MSG_TRUNC says the rest of that one message was dropped",
        flags & MSG_TRUNC != 0,
    );

    // And the blocking shape, which is the one the *manager's* main loop uses on
    // a machine where the pair was made without O_NONBLOCK: the receive waits
    // for the worker instead of answering `EAGAIN`, and returns the message when
    // it lands. A caller that gets `EAGAIN` where it expected to sleep spins;
    // one that gets `EINVAL` never sees the worker at all.
    //
    // This is bounded by a `poll` timeout rather than by blocking in the
    // receive itself, and that is not caution for its own sake: the first
    // version of this case blocked, and when the send side was broken the test
    // hung instead of failing - the same shape of mistake that stopped a whole
    // suite run at a `ptrace` test, where a stalled run looks like a slow one
    // until somebody counts the minutes.
    let mut slow = [0i32; 2];
    let opened = lin::socketpair(lin::AF_UNIX, SOCK_SEQPACKET, 0, &mut slow);
    lin::check_eq(&mut failed, b"a blocking seqpacket pair opens", opened, 0);
    if opened == 0 {
        let mut fd = [lin::PollFd {
            fd: slow[0],
            events: POLLIN,
            revents: 0,
        }];
        lin::check_eq(
            &mut failed,
            b"and nothing is readable before the worker has sent",
            lin::poll(&mut fd, 0),
            0,
        );
        let writer = lin::fork();
        if writer == 0 {
            let nap = lin::TimeSpec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            let mut rem = nap;
            let _ = lin::nanosleep(&nap, &mut rem);
            let _ = worker_send(slow[1], &payload);
            lin::exit(0);
        }
        // Five seconds is longer than the writer's second, and shorter than the
        // patience of anyone watching a test run.
        let woke = lin::poll(&mut fd, 5000);
        lin::check_true(
            &mut failed,
            b"a waiting reader is woken by the message rather than timing out",
            woke > 0,
        );
        let mut got = [0u8; 64];
        let mut ctl = [0u8; lin::cmsg_space(UCRED) + 8];
        let (rc, _, _) = worker_recv(slow[0], &mut got, &mut ctl);
        lin::check_eq(
            &mut failed,
            b"and the receive that follows takes it whole",
            rc,
            MSG_LEN as isize,
        );
        let mut status = 0i32;
        let _ = lin::waitpid(writer as usize, &mut status, 0);
    }

    let _ = lin::close(sv[0]);
    let _ = lin::close(sv[1]);
    let _ = lin::close(slow[0]);
    let _ = lin::close(slow[1]);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-udev-worker: panicked\n")
}
