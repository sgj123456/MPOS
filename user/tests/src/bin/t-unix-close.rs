//! t-unix-close - a peer's departure is a hang-up on the descriptor that
//! watched it, not silence.
//!
//! The boot this cost: OpenRC's `udev-settle` service runs `udevadm settle`,
//! which sends udevd a control ping and then `poll`s the connection for a
//! readable event with the settle timeout as its patience. udevd accepts the
//! ping, handles it and closes the connection - and on Linux that close is
//! `POLLIN|POLLHUP` on the client's descriptor, so `ctrl_send` returns at
//! once. This kernel's `UnixSock` never reported a hang-up at all: `poll_hup`
//! was the trait's `false`, the client's wait was never woken, and the settle
//! sat out its whole 120-second default before the boot went on. Measured in
//! the guest on the way in (`unixcloseprobe.c`) and against a current host
//! (`.workbuddy/probe_unix_close.c`, Linux 7.2.11).
//!
//! What a current host answers, and what is pinned here:
//!
//! * a connected **`SOCK_SEQPACKET`** end whose peer closed is readable *and*
//!   hung up - `POLLIN|POLLHUP`, epoll `EPOLLIN|EPOLLHUP` - and a receive
//!   answers 0, not `EAGAIN`. Messages the dead peer sent first are still
//!   delivered; the 0 comes after them, and a peek sees the queued message
//!   rather than the end-of-file;
//! * the same is true of a connected **`SOCK_STREAM`** end, whether it came
//!   from `socketpair` or from a named listener;
//! * a connected **`SOCK_DGRAM`** end is *not* hung up: with its peer gone it
//!   reports writable only, and its empty queue stays `EAGAIN` - the peer can
//!   be replaced by another `connect`, so "never again" would be a lie. This
//!   half is the regression the fix could have introduced.
//!
//! The two wakes are the part a close-before-the-wait cannot measure: the
//! descriptor is watched first, a forked child closes the accepted end 100 ms
//! later, and the wait must return long before its five-second patience (the
//! elapsed check is the claim - a wait that only re-scans at its deadline
//! still answers the right bits, two seconds too late). The child shares no
//! buffered output; it leaves through `_exit`, the shape `t-epoll-socket`
//! spells out.
//!
//! The epoll door is the raw `SYS_epoll_*` one for `t-epoll-socket`'s reason:
//! arm64 has no `epoll_wait` number, only `epoll_pwait`, and the packed record
//! is the machine's own shape.

use mpos_user_tests::Report;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvFlags, SocketAddrUnix, SocketFlags, SocketType, accept, bind, connect,
    listen, recv, socket, socketpair,
};
use rustix::time::{ClockId, clock_gettime};

static SEQ: &str = "/tmp/t-unix-close-seq.sock";
static STREAM: &str = "/tmp/t-unix-close-stream.sock";
static DGRAM: &str = "/tmp/t-unix-close-dgram.sock";

/// The packed report record, per machine - see the module comment.
#[cfg(target_arch = "x86_64")]
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}
#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Ev {
    events: u32,
    data: u64,
}

const ADD: i32 = 1;
const EPOLLIN: u32 = 0x001;
const EPOLLHUP: u32 = 0x010;

/// The wait's patience. The child closes 100 ms in, so an answer anywhere
/// near this number is the deadline, not the event.
const PATIENCE_SECS: i64 = 5;
/// What "woken by the close" may cost: twenty times the child's nap, and far
/// below the patience above.
const TIMELY_MS: u128 = 2000;

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn fix(r: libc::c_long) -> isize {
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

fn epoll_create1() -> isize {
    fix(unsafe { libc::syscall(libc::SYS_epoll_create1, 0 as libc::c_long) })
}

fn epoll_ctl(epfd: i32, fd: i32, ev: &Ev) -> isize {
    fix(unsafe {
        libc::syscall(
            libc::SYS_epoll_ctl,
            epfd as libc::c_long,
            ADD as libc::c_long,
            fd as libc::c_long,
            ev as *const Ev as libc::c_long,
        )
    })
}

fn epoll_wait(epfd: i32, events: &mut [Ev], timeout_ms: i32) -> isize {
    #[cfg(target_arch = "x86_64")]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_wait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            events.len() as libc::c_long,
            timeout_ms as libc::c_long,
        )
    };
    #[cfg(not(target_arch = "x86_64"))]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_pwait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            events.len() as libc::c_long,
            timeout_ms as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    fix(r)
}

fn now_ms() -> u128 {
    let ts = clock_gettime(ClockId::Monotonic);
    (ts.tv_sec as u128) * 1000 + (ts.tv_nsec as u128) / 1_000_000
}

fn addr(path: &str) -> SocketAddrUnix {
    SocketAddrUnix::new(path).unwrap_or_else(|e| panic!("building {path}: {e}"))
}

