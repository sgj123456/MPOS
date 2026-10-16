//! t-udpblock - a blocking UDP send **waits** for the send buffer, and a
//! non-blocking one is refused. On both machines.
//!
//! A datagram socket's send buffer is finite, so a burst outruns it, and what
//! the caller hears then is the caller's question. Linux answers it two ways,
//! and this kernel used to answer one of them to both: a *non-blocking* sender
//! gets `EAGAIN`, and a *blocking* sender **parks** until the buffer drains
//! (`docs/uapi/` §5.2 named the second half as the gap; `src/net/socket.rs`'s
//! `udp_send` now serves it).
//!
//! Measured on a current host before any of this was written
//! (`.workbuddy/probe-udpblock*.c`, Linux 7.2.9). The state is made by sending
//! to an on-link address that never answers ARP: the first datagram queues
//! behind the incomplete neighbour entry and an skb sitting there keeps its
//! `sk_wmem_alloc` charge, so the socket's send buffer fills behind it exactly
//! as it would behind a slow device - and the neighbour gives up about three
//! seconds later, which is also when a parked sender's room appears. With
//! `SO_SNDBUF` at its floor (2048 asked, 4608 reported) and 1400-byte
//! datagrams, **three went in** and the fourth *blocking* send slept
//! **3072-3098 ms** and then returned the datagram; the non-blocking send at
//! the same point answered `EAGAIN` at once. A signal inside the park answers
//! `EINTR` when the handler has no `SA_RESTART`, and is swallowed - the send
//! restarted and completed - when it has one. The size line is separate and is
//! *not* the buffer: a 65507-byte datagram is accepted and a 65508-byte one is
//! `EMSGSIZE`, so a datagram wider than `SO_SNDBUF` is still taken (5600 bytes
//! into a 4608-byte buffer, measured).
//!
//! # What this program pins
//!
//! The two answers, in the one state a guest can build without help: the fill
//! is done with per-send `MSG_DONTWAIT` (which is the non-blocking answer, and
//! is what stops the *fill* itself from parking), and then
//!
//! 1. the non-blocking send in the filled state is `EAGAIN`, immediately;
//! 2. the **blocking** send in the same state **waits** - pinned by cutting the
//!    wait short with a `SIGALRM` that has no `SA_RESTART`, which answers
//!    `EINTR` after the alarm's own delay rather than at once, and by the
//!    alarm's handler having run at all;
//! 3. with `SA_RESTART` the same send is restarted rather than answered, so it
//!    waits the bound out and then answers `EAGAIN` - which is the safety
//!    valve's own pin, and the reason a wedged egress cannot hang a caller
//!    forever (see `udp_send`'s `DRAIN_WAIT_MS`: smoltcp keeps a datagram its
//!    egress could not dispatch, where Linux's device drops the frame and
//!    releases the charge);
//! 4. a fresh socket afterwards still sends, so a cut-short wait cost the one
//!    datagram and nothing else.
//!
//! # Why the peer is 10.0.2.99
//!
//! slirp's own subnet is 10.0.2.0/24 and it answers ARP for the gateway
//! (10.0.2.2) and nothing else, so an address in that subnet that nothing holds
//! is the guest's version of the host's dead neighbour: on-link, so an ARP
//! request really goes out, and unanswered, so the datagram stays queued and
//! the buffer fills. An off-subnet address would not do - it routes by the
//! gateway, whose MAC is known, and the frames would leave.
//!
//! # Why not on the host
//!
//! The buffer being filled is this kernel's own size (four packet slots and a
//! 4 KiB payload ring, `src/net/mod.rs`'s `add_udp`), and the host's default
//! `SO_SNDBUF` is 212992 - the same program on a host would have to shrink it
//! to see anything, and then it would be measuring the host's buffer rather
//! than this kernel's. The claim is about this kernel's answer, so a host run
//! says so and stops.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicI32, Ordering};

