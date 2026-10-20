//! t-netlink - the route-netlink write side, applied and reported, on both machines.
//!
//! `ip addr add`, `ip route add`, `ip link set` are not file operations; they
//! are netlink requests whose answers a caller like busybox `ip` waits for
//! and whose effect a later dump has to show. This pins the whole loop the
//! kernel owes the `ip` command: a dump read from the *live* stack, a
//! mutation applied to it, the report that says what happened (zero on
//! success, the errno on refusal), and the dump again, changed.
//!
//! The wire protocol is built by hand here rather than through a library,
//! which is the point: the message layouts (`nlmsghdr`, `ifaddrmsg`, `rtmsg`,
//! `ifinfomsg`, `rtattr`) are exactly what the kernel has to parse, and a
//! test that spells them out is a test of the parser, not of a codec both
//! sides share. The buffers are fixed arrays because no netlink message here
//! comes close to a kilobyte. Neither library carries the vocabulary on this
//! target (measured: `libc` 0.2.190 binds neither `struct nlmsghdr` nor any
//! `NLMSG_*`/`NLM_F_*` constant for linux - those exist in its android and
//! uapi-mirror trees only - so the words are spelled here as `t-rtc` spells
//! its ioctl family); `rustix` speaks netlink *addresses* (`bind`,
//! `SocketAddrNetlink`) but owns no part of the record shape, which is the
//! subject.
//!
//! Both machines serve all of it: every address claim states the stack's own
//! configuration (`src/net/mod.rs`: eth0 comes up static at 10.0.2.15/24 with
//! gateway 10.0.2.2, the same on either bus - the boot line prints it on both
//! logs), and the mutations are refused or accepted by the one `rtnl` module.
//! Two claims are new pins of Linux's answers rather than migrations: the
//! link dump reports **both** interfaces Linux's enumeration names - `lo` at
//! index 1 with `IFF_LOOPBACK|IFF_UP|IFF_RUNNING`, and `eth0` at 2 - and an
//! administratively up interface also answers `IFF_RUNNING`, which is the bit
//! `ip link`'s `<UP,RUNNING>` prints and a caller that trusts `IFF_UP` alone
//! would misread on a card with no carrier.
//!
//! # Not run on the host
//!
//! A host Linux answers every request here - including the three refusals,
//! EEXIST/EADDRNOTAVAIL/ESRCH, which are Linux's own - but it answers them
//! about *its own* stack: `10.0.2.15` is this kernel's static address, not
//! something a host's `lo`/`eth0` carries, and setting the host's real NIC
//! down would take the sandbox's network with it. So the hosted run says so
//! and claims nothing, finishing green with its reason printed - batch 34's
//! rule for a program whose door only the payload opens. The pin is the two
//! sweeps.

use mpos_user_tests::{Report, host_run};
use rustix::fd::OwnedFd;
use rustix::io::{Errno, IoSliceMut};
use rustix::net::netlink::SocketAddrNetlink;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvFlags, ReturnFlags, SendFlags, SocketType,
};

// The wire words, spelled as `linux/netlink.h` and `linux/rtnetlink.h` spell
// them. Identical on both machines - these are not syscall numbers.
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 0x04;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_DUMP: u16 = 0x300; // NLM_F_ROOT | NLM_F_MATCH

const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;

const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFA_ADDRESS: u16 = 1;
const RTA_DST: u16 = 1;
const RTA_GATEWAY: u16 = 5;

// The link flags, from `net/if.h`.
const IFF_UP: u32 = 0x1;
const IFF_RUNNING: u32 = 0x40;
const IFF_LOOPBACK: u32 = 0x8;

/// The stack's own configuration, stated rather than discovered: the address
/// `src/net/mod.rs` brings eth0 up with on both machines, and the index the
/// one-NIC enumeration gives it (Linux's pair: `lo` 1, first NIC 2).
const ETH_ADDRESS: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
const ETH_INDEX: u32 = 2;