/// A named pair: the listener, the client and the accepted server end. The
/// listener stays alive for the whole check, so a hang-up can only come from
/// the connection and not from the name going away.
fn named_pair(
    r: &mut Report,
    path: &str,
    typ: SocketType,
    what: &str,
) -> (OwnedFd, OwnedFd, OwnedFd) {
    let l = socket(AddressFamily::UNIX, typ, None).unwrap_or_else(|e| panic!("socket: {e}"));
    bind(&l, &addr(path)).unwrap_or_else(|e| panic!("bind {path}: {e}"));
    listen(&l, 4).unwrap_or_else(|e| panic!("listen {path}: {e}"));
    let client = socket(AddressFamily::UNIX, typ, None).unwrap_or_else(|e| panic!("socket: {e}"));
    connect(&client, &addr(path)).unwrap_or_else(|e| panic!("connect {path}: {e}"));
    let server = accept(&l).unwrap_or_else(|e| panic!("accept {path}: {e}"));
    r.check(what, true);
    (l, client, server)
}

fn rcv(fd: &OwnedFd, flags: RecvFlags) -> Result<usize, Errno> {
    recv(fd, &mut [0u8; 64], flags).map(|(n, _)| n)
}

/// One poll of `fd` for `events`, answering the raw flags and the elapse.
fn wait_on(fd: &OwnedFd, events: PollFlags) -> (Option<PollFlags>, u128) {
    let timeout = Timespec {
        tv_sec: PATIENCE_SECS,
        tv_nsec: 0,
    };
    let mut fds = [PollFd::new(fd, events)];
    let started = now_ms();
    let n = poll(&mut fds, Some(&timeout)).unwrap_or(0);
    let elapsed = now_ms() - started;
    (if n > 0 { Some(fds[0].revents()) } else { None }, elapsed)
}

/// Fork a child that closes `server` after a nap, and leave the parent
/// holding nothing but its own end. The child also drops the listener and
/// the client, so only its copy of the accepted end keeps the connection
/// alive until the close under test.
fn close_later(server: OwnedFd, listener: OwnedFd, client_fd: i32) -> libc::pid_t {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        unsafe {
            libc::close(client_fd);
            drop(listener);
            let nap = libc::timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            };
            let mut rem = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            libc::nanosleep(&nap, &mut rem);
            drop(server);
            libc::_exit(0);
        }
    }
    pid
}

