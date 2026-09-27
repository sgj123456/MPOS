//! t-dns - the resolver's round trip, twelve times, timed.
//!
//! Every `ping`, `wget` and `nslookup` on the machine begins with one
//! UDP exchange: a query out to the name server, an answer back within a
//! second or two, and a transaction id that must match. Busybox's resolver
//! fails the whole name lookup on a single lost answer - `bad address` -
//! which makes an intermittently-dropped UDP reply look like a broken
//! network at the top and hide at the bottom. Pinned here, attempt by
//! attempt: twelve independent sockets, twelve queries for one name with
//! twelve distinct ids, and each answer timed, id-checked and counted. A
//! kernel that drops one in twelve fails the run.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// QEMU's user-network name server, the only one the guest is given.
fn nameserver() -> lin::SockAddrIn {
    lin::SockAddrIn::new([10, 0, 2, 3], 53)
}

/// The bytes of an A/IN query for `baidu.com` with transaction id `id`.
fn query(id: u16, out: &mut [u8; 32]) -> usize {
    out[0] = (id >> 8) as u8;
    out[1] = id as u8;
    out[2] = 0x01; // RD
    out[3] = 0x00; // one question, no answers
    out[4] = 0;
    out[5] = 1; // QDCOUNT
    for i in 6..12 {
        out[i] = 0; // ANCOUNT/NSCOUNT/ARCOUNT
    }
    // qname: baidu.com
    let name = b"\x05baidu\x03com\x00";
    out[12..12 + name.len()].copy_from_slice(name);
    let mut p = 12 + name.len();
    out[p] = 0;
    out[p + 1] = 1; // QTYPE A
    out[p + 2] = 0;
    out[p + 3] = 1; // QCLASS IN
    p += 4;
    p
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dns\n");
    let ns = nameserver();
    let mut failed = false;
    let mut answered = 0usize;
    let mut rtt_sum_ms = 0usize;

    for attempt in 0u16..12 {
        let id = 0xA000 + attempt;
        let fd = lin::socket(lin::AF_INET as usize, 2 /* SOCK_DGRAM */, 0) as i32;
        if fd < 0 {
            lin::puts(b"FAIL: socket: ");
            lin::puts_isize(fd as isize);
            failed = true;
            continue;
        }
        if lin::connect(fd, &ns) != 0 {
            lin::puts(b"FAIL: connect to the name server\n");
            failed = true;
            lin::close(fd);
            continue;
        }
        let mut q = [0u8; 32];
        let qlen = query(id, &mut q);
        let t0 = ms();
        if lin::sendto(fd, &q[..qlen], 0, None) as usize != qlen {
            lin::puts(b"FAIL: the query did not leave\n");
            failed = true;
            lin::close(fd);
            continue;
        }
        // The answer's arrival, with the resolver's own patience: busybox
        // gives a nameserver about a second before declaring it silent.
        let pfd = lin::PollFd {
            fd,
            events: 1, // POLLIN
            revents: 0,
        };
        let ready = lin::poll(&mut [pfd], 1500);
        let t1 = ms();
        if ready != 1 {
            lin::puts(b"FAIL: attempt ");
            lin::puts_isize(attempt as isize);
            lin::puts(b" timed out (poll answered ");
            lin::puts_isize(ready as isize);
            lin::puts(b")\n");
            failed = true;
            lin::close(fd);
            continue;
        }
        let mut r = [0u8; 512];
        let n = lin::recvfrom(fd, &mut r, 0, None);
        lin::close(fd);
        if n < 12 {
            lin::puts(b"FAIL: short answer: ");
            lin::puts_isize(n);
            lin::puts(b"\n");
            failed = true;
            continue;
        }
        let got = (r[0] as u16) << 8 | r[1] as u16;
        if got != id {
            lin::puts(b"FAIL: stale transaction id: got ");
            lin::puts_isize(got as isize);
            lin::puts(b", wanted ");
            lin::puts_isize(id as isize);
            lin::puts(b"\n");
            failed = true;
            continue;
        }
        answered += 1;
        rtt_sum_ms += (t1 - t0) as usize;
    }

    lin::puts(b"answered ");
    lin::puts_isize(answered as isize);
    lin::puts(b"/12, mean rtt ");
    if answered > 0 {
        lin::puts_isize((rtt_sum_ms / answered) as isize);
    }
    lin::puts(b" ms\n");
    lin::check_eq(
        &mut failed,
        b"every query was answered",
        answered as isize,
        12,
    );

    if failed {
        lin::puts(b"FAILED\n");
        lin::exit(1);
    }
    lin::puts(b"PASS\n");
    lin::exit(0)
}

/// Monotonic milliseconds, for a per-attempt clock.
fn ms() -> u64 {
    let mut ts: lin::TimeSpec = unsafe { core::mem::zeroed() };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-dns: panicked\n")
}
