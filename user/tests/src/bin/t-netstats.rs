//! t-netstats — §10.4's instrument: `/proc/net/dev`'s counters, read through
//! the door they are promised on and pinned against a churned datagram count.
//!
//! The gap this closes is written down in `docs/notes/` §10.4: the frame pool's
//! two remaining per-frame allocations (the seam's TX build and its RX copy)
//! could not be landed with a number, because nothing counted frames. This
//! program is that count's gate half; the rate it prints beside the claims is
//! the before/after figure the pool itself will be measured with, the way
//! `t-pipecost` gave the pipe its MiB/s and `t-diskbench` gives the block cache
//! its cold/warm pair.
//!
//! # What is claimed, and why each claim can go red
//!
//! * **The file exists and parses**: Linux's two header lines, then one row per
//!   interface, each with sixteen right-justified counters at fixed column
//!   ends - the shape `vfs/procfs`'s own host test pins byte for byte against
//!   real Linux, and what a booted reader proves end to end.
//! * **One sendto is exactly one frame**: N UDP datagrams of a fixed size put
//!   `tx_packets` forward by exactly N. This is the claim that makes the rest
//!   trustworthy - a counter that double-counted inline polls, or counted
//!   smoltcp's internal retries as extra frames, fails here rather than
//!   quietly inflating the rate below. A datagram lost on a full queue still
//!   counts: Linux's `tx_packets` counts frames handed to the driver, and UDP
//!   over slirp is lossy by contract, so the claim holds whether or not the
//!   host answers. **The burst retries `EAGAIN`**, because this kernel answers
//!   that when the socket's send buffer is momentarily full rather than waiting
//!   for the device to drain (it has no wakeup source for a completion —
//!   `docs/uapi/` §5.2, `docs/notes/` §4.3); the retries are printed as a
//!   measurement, not asserted, and a bounded runaway is a failure rather than
//!   a hang.
//! * **The numbers move only where the kernel says they may**: reading twice
//!   with no traffic between keeps every counter steady (a machine whose poll
//!   loop fabricated frames would fail this), and `lo` stays zero while eth0
//!   counts - the seam cannot see loopback traffic, and the file says so.
//! * **The pool serves a full-MTU frame from a slot**: after a first burst has
//!   filled the free lists, a second identical burst moves `pool_hits` by at
//!   least BURST - one hit per TX take, because smoltcp builds exactly one
//!   frame per datagram here and a served take is what a hit counts. The
//!   counterfactual is the policy's off-by-one (`<` where the rule is `<=`, or
//!   a slot width computed without the Ethernet header): every frame then falls
//!   back to a fresh allocation, hits stay put, and this claim goes red on the
//!   machine. It is the pin `fits_pool` names in its doc comment - a host test
//!   could not run it, because this file lives behind the machine gate.
//!
//! The **rate** (`datagrams=<n>/s` over the timed burst) is printed, not
//! asserted: TCG-vs-KVM variance makes a throughput bound flaky, which is
//! exactly why `t-pipecost` reports and does not assert its MiB/s. §10.4's pool
//! lands with a pair of these printed numbers - one from the run before it,
//! one after - and that pair, not any threshold, is the measurement.
//!
//! # Why the peer is 10.0.2.2
//!
//! slirp's own gateway address: reachable-by-route without needing anything on
//! the host to answer (an unanswered UDP packet is still a transmitted frame,
//! which is all the counter claims). That keeps this a check of *this machine*
//! rather than of the internet, so it runs in the default sweep on both
//! machines - the same reasoning `tests.toml` records for leaving `t-dhcp` out
//! and this one in.
//!
//! # The library choices
//!
//! `rustix::net::{socket, sendto}` carry the sends; the counters come from
//! plain `std::fs::read_to_string("/proc/net/dev")`, whose exact bytes are the
//! claim. The clock is `rustix::time::clock_gettime(ClockId::Monotonic)` like
//! `t-netlat`'s. Host runs skip: real Linux has the file but not this kernel's
//! appended `pool=` word, and every claim here measures this stack's seam
//! rather than the host's (`t-netlat`'s "named and not made" rule).

