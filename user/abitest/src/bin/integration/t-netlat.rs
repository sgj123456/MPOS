//! t-netlat - loopback ICMP echo latency, measured end to end.
//!
//! The one number this kernel's network stack has been tuned around is how
//! long a `ping 127.0.0.1` takes from inside the guest: the raw-socket send,
//! the stack's trip through slirp and back, and - what dominates, and what
//! every phase of the latency work changes - the wait between the reply
//! arriving and the blocked reader being told about it.
//!
//! The measurement is the same shape busybox's `ping` uses: `sendto` on a raw
//! ICMP socket, then a blocking `recvfrom`, twenty times over, with the RTT
//! read off `CLOCK_MONOTONIC` between the two. The replies are matched by
//! echo id and sequence number so a stray answer cannot pose as one.
//!
//! The printed per-request and summary microsecond numbers are the
//! measurement; the pass bar only guards the "answers come back at all, in
//! a human-patience amount of time" regression, because the acceptable
//! number is a moving target of the latency work itself.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// The loopback address: the leg whose latency is this kernel's own
/// architecture, not the network beyond the host.
const PEER: lin::SockAddrIn = lin::SockAddrIn {
    family: 2,
    port: 0,
    addr: [127, 0, 0, 1],
    zero: [0; 8],
};

/// How many echo requests one run measures. Twenty is enough for a stable
/// minimum and average while the whole suite still spends well under a
/// second here.
const ROUNDS: usize = 20;

/// The pass bar, in microseconds. Loose on purpose: it catches "loopback is
/// broken or glacial", not "regressed by a millisecond" - the summary line
/// is what the before/after comparisons read.
const RTT_LIMIT_US: u64 = 50_000;

const ECHO_ID: u16 = 0x0F0D;

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

/// Monotonic microseconds - fine enough to see a sub-millisecond round trip,
/// which is the whole point of the units.
fn now_us() -> u64 {
    let mut ts: lin::TimeSpec = unsafe { core::mem::zeroed() };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-netlat\n");
    let mut failed = false;

    let fd = lin::socket(
        lin::AF_INET as usize,
        3, /* SOCK_RAW */
        1, /* ICMP */
    ) as i32;
    lin::check_true(&mut failed, b"raw ICMP socket opens", fd >= 0);

    let mut rtts = [0u64; ROUNDS];
    for seq in 1..=ROUNDS as u16 {
        // The ICMP echo request busybox builds: type/code, checksum, id,
        // sequence, then a pattern the reply must carry back.
        let mut pkt = [0u8; 28];
        pkt[0] = 8;
        pkt[4] = (ECHO_ID >> 8) as u8;
        pkt[5] = ECHO_ID as u8;
        pkt[6] = (seq >> 8) as u8;
        pkt[7] = seq as u8;
        for i in 8..28 {
            pkt[i] = b'a' + ((i as u8 - 8) % 26);
        }
        let cs = icmp_cksum(&pkt);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;

        let t0 = now_us();
        let sent = lin::sendto(fd, &pkt, 0, Some(&PEER));
        if sent != 28 {
            lin::puts(b"FAIL: echo request did not leave: ");
            lin::puts_isize(sent);
            lin::puts(b"\n");
            failed = true;
            continue;
        }

        // The blocking read is the shape being measured: this is where the
        // reply's arrival waits to be noticed.
        let mut buf = [0u8; 96];
        let mut from: lin::SockAddrIn = unsafe { core::mem::zeroed() };
        let n = lin::recvfrom(fd, &mut buf, 0, Some(&mut from));
        let rtt = now_us().saturating_sub(t0);
        rtts[seq as usize - 1] = rtt;
        lin::puts(b"req ");
        lin::puts_isize(seq as isize);
        lin::puts(b": rtt=");
        lin::puts_isize(rtt as isize);
        lin::puts(b"us n=");
        lin::puts_isize(n);
        lin::puts(b"\n");
        if n < 0 {
            lin::puts(b"FAIL: no echo answer\n");
            failed = true;
            continue;
        }
        // A real echo reply: ICMP type 0 behind the 20-byte IPv4 header,
        // carrying back our id, sequence and first data octet.
        let echo = n >= 28;
        let reply = buf[20] == 0
            && buf[24] == (ECHO_ID >> 8) as u8
            && buf[25] == ECHO_ID as u8
            && buf[26] == (seq >> 8) as u8
            && buf[27] == seq as u8;
        if !echo || !reply {
            lin::puts(b"FAIL: not a matching echo reply\n");
            failed = true;
        }
    }

    // The summary the before/after comparisons read: the first round
    // carries the ARP resolution, so it is called out apart from the rest.
    let answered = rtts.iter().filter(|r| **r > 0).count();
    if answered == ROUNDS {
        let mut min = u64::MAX;
        let mut max = 0;
        let mut sum = 0;
        for r in &rtts {
            min = min.min(*r);
            max = max.max(*r);
            sum += r;
        }
        let later_sum: u64 = rtts[1..].iter().sum();
        lin::puts(b"NETLAT first=");
        lin::puts_isize(rtts[0] as isize);
        lin::puts(b"us min=");
        lin::puts_isize(min as isize);
        lin::puts(b"us avg=");
        lin::puts_isize((sum / ROUNDS as u64) as isize);
        lin::puts(b"us steady_avg=");
        lin::puts_isize((later_sum / (ROUNDS - 1) as u64) as isize);
        lin::puts(b"us max=");
        lin::puts_isize(max as isize);
        lin::puts(b"us\n");
        // The regression bar: every round answered and none glacial.
        if max > RTT_LIMIT_US {
            lin::puts(b"FAIL: worst round trip over the regression bar\n");
            failed = true;
        }
    } else {
        lin::puts(b"FAIL: not every round answered\n");
        failed = true;
    }

    lin::close(fd);
    if failed {
        lin::puts(b"FAILED\n");
        lin::exit(1);
    }
    lin::puts(b"PASS\n");
    lin::exit(0)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-netlat: panicked\n")
}
