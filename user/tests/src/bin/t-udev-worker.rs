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
//! *after* a receive that worked: so the failure is the `recvmsg(2)` itself.
//!
//! **The shape of that call, read off eudev's own source** (`udevd.c`, where
//! `worker_watch` is made and `worker_returned` reads it):
//!
//! ```text
//! socketpair(AF_LOCAL, SOCK_DGRAM|SOCK_CLOEXEC, 0, worker_watch)
//! setsockopt(worker_watch[READ_END], SOL_SOCKET, SO_PASSCRED, &one, sizeof(one))
//! struct worker_message {};                    /* no members: sizeof == 0 */
//! loop_write(worker_watch[WRITE_END], &message, sizeof(message), false)
//! recvmsg(fd_worker, &msg, MSG_DONTWAIT)       /* iov_len == 0, CMSG_SPACE(ucred) */
//! ```
//!
//! So the worker's message is **zero bytes** - the credentials *are* the
//! payload, which is the classic `SCM_CREDENTIALS` use of a message that carries
//! nothing - and the manager's receive offers an iovec whose length is zero. A
//! kernel that answered `EINVAL` for a zero-length data room therefore answered
//! `EINVAL` for every worker return, left udevd's loop stuck on the failing
//! read, and cost the machine every device tag it has.
//!
//! The first section below is that shape exactly, because it is the one that
//! was wrong. The rest is the family around it, and none of it is decoration: a
//! receive with **no room for the credentials** must still take the message and
//! report `MSG_CTRUNC`, a zero-length buffer against a **non-empty** message
//! must take it and report `MSG_TRUNC` (the message's own length is its
//! boundary, not the caller's buffer), `read(2)` with a zero-length buffer must
//! leave the queue alone (`vfs_read`'s short-circuit) while `recv(2)` with one
//! must consume, and the credentials' `pid` must be the *sender's* - the manager
//! looks its worker up by that number.
//!
//! The seqpacket cases at the end cover the other channel udevd opens, its
//! control socket: a named `SOCK_SEQPACKET` connection carries whole messages
//! (`t-unix-seqpacket`), and a paired one does too. Both are shapes a socket
//! layer is likely to have to hand, and neither is what the worker channel
//! uses.
//!
//! # The migration
//!
//! Every door is `rustix`'s: `net::socketpair` for the pairs (the type and the
//! `SOCK_*` flags as two words where the no-std suite folded them into one - the
//! pair the kernel is asked for is the same bits), `net::sockopt::
//! set_socket_passcred` for `SO_PASSCRED`, `net::recvmsg` with a
//! `RecvAncillaryBuffer` and `net::sendmsg` with a `SendAncillaryBuffer` for the
//! messages, `event::poll` for the two waits, `io::{read, write}` for the
//! byte-shaped doors, and `process::waitpid` for the reaps. `libc::fork` keeps
//! its own door for batch 18's reason (`rustix` binds no `fork`), and the child
//! half leaves through `libc::_exit` so it cannot run the parent's remaining
//! claims from a child's body.
//!
//! Two claims change their *door* and neither changes its meaning:
//!
//!   * **The bytes, the flags and the credentials.** Those are what `recvmsg`
//!     answers, and the crate reports all three: `RecvMsg::bytes`,
//!     `RecvMsg::flags` (whose `TRUNC` and `CTRUNC` bits *are* the kernel's
//!     `MSG_*` word), and `RecvAncillaryMessage::ScmCredentials`, the message
//!     the crate parses out of the chain. `got_bytes` below answers a failed
//!     receive with its *negative errno*, so a claim that names `EINVAL` still
//!     reports what arrived. The exact header arithmetic - `cmsg_len ==
//!     CMSG_LEN(12)`, the level, the type, `msg_controllen == CMSG_SPACE(12)` -
//!     is `t-unix-scm`'s claim, pinned there through the raw `libc::recvmsg`
//!     door for the reason that file gives; what this program needs is covered
//!     by the parse, since a chain whose level, type or length the kernel wrote
//!     wrong is not classified as credentials at all.
//!   * **The seqpacket pair's own flags.** Its claim says "CLOEXEC|NONBLOCK
//!     folded into the type", which the no-std version made on the `socketpair`
//!     return alone; here the two flags are read back off the descriptor they
//!     were asked for (`F_GETFD` and `F_GETFL`), so the claim is what its name
//!     says rather than what the call was given.
//!
//! Everything else is the no-std program's claim set unchanged, and this family
//! has no mpos-only divergence, so the program is **run on the host too**
//! (`MPOS_ON_HOST=1`): the host run is the cross-check that each answer is
//! Linux's rather than this kernel's own.

