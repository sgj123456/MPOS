//! Neighbour resolution for the one door that reaches the device directly:
//! the raw-IP send.
//!
//! Every other socket leaves through smoltcp, and smoltcp resolves the next
//! hop itself - it keeps a `NeighborCache` and fills it from the traffic it
//! sends and receives (`iface::interface::ipv4`'s ARP path). The raw-IP door
//! cannot use that: a caller carrying its own IP header (`IPPROTO_RAW`, which
//! implies `IP_HDRINCL`) has to reach the **device** rather than a smoltcp
//! socket, because smoltcp's raw emit drops a queued packet whose protocol
//! byte is not the socket's (`socket/raw.rs`), and a DHCP client's packet is
//! UDP while the channel is bound to `IPPROTO_RAW` (`docs/notes/` §4.5). So
//! the frame is built here and handed straight to the NIC - and the one thing
//! smoltcp would have done on the way, the next-hop lookup, is done here
//! too.
//!
//! What "the next hop" means is Linux's routing rule, not a guess: a
//! destination inside one of eth0's own subnets is reached on the wire
//! directly, and anything else leaves by the router a matching route names
//! (longest prefix first). A destination no route can reach is
//! `ENETUNREACH`, which is measured on the host (an `IPPROTO_RAW` send with
//! no route answers `ENETUNREACH`; the same send once a route exists returns
//! the length). What is left after the route is the **link** address, and
//! that is ARP: the cache below is filled by the frames the NIC hands the
//! seam (`observe`), and filled on demand by a probe (`resolve`) when a
//! unicast send names a neighbour the cache does not yet hold.
//!
//! The wire format is smoltcp's `ArpRepr`/`ArpPacket` - the same parser the
//! IP layer already trusts - so this module adds no wire parser of its own
//! (and so no fuzz target is owed for one). What it adds is the *cache* and
//! the *probe*, the two places smoltcp's own private cache could not be
//! reached from.
//!
//! Linux's own answer for an unresolved neighbour is asynchronous - `sendto`
//! returns the length and the machine ARPs in the background, queueing the
//! frame - measured on the host. A synchronous stack has nowhere to queue it,
//! so the probe runs in place and the send answers truthfully: the length
//! when the neighbour answered (and the frame left), `EHOSTUNREACH` when it
//! did not. That is the one divergence, and it is the honest half of the pair:
//! returning a length for a frame that never left would be this kernel's
//! worst kind of answer (`AGENTS.md` §3.2).

use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetAddress, Ipv4Address};

/// One learned mapping, with the tick past which it is stale.
#[derive(Clone, Copy)]
struct Neighbour {
    ip: [u8; 4],
    mac: [u8; 6],
    expires: u64,
}

/// How long a learned mapping is trusted: the 60 s smoltcp gives its own
/// cache (`iface::neighbor::Cache::ENTRY_LIFETIME`). The wire's own view is
/// that a MAC may change (a failover, a re-cabled port), and a cache that
/// never expired would keep sending a unicast frame to a neighbour that is no
/// longer there.
const ENTRY_TTL_TICKS: u64 = 60 * crate::hal::TICK_HZ as u64;

/// How many mappings the cache holds. A machine talks to a handful of
/// neighbours; the bound exists so a link that streams ARP cannot grow it
/// without limit (`AGENTS.md` §3's worst-case rule).
const CACHE_MAX: usize = 16;

/// ARP retransmissions before the probe gives up, and the wait for one
/// reply. Three one-second probes is Linux's own default solicitation count
/// (`ucast_solicit`/`mcast_solicit`), and far more than a working link needs.
const PROBE_TRIES: usize = 3;
const PROBE_WAIT_TICKS: u64 = crate::hal::TICK_HZ as u64;

/// The learned mappings. Kept here rather than on the stack because the seam
/// that fills it (`NicDevice::receive`) runs *inside* the stack's critical
/// section - the same reason `packet.rs`'s tap keeps its own list - and
/// because there is one NIC: a per-interface cache would be a place to get
/// the interface wrong.
static CACHE: Mutex<RefCell<Vec<Neighbour>>> = Mutex::new(RefCell::new(Vec::new()));

