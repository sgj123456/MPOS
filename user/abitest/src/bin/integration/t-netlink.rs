//! t-netlink - the route-netlink write side, applied and reported.
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
//! sides share. The buffers are fixed arrays - these programs have no
//! allocator, and no netlink message here comes close to a kilobyte.

#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const AF_NETLINK: usize = 16;
const SOCK_RAW: usize = 3;
const NETLINK_ROUTE: usize = 0;
const MSG_DONTWAIT: usize = 0x40;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 0x04;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_DUMP: u16 = 0x300;

const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETADDR: u16 = 22;
const RTM_GETROUTE: u16 = 26;

const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFA_ADDRESS: u16 = 1;
const RTA_DST: u16 = 1;
const RTA_GATEWAY: u16 = 5;

const IFF_UP: u32 = 0x1;
const ETH_INDEX: u32 = 2;

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
        while self.len % 4 != 0 {
            self.push(&[0]);
        }
    }
}

fn newaddr(_index: u32, prefix: u8, ip: [u8; 4]) -> Msg {
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
fn talk(fd: i32, req: &Msg, out: &mut [[u8; 512]; 16]) -> usize {
    if lin::sendto(fd, &req.buf[..req.len], 0, None) != req.len as isize {
        return 0;
    }
    let mut count = 0;
    loop {
        if count == out.len() {
            break;
        }
        let mut buf = [0u8; 512];
        let n = lin::recvfrom(fd, &mut buf, MSG_DONTWAIT, None);
        if n <= 0 {
            break;
        }
        let msg = &buf[..n as usize];
        let ty = u16::from_le_bytes([msg[4], msg[5]]);
        if ty == NLMSG_DONE {
            break;
        }
        out[count][..msg.len()].copy_from_slice(msg);
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
fn report_of(answers: &[[u8; 512]; 16], count: usize) -> Option<i32> {
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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-netlink\n");
    let mut failed = false;

    let fd = lin::socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) as i32;
    lin::check_true(&mut failed, b"the route socket opens", fd >= 0);
    if fd < 0 {
        lin::puts(b"FAILED\n");
        lin::exit(1);
    }

    let mut ans: [[u8; 512]; 16] = [[0u8; 512]; 16];

    // --- The dumps start from the live stack -----------------------------
    let n = talk(fd, &dump(RTM_GETADDR), &mut ans);
    lin::check_true(&mut failed, b"the address dump answers", n > 0);
    let found = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 8, IFA_ADDRESS, &[10, 0, 2, 15])
    });
    lin::check_true(
        &mut failed,
        b"the address dump carries eth0's static address",
        found,
    );

    // --- Address add / duplicate / delete ---------------------------------
    let n = talk(fd, &newaddr(ETH_INDEX, 24, [10, 0, 2, 99]), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip addr add 10.0.2.99/24 reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETADDR), &mut ans);
    let found = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 8, IFA_ADDRESS, &[10, 0, 2, 99])
    });
    lin::check_true(
        &mut failed,
        b"the dump shows the address that was added",
        found,
    );

    let n = talk(fd, &newaddr(ETH_INDEX, 24, [10, 0, 2, 99]), &mut ans);
    lin::check_eq(
        &mut failed,
        b"adding the same address again is EEXIST, as -17",
        report_of(&ans, n).unwrap_or(-1) as isize,
        -17,
    );

    let n = talk(fd, &deladdr([10, 0, 2, 99]), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip addr del reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETADDR), &mut ans);
    let still = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 8, IFA_ADDRESS, &[10, 0, 2, 99])
    });
    lin::check_true(
        &mut failed,
        b"the dump no longer shows the deleted address",
        !still,
    );

    let n = talk(fd, &deladdr([10, 0, 2, 99]), &mut ans);
    lin::check_eq(
        &mut failed,
        b"deleting a missing address is EADDRNOTAVAIL, as -99",
        report_of(&ans, n).unwrap_or(-1) as isize,
        -99,
    );

    // --- Route add / duplicate / delete ------------------------------------
    let n = talk(
        fd,
        &newroute([192, 168, 50, 0], 24, [10, 0, 2, 2]),
        &mut ans,
    );
    lin::check_eq(
        &mut failed,
        b"ip route add 192.168.50.0/24 reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETROUTE), &mut ans);
    let found = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 12, RTA_DST, &[192, 168, 50, 0])
            && has_attr(&ans[i], len, 12, RTA_GATEWAY, &[10, 0, 2, 2])
    });
    lin::check_true(
        &mut failed,
        b"the route dump shows the prefix that was added",
        found,
    );

    let n = talk(
        fd,
        &newroute([192, 168, 50, 0], 24, [10, 0, 2, 2]),
        &mut ans,
    );
    lin::check_eq(
        &mut failed,
        b"adding the same route again is EEXIST, as -17",
        report_of(&ans, n).unwrap_or(-1) as isize,
        -17,
    );

    let n = talk(fd, &delroute([192, 168, 50, 0], 24), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip route del reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETROUTE), &mut ans);
    let still = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 12, RTA_DST, &[192, 168, 50, 0])
    });
    lin::check_true(
        &mut failed,
        b"the route dump no longer shows the deleted prefix",
        !still,
    );

    let n = talk(fd, &delroute([192, 168, 50, 0], 24), &mut ans);
    lin::check_eq(
        &mut failed,
        b"deleting a missing route is ESRCH, as -3",
        report_of(&ans, n).unwrap_or(-1) as isize,
        -3,
    );

    // --- Link up/down, read back through the flags -------------------------
    let n = talk(fd, &dump(RTM_GETLINK), &mut ans);
    let flags = (0..n)
        .find_map(|i| {
            let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
            link_flags(&ans[i], len, b"eth0\0")
        })
        .unwrap_or(0);
    lin::check_true(
        &mut failed,
        b"eth0 reports IFF_UP before the test touches it",
        flags & IFF_UP != 0,
    );

    let n = talk(fd, &setlink(ETH_INDEX, 0, IFF_UP), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip link set eth0 down reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETLINK), &mut ans);
    let flags = (0..n)
        .find_map(|i| {
            let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
            link_flags(&ans[i], len, b"eth0\0")
        })
        .unwrap_or(0);
    lin::check_true(
        &mut failed,
        b"eth0 reports no IFF_UP after being set down",
        flags & IFF_UP == 0,
    );

    let n = talk(fd, &setlink(ETH_INDEX, IFF_UP, IFF_UP), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip link set eth0 up reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETLINK), &mut ans);
    let flags = (0..n)
        .find_map(|i| {
            let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
            link_flags(&ans[i], len, b"eth0\0")
        })
        .unwrap_or(0);
    lin::check_true(
        &mut failed,
        b"eth0 reports IFF_UP again after being set up",
        flags & IFF_UP != 0,
    );

    // --- MTU, set and read back ---------------------------------------------
    let n = talk(fd, &setmtu(1400), &mut ans);
    lin::check_eq(
        &mut failed,
        b"ip link set eth0 mtu 1400 reports success",
        report_of(&ans, n).unwrap_or(-1) as isize,
        0,
    );
    let n = talk(fd, &dump(RTM_GETLINK), &mut ans);
    let found = (0..n).any(|i| {
        let len = u32::from_le_bytes([ans[i][0], ans[i][1], ans[i][2], ans[i][3]]) as usize;
        has_attr(&ans[i], len, 16, IFLA_MTU, &1400u32.to_le_bytes())
    });
    lin::check_true(&mut failed, b"the link dump reports the new MTU", found);

    let _ = lin::close(fd);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-netlink: panicked\n")
}