use std::io::IoSlice;
use std::mem::MaybeUninit;
use std::time::Duration;

use mpos_user_tests::Report;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::OwnedFd;
use rustix::fs::{OFlags, fcntl_getfl};
use rustix::io::{Errno, FdFlags, IoSliceMut, fcntl_getfd, read, write};
use rustix::net::sockopt::set_socket_passcred;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, RecvMsg, ReturnFlags,
    SendAncillaryBuffer, SendFlags, SocketFlags, SocketType, UCred, recvmsg, sendmsg, socketpair,
};
use rustix::process::{Pid, WaitOptions, waitpid};

/// The worker message the real caller sizes its iovec to. The contents do not
/// matter to the kernel; the *length* is what the receive is measured against.
const MSG_LEN: usize = 24;

/// A wait that asks the question and does not wait for it.
const NOW: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// One claim about a number a call answered, so a failure says what arrived
/// instead of only that it was not what was wanted.
fn check_is(r: &mut Report, name: &str, got: isize, want: isize) {
    if got == want {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got}, want {want})"), false);
    }
}

/// A fixture claim: if it did not hold there is nothing to test, so the program
/// says which one and stops rather than reporting a shorter list of claims that
/// all passed. `None` is that failure, and the caller ends the report.
fn take<T>(r: &mut Report, name: &str, got: Result<T, Errno>) -> Option<T> {
    match got {
        Ok(value) => {
            r.check(name, true);
            Some(value)
        }
        Err(err) => {
            r.check(&format!("{name} (got {err})"), false);
            None
        }
    }
}

/// What a receive answered, as a number: its byte count, or its *negative errno*
/// when it failed. The family's whole story is which of those two a call gives
/// back, so the claims below are written against one number either way.
fn got_bytes(received: &Result<RecvMsg, Errno>) -> isize {
    match received {
        Ok(msg) => msg.bytes as isize,
        Err(err) => -(err.raw_os_error() as isize),
    }
}

/// The flags a receive left in the header - the kernel's `MSG_*` answer word,
/// empty when the call failed.
fn got_flags(received: &Result<RecvMsg, Errno>) -> ReturnFlags {
    received
        .as_ref()
        .map(|msg| msg.flags)
        .unwrap_or(ReturnFlags::empty())
}

/// One receive, shaped the way a manager makes it: no peer name asked for, an
/// iovec whose length the caller chooses - *zero* for the worker protocol, where
/// the message has no bytes - and however much room the caller offers for the
/// credentials. Answers the receive's own result and the credentials the kernel
/// attached, if any.
fn manager_recv(
    fd: &OwnedFd,
    body: &mut [u8],
    iov_len: usize,
    space: &mut [MaybeUninit<u8>],
    flags: RecvFlags,
) -> (Result<RecvMsg, Errno>, Option<UCred>) {
    let mut control = RecvAncillaryBuffer::new(space);
    // One iovec, present and *length zero*: exactly eudev's call, whose
    // `iov_len` is the size of an empty struct.
    let mut iovs = [IoSliceMut::new(&mut body[..iov_len])];
    let received = recvmsg(fd, &mut iovs, &mut control, flags);
    // Drained after the call: the crate walks the chain the kernel wrote and
    // hands back the message types it recognises, which is where the pid is.
    let cred = control.drain().find_map(|message| match message {
        RecvAncillaryMessage::ScmCredentials(cred) => Some(cred),
        // The worker channel carries credentials and nothing else, so anything
        // else in the chain is not the message this program is about.
        _ => None,
    });
    (received, cred)
}