use mpos_user_tests::Report;
use rustix::io::Errno;
use rustix::net::{AddressFamily, SendFlags, SocketAddrV4, SocketType, sendto, socket};
use rustix::time::{ClockId, clock_gettime};
use std::net::Ipv4Addr;

/// slirp's gateway: on-route on both machines' wiring, needs no listener.
const PEER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// discard(9) - if anything is listening there it drops the data.
const PORT: u16 = 9;
/// Datagrams per burst. 256 keeps the whole run well under a second even on
/// TCG while making an off-by-one or a double-count impossible to hide in
/// noise: the delta is asserted *exactly*.
const BURST: usize = 256;
/// One MTU-sized payload minus headers' worth of slack: big enough that a
/// frame is unambiguously one datagram, small enough never to fragment.
const PAYLOAD: usize = 1400;
/// How many `EAGAIN` answers one datagram may collect before the burst gives
/// up. A bound, not a tolerance: each retry costs one poll of the stack, and a
/// kernel that never accepts the datagram must fail this test rather than hang
/// it.
const RETRY_LIMIT: usize = 1000;
/// Ethernet (14) + IPv4 (20) + UDP (8): the bytes a payload picks up on its way
/// to the seam, which is where the frame counter counts.
const FRAME_HEADERS: usize = 42;

/// eth0's counters as the file's columns report them, plus the appended words.
#[derive(Clone)]
struct EthCounters {
    rx_packets: u64,
    tx_bytes: u64,
    tx_packets: u64,
    pool_hits: u64,
    pool_misses: u64,
    /// The link's MTU, from the row's own `mtu` column. The burst asks this
    /// because it is not the burst's to assume: the MTU is the *machine's*, a
    /// program that changes it changes it for the rest of the boot, and a
    /// datagram that does not fit is accepted into the socket's buffer and then
    /// never dispatched — which reads as a stack that stopped (measured: the
    /// sweep's `t-netlink` left eth0 at 1400, and this burst's 1400-byte payload
    /// plus 42 bytes of headers could not go out at all, 2000 `EAGAIN` retries
    /// and four frames at the seam).
    mtu: usize,
}

/// Read `/proc/net/dev` and pull eth0's row apart.
///
/// The parse is by *column position*, the way a real fixed-width reader does
/// it - which is part of the claim: parsing by whitespace-splitting would pass
/// on a renderer whose columns drifted, and the drift is what the byte-shape
/// test in `vfs/procfs.rs` refuses. Column ends are measured from Linux's own
/// file: rx bytes end at col 15, rx packets at 23, tx bytes at 74, tx packets
/// at 82. The appended words are read from the tail because they are named,
/// not positional.
fn read_eth() -> Option<(String, EthCounters)> {
    let text = std::fs::read_to_string("/proc/net/dev").ok()?;
    let mut lines = text.lines();
    let first = lines.next()?;
    let second = lines.next()?;
    if !first.starts_with("Inter-|") || !second.contains("face |bytes") {
        return None;
    }
    for line in lines {
        let name = line.split(':').next()?.trim().to_string();
        if name != "eth0" {
            continue;
        }
        // Each field is taken as *its own width*, ending at its column end, and
        // the width is load-bearing: the first draft used one nine-character
        // window for every field ("wide enough for an overflow"), which crosses
        // into the field on its left — on an ordinary boot the window ending at
        // rx packets' column reads `"1    1346"` — and `parse` refuses a string
        // with a space inside it, so the whole file read as unparseable and
        // every claim below was unreachable. A value that overflows its column
        // is a *host* case (a `wlan0` row's ten-digit byte count, months into an
        // uptime); the renderer's shape there is pinned where it belongs, by
        // `vfs::procfs`'s byte-shape test.
        let col = |end: usize, width: usize| -> Option<u64> {
            line.get(end.checked_sub(width)?..end)?.trim().parse().ok()
        };
        let tail = line.rsplit("mtu ").next()?;
        let (mtu_word, pool_word) = tail.split_once(" pool=")?;
        let mtu = mtu_word.trim().parse().ok()?;
        let (hits, misses) = pool_word.split_once(':')?;
        return Some((
            name,
            EthCounters {
                rx_packets: col(23, 7)?,
                tx_bytes: col(74, 8)?,
                tx_packets: col(82, 7)?,
                pool_hits: hits.parse().ok()?,
                pool_misses: misses.parse().ok()?,
                mtu,
            },
        ));
    }
    None
}

