//! t-netlat - loopback ICMP echo latency, measured end to end. On both machines.
//!
//! The one number this kernel's network stack has been tuned around is how long
//! a `ping 127.0.0.1` takes from inside the guest: the raw-socket send, the
//! stack's trip through slirp and back, and - what dominates, and what every
//! phase of the latency work changes - the wait between the reply arriving and
//! the blocked reader being told about it.
//!
//! The measurement is the same shape busybox's `ping` uses: `sendto` on a raw
//! ICMP socket, then a blocking `recvfrom`, twenty times over, with the RTT read
//! off `CLOCK_MONOTONIC` between the two. The replies are matched by echo id
//! and sequence number so a stray answer cannot pose as one.
//!
//! The printed per-request and summary microsecond numbers are the measurement;
//! the pass bar only guards the "answers come back at all, in a human-patience
//! amount of time" regression, because the acceptable number is a moving target
//! of the latency work itself.
//!
//! # The doors
//!
//! The socket is `rustix::net::socket(AF_INET, SOCK_RAW, Some(ipproto::ICMP))`.
//! Rustix names the protocol (`t-rawicmp`'s door, the same three words), and
//! the address is a typed `SocketAddrV4`. The send/receive pair is
//! `rustix::net::sendto`/`recvfrom`: unlike `t-unix-scm`'s control-message
//! claims, nothing here is a number on the wire the wrapper would hide - the
//! packet *is* built by hand (the checksum included) because that is what the
//! kernel's ICMP arm answers, and the claims are about timing and matching, not
//! about `msg_name` echoes. The clock is
//! `rustix::time::clock_gettime(ClockId::Monotonic)` in microseconds.
//!
//! One design fact moves with the program and is worth its sentence: a raw
//! socket on this kernel lives on eth0, never on the loopback interface - one
//! socket cannot serve two socket sets, and a raw socket's destination is a
//! per-call argument (`src/net/mod.rs`'s two-fabrics note). So `ping
//! 127.0.0.1` travels the virtual wire and comes back through slirp, which is
//! exactly what the no-std version measured and what the host cannot reproduce:
//! a host Linux answers 127.0.0.1 from its own loopback device, not through any
//! slirp. Not run on the host for that reason *and* the privilege one (a raw
//! socket wants CAP_NET_RAW, absent here); the pin is the two sweeps, where the
//! same slirp answers either machine's guest identically.

use std::net::Ipv4Addr;

use mpos_user_tests::{Report, host_run};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrV4, SocketType, ipproto, recvfrom, sendto,
    socket,
};
use rustix::time::{ClockId, clock_gettime};

/// The loopback address: the leg whose latency is this kernel's own
/// architecture, not the network beyond the host.
const PEER: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// How many echo requests one run measures. Twenty is enough for a stable
/// minimum and average while the whole suite still spends well under a second
/// here.
const ROUNDS: usize = 20;

/// The pass bar, in microseconds. Loose on purpose: it catches "loopback is
/// broken or glacial", not "regressed by a millisecond" - the summary line is
/// what the before/after comparisons read.
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
    let ts = clock_gettime(ClockId::Monotonic);
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

fn main() {
    println!("t-netlat");
    let mut r = Report::new();

    // A hosted run measures a different fabric (see the module comment): a host
    // Linux answers 127.0.0.1 from its loopback device, and this sandbox's
    // caller holds no CAP_NET_RAW anyway. Named and not made, batch 34's rule.
    if host_run() {
        println!(
            "note: the loopback trip through slirp is this kernel's fabric; a host run measures its own loopback device instead - named and not made"
        );
        r.finish();
    }

    let fd = match socket(AddressFamily::INET, SocketType::RAW, Some(ipproto::ICMP)) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("raw ICMP socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("raw ICMP socket opens", true);
    let peer = SocketAddrV4::new(PEER, 0);

    let mut rtts = [0u64; ROUNDS];
    let mut answered = 0usize;
    for seq in 1..=ROUNDS as u16 {
        // The ICMP echo request busybox builds: type/code, checksum, id,
        // sequence, then a pattern the reply must carry back.
        let mut pkt = [0u8; 28];
        pkt[0] = 8;
        pkt[4] = (ECHO_ID >> 8) as u8;
        pkt[5] = ECHO_ID as u8;
        pkt[6] = (seq >> 8) as u8;
        pkt[7] = seq as u8;
        for (i, byte) in pkt[8..28].iter_mut().enumerate() {
            *byte = b'a' + (i as u8 % 26);
        }
        let cs = icmp_cksum(&pkt);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;

        let t0 = now_us();
        let sent = sendto(&fd, &pkt, SendFlags::empty(), &peer);
        if sent.as_ref() != Ok(&28) {
            r.check(&format!("echo request {seq} leaves (got {sent:?})"), false);
            continue;
        }

        // The blocking read is the shape being measured: this is where the
        // reply's arrival waits to be noticed.
        let mut buf = [0u8; 96];
        let got = recvfrom(&fd, &mut buf, RecvFlags::empty());
        let rtt = now_us().saturating_sub(t0);
        rtts[seq as usize - 1] = rtt;
        let n = match &got {
            Ok((n, _truncated, _from)) => *n,
            Err(_) => {
                r.check(
                    &format!("round {seq} gets an echo answer (errno {got:?})"),
                    false,
                );
                continue;
            }
        };
        println!("req {seq}: rtt={rtt}us n={n}");
        answered += 1;
        // A real echo reply: ICMP type 0 behind the 20-byte IPv4 header,
        // carrying back our id, sequence and first data octet.
        let echo = n >= 28;
        let reply = echo
            && buf[20] == 0
            && buf[24] == (ECHO_ID >> 8) as u8
            && buf[25] == ECHO_ID as u8
            && buf[26] == (seq >> 8) as u8
            && buf[27] == seq as u8;
        r.check(
            &format!("round {seq}'s answer is a matching echo reply"),
            reply,
        );
    }

    // The summary the before/after comparisons read: the first round carries
    // the ARP resolution, so it is called out apart from the rest.
    r.check("every round answered", answered == ROUNDS);
    if answered == ROUNDS {
        let mut min = u64::MAX;
        let mut max = 0;
        let mut sum = 0;
        for x in &rtts {
            min = min.min(*x);
            max = max.max(*x);
            sum += x;
        }
        let later_sum: u64 = rtts[1..].iter().sum();
        println!(
            "NETLAT first={}us min={min}us avg={}us steady_avg={}us max={max}us",
            rtts[0],
            sum / ROUNDS as u64,
            later_sum / (ROUNDS - 1) as u64
        );
        // The regression bar: every round answered and none glacial.
        r.check(
            "worst round trip under the regression bar",
            max <= RTT_LIMIT_US,
        );
    }

    r.finish();
}