use mpos_user_tests::{Report, host_run};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SendFlags, SocketAddrV4, SocketType, sendto, socket};
use rustix::time::{ClockId, clock_gettime};

/// An on-link address nothing answers for. See the module comment.
const DEAD: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 99);
/// The gateway, which does answer: what claim 4 sends to.
const LIVE: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// discard(9): nothing listens, and an unanswered datagram is still a
/// transmitted frame, which is all the send buffer's accounting needs.
const PORT: u16 = 9;
/// One datagram's payload: MTU-sized, so a few of them fill the 4 KiB payload
/// ring and the fill is a handful of sends rather than a hundred.
const PAYLOAD: usize = 1400;
/// How long before the alarm fires into the park. Short enough that the
/// program stays quick, long enough that a park is unmistakably a park: the
/// fill's own sends take microseconds.
const ALARM_MS: u64 = 100;
/// The window a parked send's answer may take. The lower bound is what makes
/// the claim a *wait* (an immediate `EINTR` would be a refusal, not a park),
/// and the upper bound is what keeps a hang from reading as a pass.
const PARK_LOW_MS: f64 = 20.0;
const PARK_HIGH_MS: f64 = 2000.0;
/// The same window for the `SA_RESTART` arm, which waits the bound out: the
/// bound is `DRAIN_WAIT_MS`, three seconds, and a machine under load may add
/// to it.
const BOUND_LOW_MS: f64 = 1500.0;
const BOUND_HIGH_MS: f64 = 8000.0;
/// How many non-blocking sends may go in before the fill gives up. A bound, not
/// a tolerance: the buffer is four packet slots deep, so a fill that has not
/// filled it after this many is a stack that is draining, and the program must
/// say so rather than loop.
const FILL_LIMIT: usize = 32;

/// The signal the alarm's handler records, so the park's `EINTR` can be told
/// apart from some other refusal.
static ALARM_SEEN: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_alarm(sig: libc::c_int, _info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    ALARM_SEEN.store(sig, Ordering::Relaxed);
}

/// Give `SIGALRM` a handler with or without `SA_RESTART` - the one flag that
/// separates the two halves of this program. musl fills the restorer slot and
/// sets `SA_RESTORER` itself, so no trampoline is this program's business.
fn aim_alarm(restart: bool) -> bool {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    act.sa_sigaction =
        on_alarm as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    act.sa_flags = libc::SA_SIGINFO | if restart { libc::SA_RESTART } else { 0 };
    // SAFETY: a fully filled `sigaction` for a number the wrapper serves.
    unsafe { libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut()) == 0 }
}

/// Arm the interval timer for `ms` from now, and clear any earlier one.
fn arm(ms: u64) -> bool {
    let next = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: (ms / 1000) as _,
            tv_usec: ((ms % 1000) * 1000) as _,
        },
    };
    let mut spent = next;
    // SAFETY: two `itimerval`s this process owns, and one of the three clocks
    // `setitimer` names.
    unsafe { libc::setitimer(libc::ITIMER_REAL, &next, &mut spent) == 0 }
}

fn now_ms() -> f64 {
    let ts = clock_gettime(ClockId::Monotonic);
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
}