/// The address the add/delete dance makes and unmakes, and the route prefix
/// likewise - names this test owns and leaves nothing behind.
const EXTRA_ADDRESS: [u8; 4] = [10, 0, 2, 99];
const EXTRA_ROUTE: [u8; 4] = [192, 168, 50, 0];

/// A request under construction: a header and up to 128 bytes of payload,
/// which no message here comes close to filling.
struct Msg {
    buf: [u8; 160],
    len: usize,
}

impl Msg {
    fn new(ty: u16, flags: u16, payload: &[u8]) -> Msg {
        let mut m = Msg {
            buf: [0u8; 160],
            len: 16 + payload.len(),
        };
        m.buf[..4].copy_from_slice(&(m.len as u32).to_le_bytes());
        m.buf[4..6].copy_from_slice(&ty.to_le_bytes());
        m.buf[6..8].copy_from_slice(&flags.to_le_bytes());
        m.buf[8..12].copy_from_slice(&0u32.to_le_bytes()); // seq
        m.buf[12..16].copy_from_slice(&0u32.to_le_bytes()); // pid
        m.buf[16..m.len].copy_from_slice(payload);
        m
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        self.buf[..4].copy_from_slice(&(self.len as u32).to_le_bytes());
    }

    /// One route attribute: a 4-byte head, the value, padded to four.
    fn attr(&mut self, ty: u16, value: &[u8]) {
        let mut head = [0u8; 4];
        head[..2].copy_from_slice(&((4 + value.len()) as u16).to_le_bytes());
        head[2..].copy_from_slice(&ty.to_le_bytes());
        self.push(&head);
        self.push(value);
        while !self.len.is_multiple_of(4) {
            self.push(&[0]);
        }
    }
}

fn newaddr(prefix: u8, ip: [u8; 4]) -> Msg {
    // `struct ifaddrmsg`: family, prefix, flags, scope, index - the one
    // interface an address request names here is eth0, index 2.
    let head = [2u8, prefix, 0, 0, 2, 0, 0, 0];
    let mut m = Msg::new(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &head,
    );
    m.attr(IFA_ADDRESS, &ip);
    m
}

fn deladdr(ip: [u8; 4]) -> Msg {
    let head = [2u8, 0, 0, 0, 2, 0, 0, 0];
    let mut m = Msg::new(RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, &head);
    m.attr(IFA_ADDRESS, &ip);
    m
}

fn newroute(dst: [u8; 4], dst_len: u8, gateway: [u8; 4]) -> Msg {
    let mut head = [0u8; 12];
    head[0] = 2; // family
    head[1] = dst_len;
    head[4] = 254; // table: RT_TABLE_MAIN
    head[5] = 4; // protocol: RTPROT_STATIC
    head[7] = 1; // type: RTN_UNICAST
    let mut m = Msg::new(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &head,
    );
    m.attr(RTA_DST, &dst);
    m.attr(RTA_GATEWAY, &gateway);
    m
}

fn delroute(dst: [u8; 4], dst_len: u8) -> Msg {
    let mut head = [0u8; 12];
    head[0] = 2;
    head[1] = dst_len;
    head[7] = 1;
    let mut m = Msg::new(RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, &head);
    m.attr(RTA_DST, &dst);
    m
}

fn setlink(index: u32, flags: u32, change: u32) -> Msg {
    let mut head = [0u8; 16];
    head[2..4].copy_from_slice(&1u16.to_le_bytes()); // type: ethernet
    head[4..8].copy_from_slice(&index.to_le_bytes());
    head[8..12].copy_from_slice(&flags.to_le_bytes());
    head[12..16].copy_from_slice(&change.to_le_bytes());
    Msg::new(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, &head)
}

fn setmtu(mtu: u32) -> Msg {
    let mut head = [0u8; 16];
    head[2..4].copy_from_slice(&1u16.to_le_bytes());
    head[4..8].copy_from_slice(&ETH_INDEX.to_le_bytes());
    let mut m = Msg::new(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, &head);
    m.attr(IFLA_MTU, &mtu.to_le_bytes());
    m
}

fn dump(ty: u16) -> Msg {
    Msg::new(ty, NLM_F_REQUEST | NLM_F_DUMP, &[])
}