/// Send one message from the worker's end with nothing attached to it - the
/// shape `loop_write` gives the manager: one iovec, an empty control chain.
fn worker_send(fd: &OwnedFd, data: &[u8]) -> Result<usize, Errno> {
    sendmsg(
        fd,
        &[IoSlice::new(data)],
        &mut SendAncillaryBuffer::default(),
        SendFlags::empty(),
    )
}

/// Whether `cred` is this process's own, the way a manager reads a message from
/// a worker that is the process that made the pair.
fn is_own(cred: &UCred) -> bool {
    is_from(cred, std::process::id() as i32)
}

/// Whether `cred` says the message was sent by process `pid` - which is the
/// whole protocol, the manager looking its worker up by this number - by root,
/// which is what this process runs as.
fn is_from(cred: &UCred, pid: i32) -> bool {
    cred.pid.as_raw_pid() == pid && cred.uid.as_raw() == 0 && cred.gid.as_raw() == 0
}

fn main() {
    println!("t-udev-worker");
    let mut r = Report::new();

    // ---------------------------------------------------------------------
    // First: the shape the shipped eudev uses, which is the one the boot log
    // was complaining about. A `SOCK_DGRAM` pair, `SO_PASSCRED` on the
    // manager's end, a worker whose whole message is zero bytes, and a
    // manager receive offering a zero-length iovec.
    // ---------------------------------------------------------------------
    let Some((manager, worker)) = take(
        &mut r,
        "the worker channel is a SOCK_DGRAM|SOCK_CLOEXEC pair",
        socketpair(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        ),
    ) else {
        r.finish()
    };

    r.check(
        "SO_PASSCRED is accepted on the manager's end",
        set_socket_passcred(&manager, true).is_ok(),
    );

    // The worker's message, byte for byte: `struct worker_message` has no
    // members, so this is a zero-length `write(2)` - and it must still be a
    // message, not a write that did nothing.
    r.check(
        "a worker's zero-byte message is accepted",
        write(&worker, b"") == Ok(0),
    );
    let mut readable = [PollFd::new(&manager, PollFlags::IN)];
    r.check(
        "and the manager's end reports it readable",
        poll(&mut readable, Some(&NOW)).is_ok_and(|n| n == 1),
    );

    let mut body = [0u8; 8];
    let mut cred_space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmCredentials(1))];
    let (received, cred) =
        manager_recv(&manager, &mut body, 0, &mut cred_space, RecvFlags::DONTWAIT);
    check_is(
        &mut r,
        "the manager's zero-length receive answers 0, not EINVAL",
        got_bytes(&received),
        0,
    );
    r.check(
        "with the worker's credentials attached to it",
        cred.as_ref().is_some_and(is_own),
    );
    r.check(
        "and nothing truncated about a message that held no bytes",
        !got_flags(&received).intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC),
    );
    // eudev's loop reads until the queue says `EAGAIN`, so that is the answer
    // the second call must give - a kernel that had left the message queued
    // would spin here exactly as udevd did.
    let (again, _) = manager_recv(&manager, &mut body, 0, &mut cred_space, RecvFlags::DONTWAIT);
    check_is(
        &mut r,
        "the message was consumed, so the next receive is EAGAIN",
        got_bytes(&again),
        -(Errno::AGAIN.raw_os_error() as isize),
    );

    // The pid is the whole protocol: the manager finds its worker by the
    // sender's `ucred.pid`. A forked worker is the case that matters, because
    // the sender is then not the process that made the pair.
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child half says nothing and leaves through `_exit`: its exit
        // status is the claim, and nothing of this process's buffered output is
        // the parent's to flush from here.
        let queued = write(&worker, b"");
        unsafe { libc::_exit(queued.map_or(1, |_| 0)) };
    }
    let _ = waitpid(Pid::from_raw(child), WaitOptions::empty());
    let (received, cred) =
        manager_recv(&manager, &mut body, 0, &mut cred_space, RecvFlags::DONTWAIT);
    r.check(
        "a forked worker's reply carries its own pid, not the manager's",
        got_bytes(&received) == 0 && cred.as_ref().is_some_and(|c| is_from(c, child)),
    );

    // A non-empty message against the same zero-length iovec: taken, and
    // reported as truncated rather than refused. The message's own length
    // is its boundary.
    let three = [1u8, 2, 3];
    let _ = worker_send(&worker, &three);
    let (received, cred) =
        manager_recv(&manager, &mut body, 0, &mut cred_space, RecvFlags::DONTWAIT);
    check_is(
        &mut r,
        "a zero-length buffer still takes a message that has bytes",
        got_bytes(&received),
        0,
    );
    r.check(
        "and MSG_TRUNC says the three bytes were dropped",
        got_flags(&received).contains(ReturnFlags::TRUNC) && cred.as_ref().is_some_and(is_own),
    );

    // A receive with no room for the credentials at all: the message is
    // still taken, and `MSG_CTRUNC` is how the caller learns the control
    // data did not fit.
    let _ = write(&worker, b"");
    let mut no_room: [MaybeUninit<u8>; 0] = [];
    let (received, _) = manager_recv(&manager, &mut body, 0, &mut no_room, RecvFlags::DONTWAIT);
    r.check(
        "a receive with no control room still takes the message (MSG_CTRUNC)",
        got_bytes(&received) == 0 && got_flags(&received).contains(ReturnFlags::CTRUNC),
    );

    // `read(2)` is not `recv(2)`: a zero-length read returns 0 and leaves
    // the queue exactly as it was (`vfs_read`'s short-circuit), where the
    // receive family consumes. udevd uses `recvmsg`, but a reader that
    // used `read` must not silently lose a worker's reply.
    let _ = write(&worker, b"");
    let mut nothing: [u8; 0] = [];
    r.check(
        "read(fd, buf, 0) answers 0",
        read(&manager, &mut nothing) == Ok(0),
    );
    let (still, _) = manager_recv(&manager, &mut body, 0, &mut cred_space, RecvFlags::DONTWAIT);
    check_is(
        &mut r,
        "and leaves the message queued for the receive that asks for it",
        got_bytes(&still),
        0,
    );

    // The pair as the manager makes it: seqpacket, with the two flag words the
    // way `socketpair` is called in every modern program.
    let pair = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    );
    // The flags are read back off the descriptor, so the claim covers both
    // halves of "opens with CLOEXEC|NONBLOCK folded into the type".
    let folded = pair.as_ref().is_ok_and(|(end, _)| {
        fcntl_getfd(end).is_ok_and(|flags| flags.contains(FdFlags::CLOEXEC))
            && fcntl_getfl(end).is_ok_and(|flags| flags.contains(OFlags::NONBLOCK))
    });
    r.check(
        "a seqpacket pair opens with CLOEXEC|NONBLOCK folded into the type",
        folded,
    );
    let Ok((manager, worker)) = pair else {
        r.finish()
    };

    // SO_PASSCRED is what makes the kernel attach the sender's credentials to
    // every message this end receives; without it the worker protocol has no way
    // to tell a real worker from another process that guessed the socket.
    r.check(
        "SO_PASSCRED is accepted on the receiving end",
        set_socket_passcred(&manager, true).is_ok(),
    );

    // The message the worker would send: one fixed-size struct, no control data.
    let payload = [0xa5u8; MSG_LEN];
    check_is(
        &mut r,
        "the worker end's message is queued whole",
        worker_send(&worker, &payload)
            .map(|n| n as isize)
            .unwrap_or_else(|err| -(err.raw_os_error() as isize)),
        MSG_LEN as isize,
    );

    let mut buf = [0u8; 64];
    let body = buf.len();
    let mut cred_space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmCredentials(1))];
    let (received, cred) = manager_recv(
        &manager,
        &mut buf,
        body,
        &mut cred_space,
        RecvFlags::empty(),
    );
    check_is(
        &mut r,
        "the manager's recvmsg delivers it (EINVAL here is the boot-log bug)",
        got_bytes(&received),
        MSG_LEN as isize,
    );
    r.check(
        "and the sender's credentials came attached to it",
        cred.as_ref().is_some_and(is_own),
    );
    r.check(
        "with nothing truncated about the delivery",
        !got_flags(&received).intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC),
    );
    r.check(
        "and the bytes themselves intact",
        got_bytes(&received) == MSG_LEN as isize && buf[..MSG_LEN] == payload,
    );

    // A second message, received by a caller that asked for no control room at
    // all. Linux still delivers the bytes and simply drops the credentials the
    // receiver could not have held: a kernel that answered `EINVAL` for an
    // absent control buffer would work for udevd and break everything else.
    let _ = worker_send(&worker, &payload);
    let mut no_room: [MaybeUninit<u8>; 0] = [];
    let (bare, _) = manager_recv(&manager, &mut buf, body, &mut no_room, RecvFlags::empty());
    check_is(
        &mut r,
        "a receive with no control buffer still returns the message",
        got_bytes(&bare),
        MSG_LEN as isize,
    );

    // An iovec shorter than the message: a datagram is delivered *truncated*
    // rather than refused, with `MSG_TRUNC` to say so, and the next message is
    // still whole. A worker struct that grows on one side of the pair and not
    // the other is an ordinary thing, and this is what the manager sees then.
    let _ = worker_send(&worker, &payload);
    let mut short = [0u8; 8];
    let short_len = short.len();
    let (received, _) = manager_recv(
        &manager,
        &mut short,
        short_len,
        &mut cred_space,
        RecvFlags::empty(),
    );
    check_is(
        &mut r,
        "a short iovec gets the front of the datagram, not an error",
        got_bytes(&received),
        short_len as isize,
    );
    r.check(
        "and MSG_TRUNC says the rest of that one message was dropped",
        got_flags(&received).contains(ReturnFlags::TRUNC),
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
    let Some((slow_reader, slow_writer)) = take(
        &mut r,
        "a blocking seqpacket pair opens",
        socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::empty(),
            None,
        ),
    ) else {
        r.finish()
    };
    let mut waiting = [PollFd::new(&slow_reader, PollFlags::IN)];
    check_is(
        &mut r,
        "and nothing is readable before the worker has sent",
        poll(&mut waiting, Some(&NOW))
            .map(|n| n as isize)
            .unwrap_or_else(|err| -(err.raw_os_error() as isize)),
        0,
    );

    let writer = unsafe { libc::fork() };
    if writer == 0 {
        std::thread::sleep(Duration::from_secs(1));
        let _ = worker_send(&slow_writer, &payload);
        unsafe { libc::_exit(0) };
    }
    // Five seconds is longer than the writer's second, and shorter than the
    // patience of anyone watching a test run.
    let five = Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    r.check(
        "a waiting reader is woken by the message rather than timing out",
        poll(&mut waiting, Some(&five)).is_ok_and(|n| n == 1),
    );
    let mut got = [0u8; 64];
    let got_len = got.len();
    let (received, _) = manager_recv(
        &slow_reader,
        &mut got,
        got_len,
        &mut cred_space,
        RecvFlags::empty(),
    );
    check_is(
        &mut r,
        "and the receive that follows takes it whole",
        got_bytes(&received),
        MSG_LEN as isize,
    );
    let _ = waitpid(Pid::from_raw(writer), WaitOptions::empty());

    r.finish()
}
