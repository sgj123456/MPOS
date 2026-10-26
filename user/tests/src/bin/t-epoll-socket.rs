//! t-epoll-socket - an epoll wait on a network socket must end.
//!
//! The regression this pins: `epoll_wait` over a network socket used to have
//! no wakeup source at all. The wait parks in the scheduler until a watched
//! node calls it back, and the AF_INET socket never did - `poll_subscribe`
//! was the trait's do-nothing default - so a wait with no timeout slept past
//! its own data and, with nothing else watching, forever. A reply that
//! arrives while the wait is parked must end it, which is what the stack's
//! wake list now provides.
//!
//! The shape: a raw ICMP socket watched for EPOLLIN; a forked child that
//! sleeps a few ticks (so the parent is genuinely parked, not racing the
//! send) and then pings **slirp's gateway**, whose echo slirp answers. The
//! parent waits with three seconds of patience rather than no clock, and the
//! three seconds are the failure being pinned: answering by clock instead of
//! by event is the counterfactual.
//!
//! **Which address is asked, and why it is not `127.0.0.1` (measured).** A raw
//! socket here lives on eth0 and never on the loopback interface
//! (`src/net/mod.rs`'s two-fabrics note: one socket cannot serve two sets, and
//! a raw socket's destination is a per-call argument), so an echo aimed at
//! `127.0.0.1` leaves by the virtual wire and is answered - or not - by slirp.
//! It is not: QEMU's user-mode backend does not carry ICMP at all
//! (`Documentation/Networking`: *"ping (ICMP) will not work"*), so that echo
//! is dropped on the host side and no reply can ever arrive, whatever this
//! kernel does. Measured on the gate's own test machine, one program in its
//! own lane: with the peer `127.0.0.1` the two reply claims fail
//! (`the reply wakes the epoll wait`, `the echo reply reads back`) while
//! `the child exited clean` still passes - the request *was* sent (28 bytes
//! handed to `sendto`) and nothing came back. With the peer `10.0.2.2` - the
//! gateway slirp answers for the host's loopback - the same nine claims are
//! 9/9 on the same kernel (`=== SUMMARY pass=1 fail=0`, exit 33). So what the
//! fixture was reaching for is slirp's own echo, and the wake this program
//! exists to pin has never been broken: answering `127.0.0.1` is a property of
//! the emulated wire, not of this kernel's raw socket.
//!
//! **Two doors, and why.** The socket side is rustix - nothing about it is a
//! clause-(c) shape - but the epoll side rides the raw `SYS_epoll_*` door for
//! the reason `t-epoll` gives at length: arm64 carries no `epoll_wait`
//! number, only `epoll_pwait` (`nr_aarch64.rs`: `EPOLL_WAIT = 64029; // no
//! arm64 number`), and this program is the first thing to ask for an *IP*
//! wakeup on the second machine, so the wait has to be spelled the way that
//! machine has it. The packed record is declared here rather than imported
//! because the wait writes the caller's own shape back.
//!
//! **Host divergence: the door this test needs is closed there.** A raw
//! `SOCK_RAW` socket needs `CAP_NET_RAW`, and this container has it not
//! (measured: `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` answers `EPERM`,
//! `probe_rawicmp_epoll.c`) - so on a host run there is no socket to watch,
//! no ping to send, and nothing here can be measured. That is named and not
//! made: the program prints the note and finishes with no claims rather than
//! pinning an answer a machine that cannot open the door did not give.

use mpos_user_tests::{Report, host_run};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrV4, SocketType, ipproto, recvfrom, sendto,
    socket,
};
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd};

/// The *machine's* record: `data` sits at byte 8 on aarch64 and at byte 4 on the
/// x86_64 layout the struct is packed into (`EPOLL_PACKED` in the uapi is
/// `#if defined(__x86_64__)`, and musl's `sys/epoll.h` copies that guard).
/// Declared here rather than imported because the wait writes the caller's own
/// shape back - which is exactly why it has to be *this* machine's: the sibling
/// `t-epoll` carried the x86_64 shape on both machines, so the kernel could
/// write the wrong offsets on aarch64 and no claim could see it.
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

/// How long the parent's wait may take the clock to answer. The reply is
/// 100 ms out, so an answer that used this is the bug.
const PATIENCE_MS: i32 = 3000;

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Turn musl's `-1 + errno` door into the negated-errno answer these claims
/// compare against, read while the failing call is still the last one made.
fn fix(r: libc::c_long) -> isize {
    if r < 0 {
        -(last_errno() as isize)
    } else {
        r as isize
    }
}

fn create1(flags: u32) -> isize {
    let r = unsafe { libc::syscall(libc::SYS_epoll_create1, flags as libc::c_long) };
    fix(r)
}