/// One round trip: send a request, read every answer it queued. The socket
/// is datagram-shaped on this kernel - one answer per read - and the run
/// ends where the protocol says: a dump at its `NLMSG_DONE`, a mutation at
/// its one `NLMSG_ERROR` report. Reading with `MSG_DONTWAIT` keeps an
/// answer-less request (a success nobody asked an ACK for) from parking.
fn talk<F: rustix::fd::AsFd + Copy>(fd: F, req: &Msg, out: &mut Vec<[u8; 512]>) -> usize {
    if rustix::net::send(fd, &req.buf[..req.len], rustix::net::SendFlags::empty()).ok()
        != Some(req.len)
    {
        return 0;
    }
    let mut count = 0;
    loop {
        if count == 16 {
            break;
        }
        let mut buf = [0u8; 512];
        let n = match rustix::net::recv(fd, &mut buf[..], rustix::net::RecvFlags::DONTWAIT) {
            Ok((n, _)) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let msg = &buf[..n];
        let ty = u16::from_le_bytes([msg[4], msg[5]]);
        if ty == NLMSG_DONE {
            break;
        }
        let mut slot = [0u8; 512];
        slot[..msg.len()].copy_from_slice(msg);
        out.push(slot);
        count += 1;
        if ty == NLMSG_ERROR {
            break;
        }
    }
    count
}

/// The report a mutation waits for: an `NLMSG_ERROR` whose `error` word is
/// the errno - zero *is* the success report. Answers `None` when the first
/// answer was any other shape, which is what a caller that insists on the
/// report would loop on.
fn report_of(answers: &[[u8; 512]], count: usize) -> Option<i32> {
    if count == 0 {
        return None;
    }
    let first = &answers[0];
    if u16::from_le_bytes([first[4], first[5]]) != NLMSG_ERROR {
        return None;
    }
    Some(i32::from_le_bytes([
        first[16], first[17], first[18], first[19],
    ]))
}

/// Whether any answer carries an attribute of type `ty` whose value is
/// exactly `value`. `struct_len` is where the attributes start in the
/// payload; `msg_len` is the answer's own length word.
fn has_attr(msg: &[u8; 512], msg_len: usize, struct_len: usize, ty: u16, value: &[u8]) -> bool {
    if msg_len < 16 + struct_len {
        return false;
    }
    let payload = &msg[16..msg_len];
    let mut at = struct_len;
    while at + 4 <= payload.len() {
        let rta_len = u16::from_le_bytes([payload[at], payload[at + 1]]) as usize;
        if rta_len < 4 {
            return false;
        }
        let attr_ty = u16::from_le_bytes([payload[at + 2], payload[at + 3]]);
        let attr = &payload[at + 4..at + rta_len];
        if attr_ty == ty && attr == value {
            return true;
        }
        at += (rta_len + 3) & !3;
    }
    false
}

/// The `ifi_flags` word of a link answer (at payload offset 8) whose
/// `IFLA_IFNAME` is `name`.
fn link_flags(msg: &[u8; 512], msg_len: usize, name: &[u8]) -> Option<u32> {
    if !has_attr(msg, msg_len, 16, IFLA_IFNAME, name) {
        return None;
    }
    Some(u32::from_le_bytes([msg[24], msg[25], msg[26], msg[27]]))
}

/// Any answer's flags whose name is `name`, from a fresh GETLINK dump.
fn dumped_link_flags<F: rustix::fd::AsFd + Copy>(fd: F, name: &[u8]) -> Option<u32> {
    let mut ans = Vec::new();
    let n = talk(fd, &dump(RTM_GETLINK), &mut ans);
    (0..n).find_map(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        link_flags(&ans[i], len, name)
    })
}

/// Any answer of a fresh dump of `ty` carrying the named attribute.
fn dump_shows<F: rustix::fd::AsFd + Copy>(
    fd: F,
    ty: u16,
    struct_len: usize,
    attr_ty: u16,
    value: &[u8],
) -> bool {
    let mut ans = Vec::new();
    let n = talk(fd, &dump(ty), &mut ans);
    (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, struct_len, attr_ty, value)
    })
}