/// `lo`'s row, if the file carries one - and its counters must stay zero while
/// eth0's move, because loopback frames never cross the seam that counts.
fn lo_row() -> Option<EthCounters> {
    let text = std::fs::read_to_string("/proc/net/dev").ok()?;
    let line = text.lines().find(|l| l.trim_start().starts_with("lo:"))?;
    let col = |end: usize, width: usize| -> Option<u64> {
        line.get(end.checked_sub(width)?..end)?.trim().parse().ok()
    };
    Some(EthCounters {
        rx_packets: col(23, 7)?,
        tx_bytes: col(74, 8)?,
        tx_packets: col(82, 7)?,
        pool_hits: 0,
        pool_misses: 0,
        mtu: line
            .rsplit("mtu ")
            .next()?
            .split_once(' ')
            .and_then(|(word, _)| word.trim().parse().ok())?,
    })
}

/// Send `BURST` datagrams and hand back `(accepted, retries)`.
///
/// The retry is the point: when the socket's send buffer is full this kernel
/// answers `EAGAIN` — it has nothing to wait *on* for the device's completion,
/// which is the divergence `docs/uapi/` §5.2 and `docs/notes/` §4.3 carry —
/// where Linux's blocking sender waits for room. A caller does what any caller
/// does with `EAGAIN`: tries again. The retries are returned rather than
/// asserted, because a machine whose ring keeps up answers none.
fn burst(fd: &rustix::fd::OwnedFd, payload: &[u8], peer: &SocketAddrV4) -> (usize, usize) {
    let mut accepted = 0usize;
    let mut retries = 0usize;
    for _ in 0..BURST {
        let mut attempts = 0usize;
        loop {
            match sendto(fd, payload, SendFlags::empty(), peer) {
                Ok(n) if n == payload.len() => {
                    accepted += 1;
                    break;
                }
                Err(Errno::AGAIN) if attempts < RETRY_LIMIT => {
                    attempts += 1;
                    retries += 1;
                }
                _ => return (accepted, retries),
            }
        }
    }
    (accepted, retries)
}

