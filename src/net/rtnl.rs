//! The rtnetlink half of netlink: the read side `ip` and friends speak.
//!
//! Netlink is a socket family whose payload is a little message protocol of
//! its own. The uevent half carries one fixed shape of event; this half
//! answers *queries* - `ip addr` sends a `RTM_GETLINK` dump request and keeps
//! receiving until a `NLMSG_DONE`, so the answer is built here, one message
//! per interface, out of what the net layer knows.
//!
//! There is no write side: requests that mutate (add an address, bring a
//! link down) are acknowledged with success and dropped, which is what a
//! caller with nothing to re-read after the fact cannot tell from done.

use alloc::{collections::VecDeque, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Message types of the route family, as `linux/rtnetlink.h` spells them.
pub const NETLINK_ROUTE: u32 = 0;

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETADDR: u16 = 22;
/// `RTM_GETROUTE`: what a caller hunting the default gateway's interface
/// sends. An empty answer here is worse than a wrong one - fastfetch reads
/// the empty dump and spins in userland forever, where not even a signal
/// can reach it.
const RTM_GETROUTE: u16 = 26;
const NLMSG_DONE: u16 = 3;
const NLM_F_MULTI: u16 = 2;

/// Attribute types of the route attribute family.
const IFLA_IFNAME: u16 = 3;
const IFLA_ADDRESS: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const RTA_GATEWAY: u16 = 2;
const RTA_OIF: u16 = 4;

/// Link flag bits, as `linux/if.h` spells them.
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;

/// ARPHRD_ values: what an interface's `type` field says the link layer is.
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

/// QEMU's default NIC address: the one the stack's static configuration
/// assumes, so `ip addr` and the stack agree about what eth0 carries.
const ETH0_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x8000_0000);

struct Session {
    /// Answers to the last dump request, one netlink message each.
    queue: VecDeque<Vec<u8>>,
}

static TABLE: critical_section::Mutex<core::cell::RefCell<Vec<(usize, Session)>>> =
    critical_section::Mutex::new(core::cell::RefCell::new(Vec::new()));

/// Open a route-netlink session, as `socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)`
/// does, and answer the handle that names it.
pub fn open() -> usize {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    critical_section::with(|cs| {
        TABLE.borrow(cs).borrow_mut().push((
            handle,
            Session {
                queue: VecDeque::new(),
            },
        ));
    });
    handle
}

/// Whether `handle` names a route session - how the syscall layer tells this
/// family's descriptors from the network stack's and the uevent socket's.
pub fn exists(handle: usize) -> bool {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow()
            .iter()
            .any(|(h, _)| *h == handle)
    })
}

/// Whether something is waiting to be read.
pub fn ready(handle: usize) -> bool {
    with_session(handle, |s| !s.queue.is_empty()).unwrap_or(false)
}

/// Take one queued answer, oldest first.
pub fn recv(handle: usize) -> Option<Vec<u8>> {
    with_session(handle, |s| s.queue.pop_front()).flatten()
}

/// [`recv`](Self::recv) with a park: a dump reader with nothing queued waits
/// here instead of spinning, and the next generated answer wakes it.
pub fn recv_blocking(handle: usize) -> Option<Vec<u8>> {
    crate::sched::block_until(|| ready(handle));
    recv(handle)
}

/// Feed one request in: the payload of a `sendto`/`sendmsg` on a route
/// socket. The message's first sixteen bytes are its header; the type picks
/// the dump to generate, and the sequence number travels back on every
/// answer so the caller can pair them.
pub fn send(handle: usize, msg: &[u8]) -> isize {
    if msg.len() < 16 {
        return crate::syscall::err::EINVAL;
    }
    let ty = u16::from_le_bytes([msg[4], msg[5]]);
    let seq = u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]);
    let answers: Vec<Vec<u8>> = match ty {
        RTM_GETLINK => {
            let mut v = Vec::new();
            v.push(link_msg(
                seq,
                1,
                "lo",
                IFF_UP | IFF_LOOPBACK | IFF_RUNNING,
                ARPHRD_LOOPBACK,
                None,
            ));
            v.push(link_msg(
                seq,
                2,
                "eth0",
                IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST,
                ARPHRD_ETHER,
                Some(&ETH0_MAC),
            ));
            v.push(done(seq));
            v
        }
        RTM_GETADDR => {
            let mut v = Vec::new();
            v.push(addr_msg(seq, 1, 8, [127, 0, 0, 1], "lo", 254));
            v.push(addr_msg(seq, 2, 24, crate::net::LOCAL_IP, "eth0", 0));
            v.push(done(seq));
            v
        }
        RTM_GETROUTE => {
            // The default route, exactly as the static configuration implies:
            // everything leaves by the gateway over eth0. A dump with only an
            // end-marker makes callers that hunt for a default interface spin.
            let mut v = Vec::new();
            v.push(route_msg(seq, 2, crate::net::GATEWAY_IP));
            v.push(done(seq));
            v
        }
        // Mutations and the query types nobody here can answer: the request
        // is accepted and nothing comes back but the end marker, which is
        // what a caller looping on a dump needs to stop.
        _ => vec![done(seq)],
    };
    critical_section::with(|cs| {
        if let Some((_, session)) = TABLE
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|(h, _)| *h == handle)
        {
            for answer in answers {
                session.queue.push_back(answer);
            }
        }
    });
    msg.len() as isize
}

