//! t-dns - the resolver's round trip, twelve times, timed.
//!
//! Every `ping`, `wget` and `nslookup` on the machine begins with one UDP
//! exchange: a query out to the name server, an answer back within a second or
//! two, and a transaction id that must match. Busybox's resolver fails the
//! whole name lookup on a single lost answer - `bad address` - which makes an
//! intermittently-dropped UDP reply look like a broken network at the top and
//! hide at the bottom. Pinned here, attempt by attempt: twelve independent
//! sockets, twelve queries for one name with twelve distinct ids, and each
//! answer timed, id-checked and counted. A kernel that drops one in twelve
//! fails the run.
//!
//! The name server is QEMU's user-network one at `10.0.2.3`, the only resolver
//! the guest is given, and the patience is the resolver's own: busybox gives a
//! nameserver about a second before declaring it silent, so the wait is
//! bounded rather than a blocking read that would hang the sweep on a lost
//! reply.
//!
//! **Not made on a host run.** `10.0.2.3` is the address slirp hands *its*
//! guest; from outside it is a private address with no route and no resolver
//! behind it, so twelve queries there would measure the sandbox's routing and
//! then report twelve timeouts - eighteen seconds spent proving nothing about
//! either kernel. The exchange is the guest's to make.

use mpos_user_tests::{Report, host_run};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrV4, SocketType, connect, recv, send, socket,
};
use rustix::time::{ClockId, Timespec, clock_gettime};
use std::net::Ipv4Addr;

/// QEMU's user-network name server, the only one the guest is given.
const NS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const PORT: u16 = 53;
const ATTEMPTS: u16 = 12;
/// The resolver's own patience, as a `poll` timeout: busybox gives a
/// nameserver about a second before declaring it silent.
const PATIENCE: Timespec = Timespec {
    tv_sec: 1,
    tv_nsec: 500_000_000,
};

/// Monotonic milliseconds, for a per-attempt clock.
fn ms() -> u64 {
    let now = clock_gettime(ClockId::Monotonic);
    (now.tv_sec as u64) * 1000 + (now.tv_nsec as u64) / 1_000_000
}

/// The bytes of an A/IN query for `baidu.com` with transaction id `id`.
fn query(id: u16, out: &mut [u8; 32]) -> usize {
    out[0] = (id >> 8) as u8;
    out[1] = id as u8;
    out[2] = 0x01; // RD
    out[3] = 0x00; // one question, no answers
    out[4] = 0;
    out[5] = 1; // QDCOUNT
    // The three counts after it stay zero: the buffer arrives zeroed and an
    // answer is what would put something there.
    // qname: baidu.com
    let name = b"\x05baidu\x03com\x00";
    out[12..12 + name.len()].copy_from_slice(name);
    let p = 12 + name.len();
    out[p] = 0;
    out[p + 1] = 1; // QTYPE A
    out[p + 2] = 0;
    out[p + 3] = 1; // QCLASS IN
    p + 4
}

fn main() {
    println!("t-dns");
    let mut r = Report::new();

    if host_run() {
        println!(
            "note: 10.0.2.3 is the resolver slirp hands its guest; there is no route to it from this host and no resolver behind it, so the exchange is named and not made here"
        );
        r.finish();
    }

    let ns = SocketAddrV4::new(NS, PORT);
    let mut opened = 0usize;
    let mut connected = 0usize;
    let mut sent = 0usize;
    let mut polled = 0usize;
    let mut answered = 0usize;
    let mut id_ok = 0usize;
    let mut short = 0usize;
    let mut rtt_sum_ms = 0u64;

    for attempt in 0u16..ATTEMPTS {
        let id = 0xA000 + attempt;
        let Ok(sock) = socket(AddressFamily::INET, SocketType::DGRAM, None) else {
            continue;
        };
        opened += 1;
        if connect(&sock, &ns).is_err() {
            continue;
        }
        connected += 1;

        let mut q = [0u8; 32];
        let qlen = query(id, &mut q);
        let t0 = ms();
        if send(&sock, &q[..qlen], SendFlags::empty()) != Ok(qlen) {
            continue;
        }
        sent += 1;

        // The answer's arrival, with the resolver's own patience rather than a
        // read that would park on a reply that was lost.
        let mut waiting = [PollFd::new(&sock, PollFlags::IN)];
        if poll(&mut waiting, Some(&PATIENCE)) != Ok(1) {
            continue;
        }
        polled += 1;
        let t1 = ms();

        let mut answer = [0u8; 512];
        let (n, _truncated) = match recv(&sock, &mut answer, RecvFlags::empty()) {
            Ok(got) => got,
            Err(_) => continue,
        };
        if n < 12 {
            short += 1;
            continue;
        }
        let got = (answer[0] as u16) << 8 | answer[1] as u16;
        if got != id {
            println!("note: attempt {attempt} answered with transaction id {got}, wanted {id}");
            continue;
        }
        id_ok += 1;
        answered += 1;
        rtt_sum_ms += t1 - t0;
    }

    println!(
        "note: answered {answered}/{ATTEMPTS}, mean rtt {} ms",
        if answered > 0 {
            rtt_sum_ms / answered as u64
        } else {
            0
        }
    );

    r.check(
        "every one of the twelve query sockets opened",
        opened == ATTEMPTS as usize,
    );
    r.check(
        "every one reached the name server",
        connected == ATTEMPTS as usize,
    );
    r.check("every query left", sent == ATTEMPTS as usize);
    r.check(
        "every answer arrived inside the resolver's patience",
        polled == ATTEMPTS as usize,
    );
    r.check("no answer was shorter than a header", short == 0);
    r.check(
        "every answer carried its own transaction id",
        id_ok == ATTEMPTS as usize,
    );
    r.check("every query was answered", answered == ATTEMPTS as usize);

    r.finish()
}
