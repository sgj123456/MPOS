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

/// `NLMSG_ERROR`: the one message that is both an answer and a report. An
/// `error` field of zero *is* the success report, and that is the only thing
/// a mutation gets back: `ip addr add` sends its request, waits for exactly
/// this, and reads anything else as noise. Answering a mutation with
/// `NLMSG_DONE` instead is what made busybox `ip` print `unexpected reply!`
/// and then sit in `recv` for ever - an `ip` command that never returns hangs
/// every script that runs one.
const NLMSG_ERROR: u16 = 2;

/// "Answer me." A caller that asks for this waits for the report; a caller
/// that does not is not listening, and an answer nobody reads is a reply the
/// *next* command will read by mistake. A dump - `NLM_F_REQUEST | NLM_F_DUMP`
/// on a `GET` - does not ask: its answer is the run of messages itself,
/// closed by `NLMSG_DONE`, and a report on top of it would be noise.
const NLM_F_ACK: u16 = 0x04;

/// Attribute types of the route attribute family.
const IFLA_IFNAME: u16 = 3;
const IFLA_ADDRESS: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;

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
    /// The netlink port id this session answers under. Zero means unbound:
    /// Linux draws one when the socket `bind`s - an explicit nonzero request
    /// is honored, a zero request is filled in - and a socket that never
    /// binds is auto-bound at its first send. The distinction is load-bearing
    /// for callers that filter answers by `nlmsg_pid` against their own name
    /// (fastfetch's default-route probe does exactly that): an answer stamped
    /// with anything else reads as somebody else's mail and is dropped.
    portid: u32,
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
                portid: 0,
                queue: VecDeque::new(),
            },
        ));
    });
    handle
}

/// Name the session, as `bind(fd, {AF_NETLINK, nl_pid, ...})` asks. An
/// explicit nonzero `nl_pid` is honored unless another session holds it; zero
/// asks the kernel to pick, which Linux does with the binding thread's process
/// id (`netlink_autobind`), falling back to a fresh number only on collision.
/// Returns an errno or zero.
pub fn bind(handle: usize, requested: u32) -> isize {
    // Read the caller's process id before the critical section: autobinding a
    // nameless socket to it is what makes a reply match the requester's own
    // `getpid()` check.
    let tgid = crate::sched::current_tgid().0 as u32;
    critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        if requested != 0
            && table
                .iter()
                .any(|(h, s)| *h != handle && s.portid == requested)
        {
            return crate::syscall::err::EADDRINUSE;
        }
        // Already named? Linux keeps the first binding. Unknown handle? The
        // second pass below misses it and says so.
        if table
            .iter()
            .find(|(h, _)| *h == handle)
            .is_some_and(|(_, s)| s.portid != 0)
        {
            return 0;
        }
        // Draw the id while the table is only read: the mutable assignment
        // comes after.
        let pid = if requested != 0 {
            requested
        } else {
            auto_portid(&table, handle, tgid)
        };
        match table.iter_mut().find(|(h, _)| *h == handle) {
            Some((_, session)) => session.portid = pid,
            None => return crate::syscall::err::EBADF,
        }
        0
    })
}

/// The next port id no other session holds.
fn free_portid(table: &[(usize, Session)], handle: usize) -> u32 {
    let mut candidate = 1u32;
    while table
        .iter()
        .any(|(h, s)| *h != handle && s.portid == candidate)
    {
        candidate += 1;
    }
    candidate
}

/// The port id Linux's `netlink_autobind` hands a socket the caller never named:
/// the binding thread's *process* id, and only if that is already taken by
/// another socket does it fall back to a fresh number. This is not cosmetic -
/// callers that authenticate a reply by matching its `nlmsg_pid` against their
/// own `getpid()` (fastfetch's local-IP probe among them) silently discard every
/// message whose id is an unrelated small counter. `tgid` is read by the caller
/// before the critical section; `0` cannot be an autobind id (`0` means unbound).
fn auto_portid(table: &[(usize, Session)], handle: usize, tgid: u32) -> u32 {
    let taken = |id: u32| table.iter().any(|(h, s)| *h != handle && s.portid == id);
    if tgid != 0 && !taken(tgid) {
        tgid
    } else {
        free_portid(table, handle)
    }
}

/// The session's port id - zero while it is unbound, which is what Linux's
/// `getsockname` answers for a netlink socket nothing has named yet.
pub fn portid(handle: usize) -> Option<u32> {
    with_session(handle, |s| s.portid)
}

