//! t-epoll-socket - an unclocked epoll wait on a network socket must end.
//!
//! The regression this pins: `epoll_wait(timeout = -1)` over a network
//! socket used to have no wakeup source at all. The wait parks in the
//! scheduler until a watched node calls it back, and the AF_INET socket
//! never did - `poll_subscribe` was the trait's do-nothing default - so a
//! wait with no timeout slept past its own data and, with nothing else
//! watching, forever. A reply that arrives while the wait is parked must end
//! it, which is what the stack's wake list now provides.
//!
//! The shape: a raw ICMP socket watched for EPOLLIN; a forked child that
//! sleeps a few ticks (so the parent is genuinely parked, not racing the
//! send) and then pings the loopback address, whose reply rides the same
//! slirp path `ping 127.0.0.1` does. The parent waits without a timeout from
//! the caller's side - only the test's own generous three-second clock,
//! whose expiry is the failure being pinned: the wait must answer in
//! events, not in a timeout.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

static mut EVENTS: [lin::EpollEvent; 4] = [lin::EpollEvent { events: 0, data: 0 }; 4];

const PEER: lin::SockAddrIn = lin::SockAddrIn {
    family: 2,
    port: 0,
    addr: [127, 0, 0, 1],
    zero: [0; 8],
};

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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-epoll-socket\n");
    let mut failed = false;

    // The watched socket: raw ICMP, the same socket `ping` speaks through.
    let sock = lin::socket(
        lin::AF_INET as usize,
        3, /* SOCK_RAW */
        1, /* ICMP */
    ) as i32;
    lin::check_true(&mut failed, b"the raw ICMP socket opens", sock >= 0);

    let epfd = lin::epoll_create1(0) as i32;
    lin::check_true(
        &mut failed,
        b"epoll_create1 hands back a descriptor",
        epfd >= 0,
    );
    unsafe {
        EVENTS[0] = lin::EpollEvent {
            events: lin::EPOLLIN,
            data: sock as u64,
        };
    }
    let added = unsafe {
        lin::epoll_ctl(
            epfd,
            lin::EPOLL_CTL_ADD,
            sock,
            &raw mut EVENTS[0] as *mut lin::EpollEvent,
        )
    };
    lin::check_eq(&mut failed, b"ADD of the socket succeeds", added, 0);

    let pid = lin::fork();
    lin::check_true(&mut failed, b"fork succeeds", pid >= 0);
    if pid == 0 {
        // The child: park a few ticks so the parent's wait is truly under
        // way, then send the echo request. The reply rides slirp and the
        // host's loopback, exactly as `ping 127.0.0.1` does.
        let nap = lin::TimeSpec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        let mut rem: lin::TimeSpec = unsafe { core::mem::zeroed() };
        let _ = lin::nanosleep(&nap, &mut rem);
        let mut pkt = [0u8; 28];
        pkt[0] = 8; // echo request
        pkt[4] = 0x0F;
        pkt[5] = 0x0D; // id
        pkt[6] = 0;
        pkt[7] = 1; // sequence
        for i in 8..28 {
            pkt[i] = b'a' + ((i as u8 - 8) % 26);
        }
        let cs = icmp_cksum(&pkt);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;
        let sent = lin::sendto(sock, &pkt, 0, Some(&PEER));
        if sent != 28 {
            lin::exit(1);
        }
        lin::exit(0);
    }

    // The parent: an unclocked wait from the caller's side. Three seconds is
    // the test's own patience, not a timeout the wait is meant to use: the
    // reply is 100ms out, so answering by clock is the failure mode.
    let n = lin::epoll_wait(
        epfd,
        unsafe { &mut EVENTS[0] as *mut lin::EpollEvent },
        4,
        3000,
    );
    lin::check_eq(
        &mut failed,
        b"the reply wakes the unclocked epoll wait",
        n,
        1,
    );
    if n == 1 {
        let events = unsafe { EVENTS[0].events };
        lin::check_true(
            &mut failed,
            b"the report is EPOLLIN",
            events & lin::EPOLLIN != 0,
        );
    }

    // Drain the reply and reap the child, so nothing leaks past the test. The
    // drain is nonblocking on purpose: the reply the wait reported is already
    // in the socket, and a blocking read here would park the test - and with
    // it the whole suite - forever if anything about that assumption is wrong.
    let mut buf = [0u8; 96];
    let got = lin::recvfrom(sock, &mut buf, 0x40 /* MSG_DONTWAIT */, None);
    lin::check_true(&mut failed, b"the echo reply reads back", got >= 28);
    let mut status: i32 = 0;
    let reaped = lin::wait4(pid as usize, &mut status, 0);
    lin::check_eq(&mut failed, b"the child is reaped", reaped, pid);
    lin::check_eq(
        &mut failed,
        b"the child exited clean",
        (status & 0xff) as isize,
        0,
    );

    lin::close(epfd);
    lin::close(sock);
    if failed {
        lin::puts(b"FAILED\n");
        lin::exit(1);
    }
    lin::puts(b"PASS\n");
    lin::exit(0)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-epoll-socket: panicked\n")
}