/// Learn from one frame the NIC handed the seam. Only ARP.
///
/// An ARP packet names its sender on both sides of a request and a reply, and
/// a machine that answers us (or asks us) is about to be talked to, so the
/// mapping is learned from either - which is what smoltcp's own ARP path does
/// (`process_arp` fills from a request as well as a reply). A non-unicast
/// sender is refused: a frame whose source MAC is a broadcast or a multicast
/// group names no one host.
pub(crate) fn observe(frame: &[u8]) {
    let Some(eth) = crate::link::parse_ethernet(frame) else {
        return;
    };
    if eth.ethertype != crate::link::ARP_ETHERTYPE {
        return;
    }
    let Ok(packet) = ArpPacket::new_checked(eth.payload) else {
        return;
    };
    let Ok(ArpRepr::EthernetIpv4 {
        operation,
        source_hardware_addr,
        source_protocol_addr,
        ..
    }) = ArpRepr::parse(&packet)
    else {
        return;
    };
    if !matches!(operation, ArpOperation::Request | ArpOperation::Reply) {
        return;
    }
    let mac = source_hardware_addr.0;
    let ip = source_protocol_addr.octets();
    if mac[0] & 1 != 0 || ip == [0; 4] || ip[0] >= 224 {
        return;
    }
    learn(ip, mac);
}

/// The link address for `ip`, if one is cached and unexpired.
pub(crate) fn lookup(ip: [u8; 4]) -> Option<[u8; 6]> {
    let now = crate::hal::ticks();
    critical_section::with(|cs| {
        let mut cache = CACHE.borrow(cs).borrow_mut();
        // Expiry is a read's job, not a sweeper's: a stale entry is the same
        // as an absent one and drops here.
        cache.retain(|n| n.expires > now);
        cache.iter().find(|n| n.ip == ip).map(|n| n.mac)
    })
}

fn learn(ip: [u8; 4], mac: [u8; 6]) {
    let expires = crate::hal::ticks().saturating_add(ENTRY_TTL_TICKS);
    critical_section::with(|cs| {
        let mut cache = CACHE.borrow(cs).borrow_mut();
        if let Some(existing) = cache.iter_mut().find(|n| n.ip == ip) {
            existing.mac = mac;
            existing.expires = expires;
            return;
        }
        if cache.len() >= CACHE_MAX {
            // Evict the entry closest to expiring; a full cache must not be a
            // cache that stops learning.
            if let Some(idx) = (0..cache.len()).min_by_key(|&i| cache[i].expires) {
                cache.swap_remove(idx);
            }
        }
        cache.push(Neighbour { ip, mac, expires });
    });
}

/// The address a frame for `dst` must be delivered to at the link layer.
///
/// A destination inside one of eth0's own subnets is on the wire (`Some(dst)`
/// itself); anything else leaves by a router, the matching route with the
/// longest prefix winning. `None` is `ENETUNREACH`: no address, no route, or
/// no wire at all.
pub(crate) fn next_hop(dst: [u8; 4]) -> Option<[u8; 4]> {
    let addrs = super::ipv4_addrs();
    if addrs.is_empty() {
        return None;
    }
    if is_on_link(dst, &addrs) {
        return Some(dst);
    }
    let mut best: Option<(u8, [u8; 4])> = None;
    for route in super::routes() {
        if !prefix_matches(dst, route.dst, route.dst_len) {
            continue;
        }
        if best.is_none_or(|(len, _)| route.dst_len > len) {
            // A route with no gateway is the destination itself (a connected
            // route); one with a gateway is the router to hand it to. Linux
            // spells the direct case `0.0.0.0`, which this kernel's route
            // table writes as an unspecified gateway.
            let hop = if route.gateway == [0; 4] {
                dst
            } else {
                route.gateway
            };
            best = Some((route.dst_len, hop));
        }
    }
    best.map(|(_, hop)| hop)
}