/// Whether `handle` names a route session - how the syscall layer tells this
/// family's descriptors from the network stack's and the uevent socket's.
pub fn exists(handle: usize) -> bool {
    critical_section::with(|cs| TABLE.borrow(cs).borrow().iter().any(|(h, _)| *h == handle))
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
    let flags = u16::from_le_bytes([msg[6], msg[7]]);
    let seq = u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]);
    // Linux auto-binds a netlink socket at its first send - musl's
    // getifaddrs never binds at all - so by the time the answers are built
    // there is a port id for them to carry, and that id is the sender's
    // process id, exactly as `netlink_autobind` would give it. Every answer is
    // stamped with it: a reader that matches `nlmsg_pid` against its own
    // `getpid()` (fastfetch's local-IP probe) otherwise reads every message as
    // foreign mail, never sees the end marker, and waits forever for more.
    let tgid = crate::sched::current_tgid().0 as u32;
    let portid = critical_section::with(|cs| {
        let mut table = TABLE.borrow(cs).borrow_mut();
        let unbound = table
            .iter()
            .find(|(h, _)| *h == handle)
            .is_some_and(|(_, s)| s.portid == 0);
        if unbound {
            // Draw the id before taking the mutable borrow: the scan that
            // looks for a free one reads the whole table.
            let fresh = auto_portid(&table, handle, tgid);
            table
                .iter_mut()
                .find(|(h, _)| *h == handle)
                .expect("send on a route session the table no longer names")
                .1
                .portid = fresh;
            fresh
        } else {
            table
                .iter()
                .find(|(h, _)| *h == handle)
                .map(|(_, s)| s.portid)
                .expect("send on a route session the table no longer names")
        }
    });
    let answers: Vec<Vec<u8>> = match ty {
        RTM_GETLINK => {
            let mut v = Vec::new();
            v.push(link_msg(
                seq,
                portid,
                1,
                "lo",
                IFF_UP | IFF_LOOPBACK | IFF_RUNNING,
                ARPHRD_LOOPBACK,
                None,
            ));
            v.push(link_msg(
                seq,
                portid,
                2,
                "eth0",
                IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST,
                ARPHRD_ETHER,
                Some(&ETH0_MAC),
            ));
            v.push(done(seq, portid));
            v
        }
        RTM_GETADDR => {
            let mut v = Vec::new();
            v.push(addr_msg(seq, portid, 1, 8, [127, 0, 0, 1], "lo", 254));
            v.push(addr_msg(
                seq,
                portid,
                2,
                24,
                crate::net::LOCAL_IP,
                "eth0",
                0,
            ));
            v.push(done(seq, portid));
            v
        }
        RTM_GETROUTE => {
            // The default route, exactly as the static configuration implies:
            // everything leaves by the gateway over eth0 (index 2), and the
            // preferred source is eth0's own address. A dump with only an
            // end-marker makes callers that hunt for a default interface spin,
            // and a dump missing `RTA_PREFSRC` makes callers like fastfetch
            // print an empty LocalIp.
            let mut v = Vec::new();
            v.push(route_msg(
                seq,
                portid,
                2,
                crate::net::GATEWAY_IP,
                crate::net::LOCAL_IP,
            ));
            v.push(done(seq, portid));
            v
        }
        // Mutations and the query types nobody here can answer.
        //
        // A mutation is not a dump, so it gets no run of messages and no end
        // marker - it gets the report a caller that said `NLM_F_ACK` is
        // waiting for, which is an `NLMSG_ERROR` carrying error 0. Nothing
        // else satisfies `rtnl_talk`: it reads one message, insists it be
        // `NLMSG_ERROR`, and loops for another if it is not - so an end
        // marker here is a hang, not an answer.
        //
        // Without `NLM_F_ACK` there is no answer at all, which is what Linux
        // does: nobody is listening, and a message left in the queue is one
        // the next caller on this socket will read as its own.
        _ => {
            if flags & NLM_F_ACK != 0 {
                vec![ack(seq, portid, &msg[..16])]
            } else {
                Vec::new()
            }
        }
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
fn with_session<R>(handle: usize, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    critical_section::with(|cs| {
        TABLE
            .borrow(cs)
            .borrow_mut()
            .iter_mut()
            .find(|(h, _)| *h == handle)
            .map(|(_, session)| f(session))
    })
}

/// One `nlmsghdr` wrapped around `payload`. `pid` is the port id of the
/// session the answer goes to - what Linux sets, and what a caller matching
/// answers against its own name compares.
fn nlmsg(ty: u16, seq: u32, pid: u32, payload: &[u8], multi: bool) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut m = Vec::with_capacity(len);
    m.extend((len as u32).to_le_bytes());
    m.extend(ty.to_le_bytes());
    m.extend((multi as u16 * NLM_F_MULTI).to_le_bytes());
    m.extend(seq.to_le_bytes());
    m.extend(pid.to_le_bytes());
    m.extend(payload);
    m
}