/// Run `f` against `handle`'s session.
fn with_session<R>(
    handle: usize,
    f: impl FnOnce(&mut Session) -> R,
) -> Option<R> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|(h, _)| *h == handle)
            .map(|(_, session)| f(session))
    })
}

/// One `nlmsghdr` wrapped around `payload`.
fn nlmsg(ty: u16, seq: u32, payload: &[u8], multi: bool) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut m = Vec::with_capacity(len);
    m.extend((len as u32).to_le_bytes());
    m.extend(ty.to_le_bytes());
    m.extend((multi as u16 * NLM_F_MULTI).to_le_bytes());
    m.extend(seq.to_le_bytes());
    m.extend(0u32.to_le_bytes()); // nlmsg_pid: the kernel sends with 0
    m.extend(payload);
    m
}

fn done(seq: u32) -> Vec<u8> {
    nlmsg(NLMSG_DONE, seq, &[], true)
}

/// One route attribute, padded to the four-byte alignment netlink keeps.
fn rtattr(ty: u16, payload: &[u8]) -> Vec<u8> {
    let mut a = Vec::new();
    a.extend(((4 + payload.len()) as u16).to_le_bytes());
    a.extend(ty.to_le_bytes());
    a.extend(payload);
    while a.len() % 4 != 0 {
        a.push(0);
    }
    a
}

fn name_with_nul(name: &str) -> Vec<u8> {
    let mut s = Vec::from(name.as_bytes());
    s.push(0);
    s
}

/// `RTM_NEWLINK`: an `ifinfomsg` plus the interface's name (and address).
fn link_msg(
    seq: u32,
    index: u32,
    name: &str,
    flags: u32,
    link_type: u16,
    mac: Option<&[u8; 6]>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0); // ifi_family: unspecified on a dump answer
    payload.push(0); // padding
    payload.extend(link_type.to_le_bytes());
    payload.extend(index.to_le_bytes());
    payload.extend(flags.to_le_bytes());
    payload.extend(0u32.to_le_bytes()); // ifi_change
    payload.extend(rtattr(IFLA_IFNAME, &name_with_nul(name)));
    if let Some(mac) = mac {
        payload.extend(rtattr(IFLA_ADDRESS, mac));
    }
    nlmsg(RTM_NEWLINK, seq, &payload, true)
}

/// `RTM_NEWROUTE`: a `rtmsg` plus the gateway and outgoing interface. This
/// one route is the default route - `dst_len` 0 means "everything else".
fn route_msg(seq: u32, oif: u32, gateway: [u8; 4]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // rtm_family: AF_INET
    payload.push(0); // rtm_dst_len: 0 = the default route
    payload.push(0); // rtm_src_len
    payload.push(0); // rtm_tos
    payload.push(254); // rtm_table: RT_TABLE_MAIN
    payload.push(4); // rtm_protocol: RTPROT_STATIC
    payload.push(0); // rtm_scope: RT_SCOPE_UNIVERSE
    payload.push(1); // rtm_type: RTN_UNICAST
    payload.extend(rtattr(RTA_GATEWAY, &gateway));
    payload.extend(rtattr(RTA_OIF, &oif.to_le_bytes()));
    nlmsg(RTM_NEWROUTE, seq, &payload, true)
}

/// `RTM_NEWADDR`: an `ifaddrmsg` plus the address, stated twice the way
/// Linux does it - `IFA_ADDRESS` for the prefix, `IFA_LOCAL` for the local
/// end - and the interface's name.
fn addr_msg(seq: u32, index: u32, prefix: u8, ip: [u8; 4], name: &str, scope: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // ifa_family: AF_INET
    payload.push(prefix);
    payload.push(0); // ifa_flags
    payload.push(scope);
    payload.extend(index.to_le_bytes());
    payload.extend(rtattr(IFA_ADDRESS, &ip));
    payload.extend(rtattr(IFA_LOCAL, &ip));
    payload.extend(rtattr(IFA_LABEL, &name_with_nul(name)));
    nlmsg(RTM_NEWADDR, seq, &payload, true)
}