/// Send datagrams until one *datagram-sized* frame leaves the seam, and hand
/// back whether that happened inside the budget.
///
/// This exists because of a **measured** property of the stack under test, not a
/// convenience: `smoltcp`'s `socket_meta.rs` carries
/// `DISCOVERY_SILENT_TIME = 1000 ms`, so a socket whose dispatch failed because
/// the neighbour was unknown is *skipped by the egress for a second* — its probe
/// (one ARP request, which is what the seam showed once per burst) is
/// rate-limited with it. A burst that begins inside that second sees every
/// `sendto` refused (`EAGAIN`) while the socket's buffer fills: measured here as
/// 2000 refusals and four frames at the seam, which reads exactly like a stack
/// that stopped, and cost this program four wrong diagnoses before the pattern
/// was printed out of the interface itself. Priming takes the question out of
/// the measurement: one datagram per turn, and the *datagram's* frame at the
/// seam — not the 42-byte probe's — is the link being ready. The bound is the
/// silent time plus slack, so a link that never carries the datagram fails a
/// claim rather than hanging the sweep.
fn prime(fd: &rustix::fd::OwnedFd, payload: &[u8], peer: &SocketAddrV4) -> bool {
    let Some((_, at_start)) = read_eth() else {
        return false;
    };
    for _ in 0..250 {
        let _ = sendto(fd, payload, SendFlags::empty(), peer);
        if let Some((_, now)) = read_eth()
            && now.tx_bytes.saturating_sub(at_start.tx_bytes) >= PAYLOAD as u64
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

fn main() {
    println!("t-netstats");
    let mut r = Report::new();

    if mpos_user_tests::host_run() {
        println!(
            "skip: the seam claims measure this kernel's seam, and a host's /proc/net/dev carries no pool= word - named and not made"
        );
        r.finish();
    }

    // ---- The door ----
    let before = match read_eth() {
        Some((name, counters)) => {
            r.check("/proc/net/dev exists and parses as Linux shapes it", true);
            r.check("and names eth0", name == "eth0");
            counters
        }
        None => {
            r.check("/proc/net/dev exists and parses as Linux shapes it", false);
            r.finish();
        }
    };
    // The burst's datagram has to fit the link it goes out on, and the link is
    // the machine's rather than this program's: an MTU some earlier program left
    // small makes `smoltcp` accept the datagram into the socket's buffer and
    // never dispatch it, which reads from here exactly like a stack that stopped
    // (measured - see `EthCounters::mtu`). Asking the question here names the
    // reason instead of hiding it behind a failed count below.
    r.check(
        "the link's MTU leaves room for the datagram the burst sends",
        before.mtu >= PAYLOAD + FRAME_HEADERS,
    );
    if before.mtu < PAYLOAD + FRAME_HEADERS {
        println!(
            "  the link MTU is {} and one datagram needs {}; refusing to read the burst as a stack failure",
            before.mtu,
            PAYLOAD + FRAME_HEADERS
        );
        r.finish();
    }

    // ---- Churn exactly BURST frames ----
    let fd = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("a UDP socket opens ({e:?})"), false);
            r.finish();
        }
    };
    let peer = SocketAddrV4::new(PEER, PORT);
    let payload = [0xa5u8; PAYLOAD];

    // ---- Prime the link, so the burst is not spent inside the neighbour's
    // silent time ----
    //
    // `smoltcp`'s `socket_meta.rs` carries `DISCOVERY_SILENT_TIME = 1000 ms`: a
    // socket whose dispatch failed because the neighbour was unknown is skipped
    // by the egress for a second, and the probe it fired (one ARP request per
    // second, which is what the seam showed) is rate-limited with it. A burst
    // inside that second sees every `sendto` refused while the socket's buffer
    // fills — measured: 2000 refusals, four frames at the seam. Priming is one
    // datagram at a time until the first frame appears, and the bound is the
    // silent time plus slack, so a link that never comes up fails a claim rather
    // than hanging the sweep.
    r.check(
        "the link's neighbour answers inside the budget (primed with one datagram)",
        prime(&fd, &payload, &peer),
    );

    // Re-baseline after the prime: the priming datagrams crossed the same
    // counters the burst is about to be measured against.
    let before = match read_eth() {
        Some(row) => row.1,
        None => {
            r.check("and the counters read back before the burst", false);
            r.finish();
        }
    };

    let start_clock = clock_gettime(ClockId::Monotonic);
    let (sent, retries) = burst(&fd, &payload, &peer);
    let end_clock = clock_gettime(ClockId::Monotonic);
    r.check(
        "every datagram was accepted by the stack (retrying where it answered EAGAIN)",
        sent == BURST,
    );

    let after = match read_eth() {
        Some(row) => row.1,
        None => {
            r.check("and the counters read back afterwards", false);
            r.finish();
        }
    };

    // ---- The exactness claims ----
    r.check(
        "one sendto moved tx_packets by exactly one",
        after.tx_packets - before.tx_packets == BURST as u64,
    );
    // Each frame carries the 1400-byte payload plus the Ethernet, IP and UDP
    // headers — 42 bytes, and the seam counts the *whole* frame, measured: the
    // diagnostic frame a 20-byte datagram made was 62 bytes (14 + 20 + 8 + 20).
    // The counterfactual is a renderer that counted the payload alone or padded
    // it to the MTU.
    r.check(
        "and tx_bytes by exactly the frame sizes",
        after.tx_bytes - before.tx_bytes == BURST as u64 * (PAYLOAD as u64 + FRAME_HEADERS as u64),
    );
    let now = read_eth().map(|row| row.1);
    match now {
        Some(now) => r.check(
            "reading the file again invents no frames",
            now.tx_packets == after.tx_packets && now.rx_packets >= after.rx_packets,
        ),
        None => r.check("the second read answers too", false),
    }
    match lo_row() {
        Some(lo) => r.check(
            "lo stays at zero while eth0 counts (loopback never crosses the seam)",
            lo.tx_packets == 0 && lo.rx_packets == 0,
        ),
        None => r.check("the file carries an lo row", false),
    }

    // ---- The pool's warm-up claim (§10.4's second half) ----
    //
    // The burst above filled the free lists - every TX buffer it took came
    // back at `consume`'s put - so a second identical burst must be served
    // from slots. `>= BURST` rather than `==` because the same window can
    // carry RX takes and whatever else the stack builds; the exactness that
    // matters is *nonzero movement*, which only a working `fits_pool` bound
    // produces on the first post-warm-up frame and never again if the policy
    // is off by one.
    let warm = read_eth().map(|row| row.1);
    let (sent2, retries2) = burst(&fd, &payload, &peer);
    let pooled = read_eth().map(|row| row.1);
    match (&warm, &pooled) {
        (Some(warm), Some(pooled)) => r.check(
            "a warmed pool serves a full-MTU frame from a slot",
            sent2 == BURST && pooled.pool_hits >= warm.pool_hits + BURST as u64,
        ),
        _ => r.check("the pool reads come back", false),
    }

    // ---- The rate, printed for §10.4's before/after pair ----
    //
    // Timed across *both* bursts: the figure the pool lands against is the
    // steady-state one over the whole run, and taking only the first burst's
    // window would compare a cold-pool half (before) with a warm one (after) -
    // two different measurements wearing the same name. `datagrams_per_sec`
    // stays the single-word field the sweep's reader parses; the first-burst
    // window is kept as its own line for anyone who wants the cold number too.
    let end_clock2 = clock_gettime(ClockId::Monotonic);
    let micros = (end_clock.tv_sec as u64 * 1_000_000 + end_clock.tv_nsec as u64 / 1_000)
        - (start_clock.tv_sec as u64 * 1_000_000 + start_clock.tv_nsec as u64 / 1_000);
    let per_sec_first = (BURST as u64 * 1_000_000).checked_div(micros).unwrap_or(0);
    let total = BURST as u64 + sent2 as u64;
    let micros2 = (end_clock2.tv_sec as u64 * 1_000_000 + end_clock2.tv_nsec as u64 / 1_000)
        - (start_clock.tv_sec as u64 * 1_000_000 + start_clock.tv_nsec as u64 / 1_000);
    let per_sec = (total * 1_000_000).checked_div(micros2).unwrap_or(0);
    let hits = pooled.as_ref().map(|p| p.pool_hits).unwrap_or(0);
    let misses = pooled.as_ref().map(|p| p.pool_misses).unwrap_or(0);
    println!("NETSTAT_FIRST tx={BURST} datagrams_per_sec={per_sec_first}");
    println!("NETSTAT tx={total} datagrams_per_sec={per_sec} pool={hits}:{misses}");
    if retries + retries2 > 0 {
        println!(
            "note: {} of {} sends were answered EAGAIN and retried - this kernel does not wait \
             for the send buffer to drain (`docs/uapi/` §5.2), where a blocking Linux sender would",
            retries + retries2,
            BURST * 2
        );
    }

    r.finish()
}