fn ctl(epfd: i32, op: i32, fd: i32, ev: &Ev) -> isize {
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_ctl,
            epfd as libc::c_long,
            op as libc::c_long,
            fd as libc::c_long,
            ev as *const Ev as libc::c_long,
        )
    };
    fix(r)
}

/// `epoll_wait(epfd, events, maxevents, timeout)`. On the second machine the
/// only spelling of the wait is `epoll_pwait` with a null signal mask; the
/// words that matter stay the caller's.
fn wait(epfd: i32, events: &mut [Ev], maxevents: i32, timeout: i32) -> isize {
    #[cfg(target_arch = "x86_64")]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_wait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            maxevents as libc::c_long,
            timeout as libc::c_long,
        )
    };
    #[cfg(not(target_arch = "x86_64"))]
    let r = unsafe {
        libc::syscall(
            libc::SYS_epoll_pwait,
            epfd as libc::c_long,
            events.as_mut_ptr() as libc::c_long,
            maxevents as libc::c_long,
            timeout as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    };
    fix(r)
}

/// RFC 1071's one's-complement sum, which is all an echo request's checksum
/// word is. A wrong one is not refused - the peer simply never answers - so
/// it is computed here rather than taken on faith from the no-std original.
fn icmp_cksum(buf: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < buf.len() {
        sum += u16::from_be_bytes([buf[i], buf[i + 1]]) as u32;
        i += 2;
    }
    if i < buf.len() {
        sum += (buf[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn main() {
    println!("t-epoll-socket");
    let mut r = Report::new();

    // The watched socket: raw ICMP, the same socket `ping` speaks through.
    let opened = socket(AddressFamily::INET, SocketType::RAW, Some(ipproto::ICMP));
    if let Err(e) = &opened
        && host_run()
    {
        println!(
            "note: socket(AF_INET, SOCK_RAW, IPPROTO_ICMP) answered {e} - this host has no \
             CAP_NET_RAW, so there is no socket to watch and no ping to send; the wakeup \
             cannot be measured here"
        );
        r.finish();
    }
    r.check("the raw ICMP socket opens", opened.is_ok());
    let sock = match opened {
        Ok(fd) => fd,
        Err(_) => r.finish(),
    };

    let epfd = create1(0);
    r.check("epoll_create1 hands back a descriptor", epfd >= 0);

    let raw_sock = sock.as_fd().as_raw_fd();
    let mut watched = [Ev {
        events: EPOLLIN,
        data: 0,
    }; 4];
    let added = ctl(epfd as i32, ADD, raw_sock, &watched[0]);
    r.check("ADD of the socket succeeds", added == 0);

    // The claim about the fork comes after the child's branch, for the reason
    // `t-memfd-shm` spells out: a check written between `fork` and
    // `if pid == 0` is printed by both processes.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The child: park a few ticks so the parent's wait is truly under
        // way, then send the echo request. The peer is slirp's gateway, the
        // one address whose ICMP echo the emulated wire answers - see this
        // file's header for the measurement that made `127.0.0.1` wrong. It
        // leaves through `_exit`, taking no share of the parent's buffered
        // stdout.
        let nap = libc::timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        let mut rem = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::nanosleep(&nap, &mut rem);
        }
        let mut pkt = [0u8; 28];
        pkt[0] = 8; // echo request
        pkt[4] = 0x0F;
        pkt[5] = 0x0D; // id
        pkt[6] = 0;
        pkt[7] = 1; // sequence
        for (i, byte) in pkt.iter_mut().enumerate().skip(8) {
            *byte = b'a' + ((i as u8) - 8) % 26;
        }
        let cs = icmp_cksum(&pkt);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;

        let peer = SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 2), 0);
        let sent = sendto(&sock, &pkt, SendFlags::empty(), &peer).unwrap_or(0);
        unsafe { libc::_exit(if sent == 28 { 0 } else { 1 }) };
    }

    r.check("fork succeeds", pid > 0);

    // The parent: a wait whose own answer must be the event, not the clock.
    let n = wait(epfd as i32, &mut watched, 4, PATIENCE_MS);
    r.check("the reply wakes the epoll wait", n == 1);
    if n == 1 {
        r.check("the report is EPOLLIN", watched[0].events & EPOLLIN != 0);
    }

    // Drain the reply and reap the child, so nothing leaks past the test. The
    // drain is nonblocking on purpose: the reply the wait reported is already
    // in the socket, and a blocking read here would park the test - and with
    // it the whole suite - forever if anything about that assumption is wrong.
    let mut buf = [0u8; 96];
    let got = recvfrom(&sock, &mut buf, RecvFlags::DONTWAIT);
    r.check(
        "the echo reply reads back",
        matches!(&got, Ok((n, _, _)) if *n >= 28),
    );

    let mut status: libc::c_int = 0;
    let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
    r.check("the child is reaped", reaped == pid);
    r.check(
        "the child exited clean",
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
    );

    r.finish();
}