fn main() {
    println!("t-udpblock");
    let mut r = Report::new();

    // A host run measures the host's buffer, not this kernel's (the module
    // comment says why). Named and not made.
    if host_run() {
        println!(
            "note: the send buffer being filled is this kernel's own size (4 slots x 4 KiB); a host run measures the host's 212992-byte SO_SNDBUF instead - named and not made"
        );
        r.finish();
    }

    let fd = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("a datagram socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("a datagram socket opens", true);
    let dead = SocketAddrV4::new(DEAD, PORT);
    let live = SocketAddrV4::new(LIVE, PORT);
    let payload = [0xa5u8; PAYLOAD];

    // 1. Fill the send buffer, one datagram at a time, without parking. The
    //    per-send `MSG_DONTWAIT` is what makes this the non-blocking answer -
    //    and what keeps the fill from parking, which is the very thing the
    //    blocking send below is there to observe.
    let mut filled = 0usize;
    let mut fill_errno = None;
    while filled < FILL_LIMIT {
        match sendto(&fd, &payload, SendFlags::DONTWAIT, &dead) {
            Ok(_) => filled += 1,
            Err(e) => {
                fill_errno = Some(e);
                break;
            }
        }
    }
    r.check(
        "the send buffer fills: at least one datagram goes in",
        filled >= 1,
    );
    r.check(
        "and the send after it is refused, not accepted",
        fill_errno == Some(Errno::AGAIN),
    );
    println!("note: {filled} datagrams of {PAYLOAD} bytes fit before the refusal");

    // 2. The non-blocking send in the filled state: `EAGAIN`, at once. This is
    //    the answer the kernel gave to *every* caller before the park existed.
    let t = now_ms();
    let nb = sendto(&fd, &payload, SendFlags::DONTWAIT, &dead);
    let nb_ms = now_ms() - t;
    r.check(
        "a non-blocking send on a full buffer is EAGAIN",
        nb.is_err(),
    );
    r.check("and it is immediate", nb_ms < 10.0);
    println!("note: the non-blocking send answered after {nb_ms:.3} ms");

    // 3. The blocking send in the same state: it waits. The alarm cuts the wait
    //    short, so the claim is about the *latency* and the answer together -
    //    an `EINTR` that arrives at once would be a refusal wearing a park's
    //    clothes.
    ALARM_SEEN.store(0, Ordering::Relaxed);
    r.check(
        "a SIGALRM handler installs without SA_RESTART",
        aim_alarm(false),
    );
    r.check("the alarm arms", arm(ALARM_MS));
    let t = now_ms();
    let blocking = sendto(&fd, &payload, SendFlags::empty(), &dead);
    let park_ms = now_ms() - t;
    let seen = ALARM_SEEN.load(Ordering::Relaxed);
    r.check(
        "a blocking send on a full buffer waits, and the alarm ends the wait with EINTR",
        blocking.is_err() && seen == libc::SIGALRM,
    );
    r.check(
        "the wait lasted the alarm's delay",
        park_ms >= PARK_LOW_MS && park_ms <= PARK_HIGH_MS,
    );
    println!("note: the parked send answered after {park_ms:.1} ms with the alarm's SIGALRM seen");

    // 4. With `SA_RESTART` the same send is restarted rather than answered, so
    //    it waits the bound out. The bound is the safety valve: the egress here
    //    never drains (the neighbour never resolves), so an unbounded park
    //    would hang this program, and Linux - whose device drops the frame -
    //    would have answered by now. `EAGAIN` after the bound is the named
    //    divergence, and it is the same answer the non-blocking caller got.
    r.check("the same handler installs with SA_RESTART", aim_alarm(true));
    r.check("the alarm arms again", arm(ALARM_MS));
    let t = now_ms();
    let restarted = sendto(&fd, &payload, SendFlags::empty(), &dead);
    let bound_ms = now_ms() - t;
    r.check(
        "a restarted blocking send waits the bound out and then answers EAGAIN",
        restarted == Err(Errno::AGAIN),
    );
    r.check(
        "and the bound is seconds, not forever",
        bound_ms >= BOUND_LOW_MS && bound_ms <= BOUND_HIGH_MS,
    );
    println!("note: the bound ran out after {bound_ms:.1} ms");

    // 5. The socket survived both: a fresh one sends to the gateway, which does
    //    answer ARP. The cut-short waits cost their datagrams and nothing else.
    let fresh = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(f) => f,
        Err(e) => {
            r.check(&format!("a second datagram socket opens ({e:?})"), false);
            r.finish();
        }
    };
    let live_send = sendto(&fresh, &payload, SendFlags::empty(), &live);
    r.check(
        "a fresh socket still sends to a peer that answers ARP",
        live_send.is_ok(),
    );

    r.finish();
}