/// The report of one mutation, as an isize for the claim that compares it.
fn mutation_report<F: rustix::fd::AsFd + Copy>(fd: F, req: &Msg) -> isize {
    let mut ans = Vec::new();
    let n = talk(fd, req, &mut ans);
    report_of(&ans, n).unwrap_or(-1) as isize
}

/// A fresh route socket with one `RTM_GETLINK` dump's answers queued - the
/// fixture every receive-door claim below starts from, so each one sees the
/// same two answers in the same order.
fn socket_with_dump() -> Option<OwnedFd> {
    let fd = rustix::net::socket(AddressFamily::NETLINK, SocketType::RAW, None).ok()?;
    rustix::net::bind(&fd, &SocketAddrNetlink::new(0, 0)).ok()?;
    let req = dump(RTM_GETLINK);
    rustix::net::send(&fd, &req.buf[..req.len], SendFlags::empty()).ok()?;
    Some(fd)
}

/// One `recvmsg` through a single iovec: the syscall's answer, the
/// `msg_flags` word it left, and the refusal when there was one. The message
/// word is what a short receive's claim reads - `MSG_TRUNC` there says a copy
/// was cut, which no return value can.
fn door(fd: &OwnedFd, buf: &mut [u8], flags: RecvFlags) -> (isize, ReturnFlags, Option<Errno>) {
    let mut iov = [IoSliceMut::new(buf)];
    let mut ctl = RecvAncillaryBuffer::default();
    match rustix::net::recvmsg(fd, &mut iov, &mut ctl, flags) {
        Ok(msg) => (msg.bytes as isize, msg.flags, None),
        Err(e) => (-1, ReturnFlags::empty(), Some(e)),
    }
}

/// The `ifi_index` a link answer names, at `nlmsghdr` (16) + `ifinfomsg`'s
/// index (4) - how a claim tells the dump's two answers apart.
fn answer_index(prefix: &[u8]) -> u32 {
    u32::from_le_bytes([prefix[20], prefix[21], prefix[22], prefix[23]])
}