fn reap(pid: libc::pid_t) {
    let mut status: libc::c_int = 0;
    if pid > 0 {
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}

fn main() {
    println!("t-unix-close");
    let mut r = Report::new();
    for path in [SEQ, STREAM, DGRAM] {
        let _ = rustix::fs::unlink(path);
    }

    // ---------------------------------------------------------------------
    // A named SEQPACKET connection: a queued message still arrives, the read
    // after it is 0, and the descriptor is readable and hung up.
    // ---------------------------------------------------------------------
    {
        let (l, c, s) = named_pair(
            &mut r,
            SEQ,
            SocketType::SEQPACKET,
            "a named seqpacket pair connects",
        );
        r.check(
            "the server's last message is sent",
            rustix::io::write(&s, b"bye") == Ok(3),
        );
        drop(s);
        let peeked = rcv(&c, RecvFlags::PEEK | RecvFlags::DONTWAIT);
        r.check(
            &format!("a peek after the close still sees the queued message (got {peeked:?})"),
            peeked == Ok(3),
        );
        let first = rcv(&c, RecvFlags::empty());
        r.check(
            &format!("a blocking read takes the message, not the end-of-file (got {first:?})"),
            first == Ok(3),
        );
        let at_end = rcv(&c, RecvFlags::DONTWAIT);
        r.check(
            &format!("and the read after it answers 0 rather than EAGAIN (got {at_end:?})"),
            at_end == Ok(0),
        );
        let (revents, _) = wait_on(&c, PollFlags::IN);
        r.check(
            &format!("poll reports the close as POLLIN|POLLHUP (got {revents:?})"),
            revents.is_some_and(|f| f.contains(PollFlags::IN | PollFlags::HUP)),
        );
        drop(l);
        let _ = rustix::fs::unlink(SEQ);
    }

    // ---------------------------------------------------------------------
    // The wake: the wait is under way when the far end closes, and the close
    // - not the five-second patience - ends it.
    // ---------------------------------------------------------------------
    {
        let (l, c, s) = named_pair(
            &mut r,
            SEQ,
            SocketType::SEQPACKET,
            "a second seqpacket pair connects",
        );
        let pid = close_later(s, l, c.as_raw_fd());
        r.check("the closing child starts", pid > 0);
        let (revents, elapsed) = wait_on(&c, PollFlags::IN);
        r.check(
            &format!("the close wakes a parked poll (revents={revents:?}, {elapsed} ms)"),
            revents.is_some_and(|f| f.contains(PollFlags::IN | PollFlags::HUP))
                && elapsed < TIMELY_MS,
        );
        reap(pid);
        let _ = rustix::fs::unlink(SEQ);
    }

    // ---------------------------------------------------------------------
    // The same wake through epoll, whose wait walks the interest list rather
    // than the caller's array.
    // ---------------------------------------------------------------------
    {
        let (l, c, s) = named_pair(
            &mut r,
            SEQ,
            SocketType::SEQPACKET,
            "a seqpacket pair for epoll connects",
        );
        let epfd = epoll_create1();
        let ev = Ev {
            events: EPOLLIN,
            data: 0,
        };
        r.check(
            "the client is added to an epoll instance",
            epfd >= 0 && epoll_ctl(epfd as i32, c.as_raw_fd(), &ev) == 0,
        );
        let pid = close_later(s, l, c.as_raw_fd());
        let mut out = [ev; 4];
        let started = now_ms();
        let n = epoll_wait(epfd as i32, &mut out, (PATIENCE_SECS * 1000) as i32);
        let elapsed = now_ms() - started;
        r.check(
            &format!(
                "epoll_wait ends on the close with EPOLLIN|EPOLLHUP (n={n}, events=0x{:x}, {elapsed} ms)",
                if n > 0 { out[0].events } else { 0 }
            ),
            n == 1
                && out[0].events & (EPOLLIN | EPOLLHUP) == EPOLLIN | EPOLLHUP
                && elapsed < TIMELY_MS,
        );
        reap(pid);
        unsafe { libc::close(epfd as i32) };
        let _ = rustix::fs::unlink(SEQ);
    }

    // ---------------------------------------------------------------------
    // A socketpair SEQPACKET end: the same close, with no name in the way.
    // ---------------------------------------------------------------------
    {
        let (a, b) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::empty(),
            None,
        )
        .unwrap_or_else(|e| panic!("socketpair: {e}"));
        drop(b);
        let (revents, _) = wait_on(&a, PollFlags::IN);
        r.check(
            &format!("a seqpacket pair's end reports POLLIN|POLLHUP (got {revents:?})"),
            revents.is_some_and(|f| f.contains(PollFlags::IN | PollFlags::HUP)),
        );
        let at_end = rcv(&a, RecvFlags::DONTWAIT);
        r.check(
            &format!("and its receive answers 0 (got {at_end:?})"),
            at_end == Ok(0),
        );
    }

    // ---------------------------------------------------------------------
    // A named STREAM connection: the wrapper around the byte channel has to
    // forward the channel's hang-up, as the socketpair's direct end does.
    // ---------------------------------------------------------------------
    {
        let (l, c, s) = named_pair(
            &mut r,
            STREAM,
            SocketType::STREAM,
            "a named stream pair connects",
        );
        drop(s);
        let (revents, _) = wait_on(&c, PollFlags::IN);
        r.check(
            &format!("a named stream reports POLLIN|POLLHUP (got {revents:?})"),
            revents.is_some_and(|f| f.contains(PollFlags::IN | PollFlags::HUP)),
        );
        let at_end = rcv(&c, RecvFlags::DONTWAIT);
        r.check(
            &format!("and its receive answers 0 (got {at_end:?})"),
            at_end == Ok(0),
        );
        drop(l);
        let _ = rustix::fs::unlink(STREAM);
    }

    // ---------------------------------------------------------------------
    // The datagram contrast: a dead peer is not an end-of-stream there. The
    // descriptor stays writable, is not hung up, and the empty queue is
    // EAGAIN - a `connect` can name a new peer.
    // ---------------------------------------------------------------------
    {
        let server = socket(AddressFamily::UNIX, SocketType::DGRAM, None)
            .unwrap_or_else(|e| panic!("socket: {e}"));
        bind(&server, &addr(DGRAM)).unwrap_or_else(|e| panic!("bind {DGRAM}: {e}"));
        let client = socket(AddressFamily::UNIX, SocketType::DGRAM, None)
            .unwrap_or_else(|e| panic!("socket: {e}"));
        connect(&client, &addr(DGRAM)).unwrap_or_else(|e| panic!("connect {DGRAM}: {e}"));
        drop(server);
        let (revents, _) = wait_on(&client, PollFlags::IN | PollFlags::OUT);
        r.check(
            &format!("a datagram's dead peer reports writable, not hung up (got {revents:?})"),
            revents.is_some_and(|f| {
                f.contains(PollFlags::OUT)
                    && !f.contains(PollFlags::HUP)
                    && !f.contains(PollFlags::ERR)
            }),
        );
        let empty = rcv(&client, RecvFlags::DONTWAIT);
        r.check(
            &format!("and its empty queue stays EAGAIN, not 0 (got {empty:?})"),
            empty == Err(Errno::AGAIN),
        );
        let _ = rustix::fs::unlink(DGRAM);
    }

    r.finish()
}