fn done(seq: u32, pid: u32) -> Vec<u8> {
    nlmsg(NLMSG_DONE, seq, pid, &[], true)
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

/// The success report: `NLMSG_ERROR` with an `error` of zero, followed by the
/// header of the message it answers - which is how the caller pairs the two,
/// since `nlmsg_seq` alone is shared with every request on the socket.
fn ack(seq: u32, pid: u32, request: &[u8]) -> Vec<u8> {
    let mut payload = 0i32.to_le_bytes().to_vec();
    payload.extend_from_slice(&request[..16.min(request.len())]);
    nlmsg(NLMSG_ERROR, seq, pid, &payload, false)
}

/// `RTM_NEWLINK`: an `ifinfomsg` plus the interface's name (and address).
fn link_msg(
    seq: u32,
    pid: u32,
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
    nlmsg(RTM_NEWLINK, seq, pid, &payload, true)
}

/// `RTM_NEWROUTE`: a `rtmsg` plus the four attributes a default route needs
/// to be useful to a caller. `dst_len` 0 makes this "everything else", so
/// `RTA_DST` is the network address 0.0.0.0; `RTA_GATEWAY` is the next hop;
/// `RTA_OIF` is the outgoing interface index, which is how a caller ties the
/// route to a named device; and `RTA_PREFSRC` is the kernel's preferred source
/// address for the route - the value `fastfetch` and friends read as the
/// machine's local IP. Omit any of them and a strict caller drops the whole
/// reply rather than guess.
fn route_msg(
    seq: u32,
    pid: u32,
    oif: u32,
    gateway: [u8; 4],
    prefsrc: [u8; 4],
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // rtm_family: AF_INET
    payload.push(0); // rtm_dst_len: 0 = the default route
    payload.push(0); // rtm_src_len
    payload.push(0); // rtm_tos
    payload.push(254); // rtm_table: RT_TABLE_MAIN
    payload.push(4); // rtm_protocol: RTPROT_STATIC
    payload.push(0); // rtm_scope: RT_SCOPE_UNIVERSE
    payload.push(1); // rtm_type: RTN_UNICAST
    // rtm_flags closes `struct rtmsg` at 12 bytes. Skipping it shifts every
    // attribute four bytes early: a strict caller reads the four attribute
    // bytes that should have been flags as `rta_len`, sees the zero payload of
    // the next attribute, and reports `deficit N, rta_len=0!`.
    payload.extend(0u32.to_le_bytes());
    payload.extend(rtattr(RTA_DST, &[0u8; 4]));
    payload.extend(rtattr(RTA_GATEWAY, &gateway));
    payload.extend(rtattr(RTA_OIF, &oif.to_le_bytes()));
    payload.extend(rtattr(RTA_PREFSRC, &prefsrc));
    nlmsg(RTM_NEWROUTE, seq, pid, &payload, true)
}

/// `RTM_NEWADDR`: an `ifaddrmsg` plus the address, stated twice the way
/// Linux does it - `IFA_ADDRESS` for the prefix, `IFA_LOCAL` for the local
/// end - and the interface's name.
fn addr_msg(
    seq: u32,
    pid: u32,
    index: u32,
    prefix: u8,
    ip: [u8; 4],
    name: &str,
    scope: u8,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // ifa_family: AF_INET
    payload.push(prefix);
    payload.push(0); // ifa_flags
    payload.push(scope);
    payload.extend(index.to_le_bytes());
    payload.extend(rtattr(IFA_ADDRESS, &ip));
    payload.extend(rtattr(IFA_LOCAL, &ip));
    payload.extend(rtattr(IFA_LABEL, &name_with_nul(name)));
    nlmsg(RTM_NEWADDR, seq, pid, &payload, true)
}