fn main() {
    println!("t-netlink");
    let mut r = Report::new();

    // The hosted run measures the host's own stack and could take its NIC
    // down with it; see the module comment. It says so and claims nothing.
    if host_run() {
        println!(
            "note: the route socket's answers belong to the machine's own stack; this program is pinned by the two sweeps, not a host run"
        );
        r.finish();
    }

    let fd = match rustix::net::socket(AddressFamily::NETLINK, SocketType::RAW, None) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("the route socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("the route socket opens", true);
    // An unbound netlink socket cannot receive: the kernel needs the port id
    // the bind assigns to route the answers back. `rustix` carries the bind
    // and the address type; the auto-bind on first send is a glibc convenience
    // the raw door does not have, so the migrated program binds explicitly -
    // the shape busybox `ip` builds itself. A port id of 0 asks the kernel to
    // assign one, and binding twice (a retry) is EADDRINUSE, which this run
    // never reaches.
    let bound = rustix::net::bind(&fd, &rustix::net::netlink::SocketAddrNetlink::new(0, 0));
    r.check("and binds its port id", bound.is_ok());
    if bound.is_err() {
        r.finish();
    }

    // --- The dumps start from the live stack -----------------------------
    r.check("the address dump answers", {
        let mut ans = Vec::new();
        talk(&fd, &dump(RTM_GETADDR), &mut ans) > 0
    });
    r.check(
        "the address dump carries eth0's static address",
        dump_shows(&fd, RTM_GETADDR, 8, IFA_ADDRESS, &ETH_ADDRESS),
    );

    // --- Address add / duplicate / delete ---------------------------------
    r.check(
        "ip addr add 10.0.2.99/24 reports success",
        mutation_report(&fd, &newaddr(24, EXTRA_ADDRESS)) == 0,
    );
    r.check(
        "the dump shows the address that was added",
        dump_shows(&fd, RTM_GETADDR, 8, IFA_ADDRESS, &EXTRA_ADDRESS),
    );
    r.check(
        "adding the same address again is EEXIST, as -17",
        mutation_report(&fd, &newaddr(24, EXTRA_ADDRESS)) == -17,
    );
    r.check(
        "ip addr del reports success",
        mutation_report(&fd, &deladdr(EXTRA_ADDRESS)) == 0,
    );
    r.check(
        "the dump no longer shows the deleted address",
        !dump_shows(&fd, RTM_GETADDR, 8, IFA_ADDRESS, &EXTRA_ADDRESS),
    );
    r.check(
        "deleting a missing address is EADDRNOTAVAIL, as -99",
        mutation_report(&fd, &deladdr(EXTRA_ADDRESS)) == -99,
    );

    // --- Route add / duplicate / delete ------------------------------------
    r.check(
        "ip route add 192.168.50.0/24 reports success",
        mutation_report(&fd, &newroute(EXTRA_ROUTE, 24, GATEWAY)) == 0,
    );
    r.check("the route dump shows the prefix that was added", {
        let mut ans = Vec::new();
        let n = talk(&fd, &dump(RTM_GETROUTE), &mut ans);
        (0..n).any(|i| {
            let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
            has_attr(&ans[i], len, 12, RTA_DST, &EXTRA_ROUTE)
                && has_attr(&ans[i], len, 12, RTA_GATEWAY, &GATEWAY)
        })
    });
    r.check(
        "adding the same route again is EEXIST, as -17",
        mutation_report(&fd, &newroute(EXTRA_ROUTE, 24, GATEWAY)) == -17,
    );
    r.check(
        "ip route del reports success",
        mutation_report(&fd, &delroute(EXTRA_ROUTE, 24)) == 0,
    );
    r.check(
        "the route dump no longer shows the deleted prefix",
        !dump_shows(&fd, RTM_GETROUTE, 12, RTA_DST, &EXTRA_ROUTE),
    );
    r.check(
        "deleting a missing route is ESRCH, as -3",
        mutation_report(&fd, &delroute(EXTRA_ROUTE, 24)) == -3,
    );

    // --- Link up/down, read back through the flags -------------------------
    let flags = dumped_link_flags(&fd, b"eth0\0").unwrap_or(0);
    r.check(
        "eth0 reports IFF_UP before the test touches it",
        flags & IFF_UP != 0,
    );
    // New pins, stated as Linux states them: the administrative UP bit comes
    // with the carrier's RUNNING bit, and the dump names both interfaces the
    // enumeration has - lo at 1, loopback-flagged.
    r.check(
        "and an up eth0 also reports IFF_RUNNING",
        flags & IFF_RUNNING != 0,
    );
    let lo = dumped_link_flags(&fd, b"lo\0").unwrap_or(0);
    r.check(
        "the link dump names lo too, up and loopback",
        lo & IFF_UP != 0 && lo & IFF_LOOPBACK != 0,
    );

    r.check(
        "ip link set eth0 down reports success",
        mutation_report(&fd, &setlink(ETH_INDEX, 0, IFF_UP)) == 0,
    );
    let flags = dumped_link_flags(&fd, b"eth0\0").unwrap_or(0xffff_ffff);
    r.check(
        "eth0 reports no IFF_UP after being set down",
        flags & IFF_UP == 0,
    );

    r.check(
        "ip link set eth0 up reports success",
        mutation_report(&fd, &setlink(ETH_INDEX, IFF_UP, IFF_UP)) == 0,
    );
    let flags = dumped_link_flags(&fd, b"eth0\0").unwrap_or(0);
    r.check(
        "eth0 reports IFF_UP again after being set up",
        flags & IFF_UP != 0,
    );

    // --- MTU, set and read back ---------------------------------------------
    r.check(
        "ip link set eth0 mtu 1400 reports success",
        mutation_report(&fd, &setmtu(1400)) == 0,
    );
    r.check(
        "the link dump reports the new MTU",
        dump_shows(&fd, RTM_GETLINK, 16, IFLA_MTU, &1400u32.to_le_bytes()),
    );
    // ...and put it back, because the MTU is the *machine's*, not this test's:
    // the sweep runs program after program in one boot, and the next network
    // program sends datagrams sized for the MTU it expects. Leaving 1400 here is
    // what broke `t-netstats` — measured: its 1400-byte payload plus 42 bytes of
    // headers cannot be dispatched under a 1400-byte MTU, so smoltcp accepted the
    // datagrams into the socket's buffer, never put one on the wire, and the
    // buffer filled after four (2000 `EAGAIN` retries, four frames at the seam,
    // one of them the ARP request). The rule the repo states for a check that
    // touches shared state — put it back — is what this line is.
    r.check(
        "ip link set eth0 mtu 1500 puts it back",
        mutation_report(&fd, &setmtu(1500)) == 0,
    );

    drop(fd);

    // ---------------------------------------------------------------------
    // The receive doors look without taking, and `MSG_TRUNC` names the
    // answer's own length.
    //
    // Both doors used to ignore the flag word entirely: `recvfrom` popped the
    // front answer whatever `MSG_PEEK` said and returned `min(len, message)`
    // whatever `MSG_TRUNC` said, and `recvmsg` did the same behind its iovec
    // chain. Measured whole on a current host first
    // (`.workbuddy/probe_netlink_peek.c`, Linux 7.2.11, over a `RTM_GETLINK`
    // dump, where "was the front taken?" is answered by *which* interface the
    // next answer names rather than by an empty-queue `EAGAIN` alone):
    //
    //   * a peek answers the message's length and leaves it; the plain
    //     receive after it returns the same message, and a peek is repeatable;
    //   * a short receive - 8 bytes of a 3020-byte answer - answers 8 and
    //     *consumes* the message, and the next answer is a different one;
    //   * `MSG_TRUNC` makes the return the message's own length (3020 through
    //     an 8-byte room) while the copy stays 8, consumes it if it was not a
    //     peek, and `msg_flags` carries `MSG_TRUNC` whenever a copy was cut -
    //     whether or not the caller asked for the flag;
    //   * a zero-length room answers 0; without the peek it consumes the
    //     message and with it the message stays, and `msg_flags` reports the
    //     cut either way;
    //   * `MSG_DONTWAIT` on a drained queue is `EAGAIN`, peek or not.
    //
    // The two answers of a link dump are told apart by `ifi_index` - `lo` at
    // 1, `eth0` at 2 - so every "consumed?" claim below is a claim about which
    // message the front is, not merely that the queue emptied.
    // ---------------------------------------------------------------------
    let fixture = socket_with_dump();
    r.check("a fresh route socket queues a link dump", fixture.is_some());
    if let Some(fd) = fixture {
        let mut front = [0u8; 64];
        let (full, flags, err) = door(&fd, &mut front, RecvFlags::PEEK | RecvFlags::TRUNC);
        let word = u32::from_le_bytes([front[0], front[1], front[2], front[3]]) as isize;
        r.check(
            "a peek with MSG_TRUNC answers the answer's own length through 64 bytes",
            err.is_none() && full == word && full > 64,
        );
        r.check(
            "and says the copy was cut in msg_flags",
            flags.contains(ReturnFlags::TRUNC),
        );
    }
    if let Some(fd) = socket_with_dump() {
        let mut front = [0u8; 64];
        let mut other = [0u8; 64];
        let (full, _, _) = door(&fd, &mut front, RecvFlags::PEEK | RecvFlags::TRUNC);
        let (again, _, _) = door(&fd, &mut other, RecvFlags::PEEK | RecvFlags::TRUNC);
        r.check(
            "the look is repeatable and takes nothing",
            again == full && other == front,
        );
        let mut take = [0u8; 64];
        let (n, _, _) = door(&fd, &mut take, RecvFlags::empty());
        r.check(
            "the plain receive after the looks answers the same bytes",
            n == 64 && take == front,
        );
        let (_, _, _) = door(&fd, &mut other, RecvFlags::PEEK | RecvFlags::TRUNC);
        r.check(
            "and the message it took is gone: the front names another interface",
            answer_index(&other) != answer_index(&front),
        );
    } else {
        r.check("the look is repeatable and takes nothing", false);
        r.check(
            "the plain receive after the looks answers the same bytes",
            false,
        );
        r.check(
            "and the message it took is gone: the front names another interface",
            false,
        );
    }

    if let Some(fd) = socket_with_dump() {
        let mut front = [0u8; 64];
        let mut other = [0u8; 64];
        let mut small = [0u8; 8];
        let (_, _, _) = door(&fd, &mut front, RecvFlags::PEEK | RecvFlags::TRUNC);
        let (n, flags, _) = door(&fd, &mut small, RecvFlags::empty());
        let (_, _, _) = door(&fd, &mut other, RecvFlags::PEEK | RecvFlags::TRUNC);
        r.check(
            "a short receive answers the room and consumes the answer",
            n == 8 && answer_index(&other) != answer_index(&front),
        );
        r.check(
            "and reports the cut in msg_flags without being asked",
            flags.contains(ReturnFlags::TRUNC),
        );
        let (n, _, _) = door(&fd, &mut small, RecvFlags::TRUNC);
        r.check(
            "MSG_TRUNC answers the next answer's own length through the same 8 bytes",
            n > 8,
        );
    } else {
        r.check(
            "a short receive answers the room and consumes the answer",
            false,
        );
        r.check(
            "and reports the cut in msg_flags without being asked",
            false,
        );
        r.check(
            "MSG_TRUNC answers the next answer's own length through the same 8 bytes",
            false,
        );
    }

    // The zero-length room: a receive that takes the message and answers 0, a
    // look that leaves it, and the message word's report in both.
    if let Some(fd) = socket_with_dump() {
        let mut front = [0u8; 64];
        let mut other = [0u8; 64];
        let (_, _, _) = door(&fd, &mut front, RecvFlags::PEEK | RecvFlags::TRUNC);
        let (n, flags, _) = door(&fd, &mut [], RecvFlags::PEEK);
        let (_, _, _) = door(&fd, &mut other, RecvFlags::PEEK | RecvFlags::TRUNC);
        r.check(
            "a zero-length peek answers 0 and leaves the answer",
            n == 0 && answer_index(&other) == answer_index(&front),
        );
        r.check(
            "with the cut reported in msg_flags",
            flags.contains(ReturnFlags::TRUNC),
        );
        let (n, flags, _) = door(&fd, &mut [], RecvFlags::empty());
        let (_, _, _) = door(&fd, &mut other, RecvFlags::PEEK | RecvFlags::TRUNC);
        r.check(
            "a zero-length receive answers 0 and consumes it",
            n == 0 && answer_index(&other) != answer_index(&front),
        );
        r.check(
            "and reports the cut too",
            flags.contains(ReturnFlags::TRUNC),
        );
    } else {
        r.check("a zero-length peek answers 0 and leaves the answer", false);
        r.check("with the cut reported in msg_flags", false);
        r.check("a zero-length receive answers 0 and consumes it", false);
        r.check("and reports the cut too", false);
    }

    // `MSG_DONTWAIT` on a drained queue: the wait is the caller's to skip.
    if let Some(fd) = socket_with_dump() {
        let mut buf = [0u8; 512];
        while rustix::net::recv(&fd, &mut buf[..], RecvFlags::DONTWAIT).is_ok() {}
        let plain = rustix::net::recv(&fd, &mut buf[..], RecvFlags::DONTWAIT);
        let look = rustix::net::recv(&fd, &mut buf[..], RecvFlags::DONTWAIT | RecvFlags::PEEK);
        r.check(
            "MSG_DONTWAIT on a drained queue is EAGAIN, peek or not",
            plain.err() == Some(Errno::AGAIN) && look.err() == Some(Errno::AGAIN),
        );
    } else {
        r.check(
            "MSG_DONTWAIT on a drained queue is EAGAIN, peek or not",
            false,
        );
    }

    r.finish()
}