/// `true` if `dst` is the all-ones broadcast of any of `addrs`' subnets (a
/// directed broadcast), which is reached at the link layer by the broadcast
/// MAC with no ARP. The limited broadcast (`255.255.255.255`) is smoltcp's
/// `Ipv4Address::is_broadcast`, checked by the caller before this.
pub(crate) fn is_subnet_broadcast(dst: [u8; 4]) -> bool {
    super::ipv4_addrs()
        .iter()
        .any(|a| prefix_matches(dst, a.ip, a.prefix) && host_all_ones(dst, a.prefix))
}

/// Resolve `next_hop` to a MAC: the cache, or an ARP probe it answers.
///
/// `Err(err::EINTR)` if a signal ended a wait, `Err(err::EHOSTUNREACH)` if
/// every probe went unanswered, `Err(err::ENETDOWN)` with no card.
pub(crate) fn resolve(next_hop: [u8; 4]) -> Result<[u8; 6], isize> {
    if let Some(mac) = lookup(next_hop) {
        return Ok(mac);
    }
    let Some(mac) = super::packet_mac() else {
        return Err(crate::err::ENETDOWN);
    };
    let Some(ip) = super::primary_ipv4() else {
        return Err(crate::err::ENETUNREACH);
    };
    let request = request_frame(mac, ip, next_hop);
    for _ in 0..PROBE_TRIES {
        let _ = super::packet_transmit(&request);
        let deadline = crate::hal::ticks().saturating_add(PROBE_WAIT_TICKS);
        // The condition drives the stack so the reply reaches the seam and
        // `observe`; a learned mapping is checked outside the stack's lock.
        let learned = || {
            let _ = super::with_stack(|stack| stack.poll_once());
            lookup(next_hop).is_some()
        };
        match crate::sched::block_until_deadline(learned, deadline) {
            crate::sched::WaitEnd::Condition => {
                if let Some(mac) = lookup(next_hop) {
                    return Ok(mac);
                }
            }
            crate::sched::WaitEnd::Interrupted => return Err(crate::err::EINTR),
            crate::sched::WaitEnd::Deadline => {}
        }
    }
    Err(crate::err::EHOSTUNREACH)
}

/// The ARP request for `target`, as a whole Ethernet frame: source `mac`, the
/// sender's protocol address `ip`, and a zero target MAC - the reply fills in
/// what we ask for.
fn request_frame(mac: [u8; 6], ip: [u8; 4], target: [u8; 4]) -> Vec<u8> {
    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(mac),
        source_protocol_addr: Ipv4Address::from_octets(ip),
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: Ipv4Address::from_octets(target),
    };
    let mut buf = alloc::vec![0u8; repr.buffer_len()];
    let mut packet = ArpPacket::new_unchecked(&mut buf[..]);
    repr.emit(&mut packet);
    crate::link::encode_ethernet([0xff; 6], mac, crate::link::ARP_ETHERTYPE, &buf)
}

/// `true` if `dst` shares the first `prefix` bits with `net`.
fn prefix_matches(dst: [u8; 4], net: [u8; 4], prefix: u8) -> bool {
    let bits = prefix.min(32) as u32;
    // `checked_shl` for the `/0` case: a zero-length prefix masks nothing, and
    // shifting by 32 is the one place a plain `<<` would be undefined.
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    let dst = u32::from_be_bytes(dst) & mask;
    let net = u32::from_be_bytes(net) & mask;
    dst == net
}

/// `true` if every host bit of `dst` (the bits `prefix` leaves) is one.
fn host_all_ones(dst: [u8; 4], prefix: u8) -> bool {
    let bits = prefix.min(32) as u32;
    if bits >= 32 {
        return false;
    }
    let host_mask = u32::MAX >> bits;
    u32::from_be_bytes(dst) & host_mask == host_mask
}

fn is_on_link(dst: [u8; 4], addrs: &[super::AddrFacts]) -> bool {
    addrs.iter().any(|a| prefix_matches(dst, a.ip, a.prefix))
}
