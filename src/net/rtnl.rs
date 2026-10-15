//! The rtnetlink half of netlink: both halves of what `ip` speaks.
//!
//! Netlink is a socket family whose payload is a little message protocol of
//! its own. The uevent half carries one fixed shape of event; this half
//! answers *queries* - `ip addr` sends a `RTM_GETLINK` dump request and keeps
//! receiving until a `NLMSG_DONE`, so the answer is built here, one message
//! per interface, out of what the net layer knows.
//!
//! And it *takes* the mutations: `ip addr add`, `ip route add`, `ip link
//! set` are parsed here and applied to the live stack - the address and
//! route tables smoltcp consults, and the link's up flag that gates the
//! stack's polling. A mutation is answered with the `NLMSG_ERROR` report a
//! caller running `rtnl_talk` waits for, carrying zero on success or the
//! errno it failed with; a request that never asked for an ACK and did not
//! fail gets nothing back, exactly as Linux leaves an unread success.

use alloc::{collections::VecDeque, format, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Message types of the route family, as `linux/rtnetlink.h` spells them.
pub const NETLINK_ROUTE: u32 = 0;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
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

/// `NLM_F_REPLACE`: "settle your own conflict" - the caller of `ip route
/// replace` expects the destination's old entry to give way, where `add`
/// expects `EEXIST`.
const NLM_F_REPLACE: u16 = 0x100;

/// `NLM_F_CREATE`: "make one if there is none." On a link request with no
/// index it names `ip link add`, which this kernel cannot do.
const NLM_F_CREATE: u16 = 0x400;

/// Attribute types of the route attribute family, as `linux/if_link.h`
/// numbers them: `IFLA_ADDRESS` 1, `IFLA_BROADCAST` 2, `IFLA_IFNAME` 3,
/// `IFLA_MTU` 4, `IFLA_QDISC` 6 - the numbers are the contract, and getting
/// one wrong swaps whole fields (an MTU read as a broadcast address, a qdisc
/// read as a MAC).
const IFLA_IFNAME: u16 = 3;
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_MTU: u16 = 4;
/// The queueing discipline's name, as a string. `ip link` prints it; the
/// stack has no qdisc, which Linux spells "noqueue".
const IFLA_QDISC: u16 = 6;
/// The transmit queue length, as a native-order word.
const IFLA_TXQLEN: u16 = 13;
/// The two ways a link request can name the network namespace to move a device
/// *to*: by pid (`IFLA_NET_NS_PID`, a pid read in the caller's own namespace)
/// or by an open `/proc/<pid>/ns/net` descriptor (`IFLA_NET_NS_FD`). Numbered
/// as `linux/if_link.h` numbers them.
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_NET_NS_FD: u16 = 28;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
/// `IFA_F_PERMANENT`: the address was put there on purpose, not learned.
/// Without it busybox `ip addr` prints the address as `dynamic` - the mark
/// of an address a DHCP client or router advertisement left behind.
const IFA_F_PERMANENT: u8 = 0x80;
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

/// The interface indexes this kernel's single NIC and its loopback answer
/// under: `lo` is 1 and `eth0` is 2, the pair Linux's own enumeration of a
/// one-NIC machine would hand out.
const LO_INDEX: u32 = 1;
const ETH_INDEX: u32 = 2;

/// `AF_INET`, the one family the stack speaks.
const AF_INET: u8 = 2;

/// ARPHRD_ values: what an interface's `type` field says the link layer is.
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

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
            return crate::err::EADDRINUSE;
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
            None => return crate::err::EBADF,
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
/// here instead of spinning, and the next generated answer wakes it. A
/// catchable signal ends the wait - `Interrupted`, nothing consumed - so a
/// program that reads its route table and also answers its own timers is not
/// held by a queue nothing has filled yet. `Ok(None)` remains what it was:
/// the session is gone (its descriptor closed while this task waited).
pub fn recv_blocking(handle: usize) -> crate::vfs::VfsResult<Option<Vec<u8>>> {
    match crate::sched::block_until_interruptible(|| ready(handle)) {
        crate::sched::WaitEnd::Interrupted => return Err(crate::vfs::VfsError::Interrupted),
        _ => {}
    }
    Ok(recv(handle))
}

/// Feed one request in: the payload of a `sendto`/`sendmsg` on a route
/// socket. The message's first sixteen bytes are its header; the type picks
/// the dump to generate, and the sequence number travels back on every
/// answer so the caller can pair them.
pub fn send(handle: usize, msg: &[u8]) -> isize {
    if msg.len() < 16 {
        return crate::err::EINVAL;
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
        RTM_GETLINK => dump_links(seq, portid),
        RTM_GETADDR => dump_addrs(seq, portid),
        RTM_GETROUTE => dump_routes(seq, portid),
        // The mutations: parse, apply to the live stack, and answer with the
        // report. A failure is *always* answered - Linux sends `NLMSG_ERROR`
        // with the errno whether or not the caller asked for an ACK, because
        // the error is the reply, not a courtesy. A success without
        // `NLM_F_ACK` gets nothing: nobody is listening, and a message left
        // in the queue is one the next caller on this socket will read as
        // its own.
        RTM_NEWADDR | RTM_DELADDR | RTM_NEWROUTE | RTM_DELROUTE | RTM_NEWLINK | RTM_SETLINK
        | RTM_DELLINK => {
            let rc = apply(ty, flags, msg);
            if rc != 0 {
                vec![err_reply(seq, portid, rc, &msg[..16])]
            } else if flags & NLM_F_ACK != 0 {
                vec![ack(seq, portid, &msg[..16])]
            } else {
                Vec::new()
            }
        }
        // The query types nobody here can answer.
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

/// The failure report: same message shape, an `error` of the errno. The
/// field carries the errno **negated** - the kernel's own convention, which
/// is what `rtnl_talk` reads back through `errno = -err->error` before
/// printing. Writing it positive made busybox print the message for an
/// errno no table knows: "No error information".
fn err_reply(seq: u32, pid: u32, errno: isize, request: &[u8]) -> Vec<u8> {
    let mut payload = (errno as i32).to_le_bytes().to_vec();
    payload.extend_from_slice(&request[..16.min(request.len())]);
    nlmsg(NLMSG_ERROR, seq, pid, &payload, false)
}

/// Walk the attributes of one message: the bytes after its header and
/// family-specific struct, pair by `rta_len`/`rta_type` (a 4-byte head, then
/// the value, padded to four). `f` sees each attribute's type and value; an
/// attribute shorter than its own head ends the walk - there is no honest
/// way to step past one, and a malformed request is `EINVAL` wherever it
/// lands.
fn for_each_attr<F: FnMut(u16, &[u8])>(payload: &[u8], skip: usize, mut f: F) -> bool {
    let mut at = skip;
    while at + 4 <= payload.len() {
        let rta_len = u16::from_le_bytes([payload[at], payload[at + 1]]) as usize;
        if rta_len < 4 || at + rta_len > payload.len() {
            return false;
        }
        let ty = u16::from_le_bytes([payload[at + 2], payload[at + 3]]);
        f(ty, &payload[at + 4..at + rta_len]);
        at += (rta_len + 3) & !3;
    }
    true
}

/// A four-byte big-endian value out of an attribute, or nothing for a short
/// one. IP addresses and MTUs are the sizes the users of these attributes
/// send; anything else is a malformed request.
fn be32(value: &[u8]) -> Option<u32> {
    (value.len() == 4).then(|| u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

/// A four-byte little-endian value out of an attribute. The non-address
/// netlink integers - an MTU, a queue length - are native-order words,
/// which on this machine is little.
fn le32(value: &[u8]) -> Option<u32> {
    (value.len() == 4).then(|| u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

/// How a link request names the network namespace a device is being moved
/// into. Only one of the two ever appears, and a caller that sends both has
/// the last one win - which is what Linux's own parser does, since each arm
/// writes the same variable.
enum Move {
    /// A pid, read in the caller's own pid namespace: the task's net namespace
    /// is the destination. This is `ip link set eth0 netns <pid>`.
    Pid(u32),
    /// An open `/proc/<pid>/ns/net`: the descriptor `setns` would join, which
    /// is what `ip link set eth0 netns <name>` passes.
    Fd(u32),
}

/// Apply one mutation request to the live stack. Returns the errno (already
/// negated, the kernel's convention) - zero is success.
fn apply(ty: u16, flags: u16, msg: &[u8]) -> isize {
    match ty {
        RTM_NEWADDR => {
            // `struct ifaddrmsg` is eight bytes: family, prefix, flags,
            // scope, then the interface index.
            let Some(body) = msg.get(16..) else {
                return crate::err::EINVAL;
            };
            if body.len() < 8 {
                return crate::err::EINVAL;
            }
            if body[0] != AF_INET {
                // The one family the stack speaks. IPv6 address requests
                // would need the stack's IPv6 half, which is not built.
                return crate::err::EAFNOSUPPORT;
            }
            let index = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            if index != ETH_INDEX {
                // `lo` holds one address and it is not this kernel's to
                // change; any other index names no device at all.
                return if index == LO_INDEX {
                    crate::err::EOPNOTSUPP
                } else {
                    crate::err::ENODEV
                };
            }
            let mut addr = None;
            let ok = for_each_attr(body, 8, |ty, value| {
                // `IFA_LOCAL` names the address; `IFA_ADDRESS` is the
                // prefix's other end, which for a peerless point-to-point
                // link is the same word. Either one answers here, `LOCAL`
                // winning if both appear - which is what `ip addr add`
                // sends and what a plain `IFA_ADDRESS` caller expects.
                if ty == IFA_LOCAL || (ty == IFA_ADDRESS && addr.is_none()) {
                    addr = be32(value);
                }
            });
            if !ok {
                return crate::err::EINVAL;
            }
            let Some(addr) = addr else {
                // No address to add is Linux's `EINVAL` ("invalid argument
                // for a prefix with no address in it").
                return crate::err::EINVAL;
            };
            let prefix = body[1];
            if prefix > 32 {
                return crate::err::EINVAL;
            }
            let rc = crate::net::add_ipv4_addr(addr.to_be_bytes(), prefix);
            match rc {
                Ok(()) => 0,
                // `ip addr replace` is the caller settling its own conflict.
                Err(e) if e == crate::err::EEXIST && flags & NLM_F_REPLACE != 0 => {
                    let _ = crate::net::del_ipv4_addr(addr.to_be_bytes());
                    crate::net::add_ipv4_addr(addr.to_be_bytes(), prefix)
                        .err()
                        .unwrap_or(0)
                }
                Err(e) => e,
            }
        }
        RTM_DELADDR => {
            let Some(body) = msg.get(16..) else {
                return crate::err::EINVAL;
            };
            if body.len() < 8 {
                return crate::err::EINVAL;
            }
            if body[0] != AF_INET {
                return crate::err::EAFNOSUPPORT;
            }
            let index = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            if index != ETH_INDEX {
                return if index == LO_INDEX {
                    crate::err::EOPNOTSUPP
                } else {
                    crate::err::ENODEV
                };
            }
            let mut addr = None;
            let ok = for_each_attr(body, 8, |ty, value| {
                if ty == IFA_LOCAL || (ty == IFA_ADDRESS && addr.is_none()) {
                    addr = be32(value);
                }
            });
            if !ok {
                return crate::err::EINVAL;
            }
            let Some(addr) = addr else {
                return crate::err::EINVAL;
            };
            match crate::net::del_ipv4_addr(addr.to_be_bytes()) {
                Ok(()) => 0,
                Err(e) => e,
            }
        }
        RTM_NEWROUTE => {
            // `struct rtmsg` is twelve bytes: family, dst_len, src_len, tos,
            // table, protocol, scope, type, then four bytes of flags.
            let Some(body) = msg.get(16..) else {
                return crate::err::EINVAL;
            };
            if body.len() < 12 {
                return crate::err::EINVAL;
            }
            if body[0] != AF_INET {
                return crate::err::EAFNOSUPPORT;
            }
            let dst_len = body[1];
            if dst_len > 32 {
                return crate::err::EINVAL;
            }
            let mut dst = 0u32;
            let mut gateway = None;
            let ok = for_each_attr(body, 12, |ty, value| match ty {
                RTA_DST => dst = be32(value).unwrap_or(dst),
                RTA_GATEWAY => gateway = be32(value),
                _ => {}
            });
            if !ok {
                return crate::err::EINVAL;
            }
            let Some(gateway) = gateway else {
                // A route with no gateway names an on-link prefix - the
                // interface's own address is already exactly that, by
                // smoltcp's design, and a second spelling of it would be a
                // route the stack cannot express. Linux accepts one; this
                // kernel says what it means instead of pretending.
                return crate::err::EINVAL;
            };
            // The destination is the network, not the address: the host
            // bits a caller left set are not part of the prefix, and
            // keeping them would make the entry unfindable by the exact
            // match a later `del` asks for.
            let mask: u32 = if dst_len == 0 {
                0
            } else {
                u32::MAX << (32 - dst_len as u32)
            };
            let dst = dst & mask;
            let rc = crate::net::add_route(dst.to_be_bytes(), dst_len, gateway.to_be_bytes());
            match rc {
                Ok(()) => 0,
                Err(e) if e == crate::err::EEXIST && flags & NLM_F_REPLACE != 0 => {
                    let _ = crate::net::del_route(dst.to_be_bytes(), dst_len);
                    crate::net::add_route(dst.to_be_bytes(), dst_len, gateway.to_be_bytes())
                        .err()
                        .unwrap_or(0)
                }
                Err(e) => e,
            }
        }
        RTM_DELROUTE => {
            let Some(body) = msg.get(16..) else {
                return crate::err::EINVAL;
            };
            if body.len() < 12 {
                return crate::err::EINVAL;
            }
            if body[0] != AF_INET {
                return crate::err::EAFNOSUPPORT;
            }
            let dst_len = body[1];
            if dst_len > 32 {
                return crate::err::EINVAL;
            }
            let mut dst = 0u32;
            let ok = for_each_attr(body, 12, |ty, value| {
                if ty == RTA_DST {
                    dst = be32(value).unwrap_or(dst);
                }
            });
            if !ok {
                return crate::err::EINVAL;
            }
            // No `RTA_DST` at all is the default route - the prefix of
            // length zero, which is what `ip route del default` sends.
            let mask: u32 = if dst_len == 0 {
                0
            } else {
                u32::MAX << (32 - dst_len as u32)
            };
            match crate::net::del_route((dst & mask).to_be_bytes(), dst_len) {
                Ok(()) => 0,
                Err(e) => e,
            }
        }
        RTM_NEWLINK | RTM_SETLINK => {
            // `struct ifinfomsg` is sixteen bytes: family, two bytes of
            // padding, type, index, flags, change mask.
            let Some(body) = msg.get(16..) else {
                return crate::err::EINVAL;
            };
            if body.len() < 16 {
                return crate::err::EINVAL;
            }
            let index = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            if index == 0 && flags & NLM_F_CREATE != 0 {
                // `ip link add`: no device to create, and no way to pretend.
                return crate::err::EOPNOTSUPP;
            }
            if index != LO_INDEX && index != ETH_INDEX {
                return crate::err::ENODEV;
            }
            let change = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
            let mut up: Option<bool> = None;
            let mut mtu: Option<u32> = None;
            let mut name: Option<Vec<u8>> = None;
            // A request to move this device into another network namespace:
            // `ip link set eth0 netns N` carries one of these, and it is served
            // now - the wire travels, down and unconfigured, into the namespace
            // it names (`net::move_wire`, and §6.3 for the measurement that
            // says what "served" has to mean).
            //
            // Before any of that existed the attribute fell to the catch-all and
            // the request answered `0`: a script that ran the command believed
            // its wire had moved when nothing had, which is the worst answer
            // this ABI can give, and why this arm was written before the move
            // was.
            let mut move_to = None;
            let ok = for_each_attr(body, 16, |ty, value| {
                match ty {
                    IFLA_MTU => mtu = le32(value),
                    IFLA_NET_NS_PID => {
                        if let Some(pid) = le32(value) {
                            move_to = Some(Move::Pid(pid));
                        }
                    }
                    IFLA_NET_NS_FD => {
                        if let Some(fd) = le32(value) {
                            move_to = Some(Move::Fd(fd));
                        }
                    }
                    IFLA_IFNAME => {
                        // The NUL a label carries is part of the value, not
                        // the name.
                        let end = value.iter().position(|b| *b == 0).unwrap_or(value.len());
                        name = Some(value[..end].to_vec());
                    }
                    _ => {}
                }
            });
            if !ok {
                return crate::err::EINVAL;
            }
            if let Some(target) = move_to {
                // `lo` is not movable at all: Linux refuses a netns-local
                // device with `EINVAL` (measured: `ip link set lo netns N`
                // against a live namespace of another kind answers -22).
                if index == LO_INDEX {
                    return crate::err::EINVAL;
                }
                let to = match target {
                    // A pid read in the caller's own pid namespace, naming the
                    // task whose net namespace is the destination.
                    Move::Pid(pid) => {
                        match crate::sched::ns_of(pid as usize, crate::vfs::ns::Kind::Net) {
                            Some(crate::vfs::ns::Namespace::Net(id)) => Some(id),
                            // A pid that names no task: `ESRCH`, which is what
                            // Linux's `get_net_ns_by_pid` answers (measured:
                            // -3), and the reason this is not a silent `0`.
                            _ => return crate::err::ESRCH,
                        }
                    }
                    // An open `/proc/<pid>/ns/net`, the same object `setns`
                    // joins - so the resolution is `setns`'s, and so is the
                    // errno for a descriptor that is not one (measured: -22).
                    Move::Fd(fd) => {
                        let Ok(file) = crate::sched::current_fds().clone_at(fd as i32) else {
                            return crate::err::EBADF;
                        };
                        match file.inode().as_namespace() {
                            Some(crate::vfs::ns::Namespace::Net(id)) => Some(id),
                            _ => return crate::err::EINVAL,
                        }
                    }
                };
                // The device is looked up in the namespace the *caller* is in,
                // which is the one that owns it - a wire that has already left
                // answers `ENODEV` rather than following the caller around.
                return match crate::net::move_wire(crate::sched::current_net_ns(), to) {
                    Ok(()) => 0,
                    Err(e) => e,
                };
            }
            // The `ifi_change` mask is what says the request means its
            // flags: `ip link set eth0 down` sends flags with only IFF_UP
            // clear and change with IFF_UP set, while `ip link set eth0
            // name X` sends flags nobody should read.
            if change & IFF_UP != 0 {
                let requested = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                up = Some(requested & IFF_UP != 0);
            }
            if let Some(up) = up
                && let Err(e) = crate::net::set_link_up(index, up)
            {
                return e;
            }
            if let Some(mtu) = mtu {
                if index != ETH_INDEX {
                    return crate::err::EOPNOTSUPP;
                }
                if let Err(e) = crate::net::set_mtu(mtu) {
                    return e;
                }
            }
            if let Some(name) = name {
                if index != ETH_INDEX {
                    return crate::err::EOPNOTSUPP;
                }
                let Ok(name) = core::str::from_utf8(&name) else {
                    return crate::err::EINVAL;
                };
                if let Err(e) = crate::net::set_eth_name(name) {
                    return e;
                }
            }
            0
        }
        RTM_DELLINK => crate::err::EOPNOTSUPP,
        _ => crate::err::EOPNOTSUPP,
    }
}

/// `RTM_GETLINK`'s answer: one message per interface, from what the stack
/// reports. The loopback's flags and eth0's flags, name, MTU and address are
/// all live - a link brought down reports down, a renamed one reports its new
/// name, and nothing here is a constant the stack could disagree with.
fn dump_links(seq: u32, pid: u32) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let Some(facts) = crate::net::link_facts() else {
        return vec![done(seq, pid)];
    };
    v.push(link_msg(
        seq,
        pid,
        LO_INDEX,
        "lo",
        flags_of(facts.lo_up, true),
        ARPHRD_LOOPBACK,
        None,
        65536,
    ));
    // Only where there is a wire: a namespace that has none lists `lo` and
    // nothing else, which is what a machine with no NIC reports - an entry for a
    // device that does not exist would be a dump disagreeing with the stack it
    // describes.
    if facts.eth_present {
        v.push(link_msg(
            seq,
            pid,
            ETH_INDEX,
            &facts.eth_name,
            flags_of(facts.eth_up, false),
            ARPHRD_ETHER,
            Some(&facts.mac),
            facts.mtu,
        ));
    }
    v.push(done(seq, pid));
    v
}

/// An interface's flags: `IFF_UP` as the administrative bit says, and the
/// carrier-and-capability bits a link of that kind always carries. `RUNNING`
/// rides `UP` - a stack that is not polling a downed link has no carrier to
/// report, which is exactly what `ip link` prints `NO-CARRIER` from.
fn flags_of(up: bool, loopback: bool) -> u32 {
    let mut f = IFF_UP | IFF_RUNNING;
    if !up {
        f &= !(IFF_UP | IFF_RUNNING);
    }
    if loopback {
        f | IFF_LOOPBACK
    } else {
        f | IFF_BROADCAST | IFF_MULTICAST
    }
}

/// `RTM_GETADDR`'s answer: the loopback's addresses and every address eth0
/// carries, in the order each interface holds them. The label is the
/// interface's name - `eth0` for the first address, `eth0:1` for the second,
/// the alias convention `ip addr` prints.
///
/// Both halves are read from the live interfaces rather than from constants,
/// which is what §4.4 promises and what the loopback's promotion made
/// observable: `lo` is a real interface carrying a real `127.0.0.0/8`, so
/// printing a remembered `127.0.0.1` would be a dump that could disagree with
/// the stack it describes.
fn dump_addrs(seq: u32, pid: u32) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let Some(facts) = crate::net::link_facts() else {
        return vec![done(seq, pid)];
    };
    // The loopback's addresses are reported whatever its flag says: an address
    // outlives the link's up-ness (Linux reports both states), and callers
    // walking `getifaddrs` expect 127.0.0.1 to be there. One, as the machine
    // is configured, and the label needs no alias suffix because Linux does not
    // number `lo`'s addresses.
    for addr in crate::net::lo_addrs() {
        v.push(addr_msg(
            seq,
            pid,
            LO_INDEX,
            addr.prefix,
            addr.ip,
            "lo",
            254,
        ));
    }
    for (n, addr) in crate::net::ipv4_addrs().into_iter().enumerate() {
        let label = if n == 0 {
            facts.eth_name.clone()
        } else {
            format!("{}:{}", facts.eth_name, n)
        };
        v.push(addr_msg(
            seq,
            pid,
            ETH_INDEX,
            addr.prefix,
            addr.ip,
            &label,
            0,
        ));
    }
    v.push(done(seq, pid));
    v
}

/// `RTM_GETROUTE`'s answer: every prefix the routing table holds, each with
/// the gateway it leaves by and the preferred source address. The preferred
/// source is the interface's first address - what `fastfetch` reads as the
/// machine's local IP, and the field whose absence once made it print an
/// empty one.
fn dump_routes(seq: u32, pid: u32) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let prefsrc = crate::net::primary_ipv4();
    for route in crate::net::routes() {
        v.push(route_msg(
            seq,
            pid,
            ETH_INDEX,
            route.dst,
            route.dst_len,
            route.gateway,
            prefsrc,
        ));
    }
    v.push(done(seq, pid));
    v
}

/// `RTM_NEWLINK`: an `ifinfomsg` plus the interface's name, address and MTU.
#[allow(clippy::too_many_arguments)]
fn link_msg(
    seq: u32,
    pid: u32,
    index: u32,
    name: &str,
    flags: u32,
    link_type: u16,
    mac: Option<&[u8; 6]>,
    mtu: u32,
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
        // An Ethernet link broadcasts to the whole-wire address.
        payload.extend(rtattr(IFLA_BROADCAST, &[0xff; 6]));
    } else {
        // The loopback has no station address to name. Linux still carries
        // the broadcast attribute - all zeros, "nobody" - and `ip link`
        // prints it, so a message without one makes the printer read
        // garbage where the attribute should have been.
        payload.extend(rtattr(IFLA_ADDRESS, &[0x00; 6]));
        payload.extend(rtattr(IFLA_BROADCAST, &[0x00; 6]));
    }
    payload.extend(rtattr(IFLA_MTU, &mtu.to_le_bytes()));
    // A discipline name the link does not have: "noqueue" is what Linux
    // reports for a device with no qdisc, which is what this stack runs.
    payload.extend(rtattr(IFLA_QDISC, b"noqueue\0"));
    // The transmit queue length. Linux's own default for every NIC is 1000;
    // the attribute is a native-order word, like every netlink integer that
    // is not an address.
    payload.extend(rtattr(IFLA_TXQLEN, &1000u32.to_le_bytes()));
    nlmsg(RTM_NEWLINK, seq, pid, &payload, true)
}

/// `RTM_NEWROUTE`: a `rtmsg` plus the four attributes a route needs to be
/// useful to a caller. `dst_len` says what the destination prefix is, so
/// `RTA_DST` carries the network address (0.0.0.0 for the default route);
/// `RTA_GATEWAY` is the next hop; `RTA_OIF` is the outgoing interface index,
/// which is how a caller ties the route to a named device; and `RTA_PREFSRC`
/// is the kernel's preferred source address for the route - the value
/// `fastfetch` and friends read as the machine's local IP. Omit any of them
/// and a strict caller drops the whole reply rather than guess.
fn route_msg(
    seq: u32,
    pid: u32,
    oif: u32,
    dst: [u8; 4],
    dst_len: u8,
    gateway: [u8; 4],
    prefsrc: Option<[u8; 4]>,
) -> Vec<u8> {
    // `struct rtmsg`, field by field. Every entry carries its own comment so
    // the formatter keeps one field per line: without it, the uncommented
    // `dst_len` is joined to the `rtm_src_len` beside it and the list stops
    // reading as a struct.
    let mut payload = vec![
        2,       // rtm_family: AF_INET
        dst_len, // rtm_dst_len
        0,       // rtm_src_len
        0,       // rtm_tos
        254,     // rtm_table: RT_TABLE_MAIN
        4,       // rtm_protocol: RTPROT_STATIC
        0,       // rtm_scope: RT_SCOPE_UNIVERSE
        1,       // rtm_type: RTN_UNICAST
    ];
    // rtm_flags closes `struct rtmsg` at 12 bytes. Skipping it shifts every
    // attribute four bytes early: a strict caller reads the four attribute
    // bytes that should have been flags as `rta_len`, sees the zero payload of
    // the next attribute, and reports `deficit N, rta_len=0!`.
    payload.extend(0u32.to_le_bytes());
    payload.extend(rtattr(RTA_DST, &dst));
    payload.extend(rtattr(RTA_GATEWAY, &gateway));
    payload.extend(rtattr(RTA_OIF, &oif.to_le_bytes()));
    if let Some(prefsrc) = prefsrc {
        payload.extend(rtattr(RTA_PREFSRC, &prefsrc));
    }
    nlmsg(RTM_NEWROUTE, seq, pid, &payload, true)
}

/// `RTM_NEWADDR`: an `ifaddrmsg` plus the address, stated twice the way
/// Linux does it - `IFA_ADDRESS` for the prefix, `IFA_LOCAL` for the local
/// end - and the address's label.
fn addr_msg(
    seq: u32,
    pid: u32,
    index: u32,
    prefix: u8,
    ip: [u8; 4],
    name: &str,
    scope: u8,
) -> Vec<u8> {
    // `struct ifaddrmsg`, field by field.
    let mut payload = vec![
        2,               // ifa_family: AF_INET
        prefix,          // ifa_prefixlen
        IFA_F_PERMANENT, // ifa_flags
        scope,           // ifa_scope
    ];
    payload.extend(index.to_le_bytes());
    payload.extend(rtattr(IFA_ADDRESS, &ip));
    payload.extend(rtattr(IFA_LOCAL, &ip));
    payload.extend(rtattr(IFA_LABEL, &name_with_nul(name)));
    nlmsg(RTM_NEWADDR, seq, pid, &payload, true)
}
