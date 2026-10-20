//! t-dhcp - the link layer as a socket: an `AF_PACKET` client gets a DHCP
//! answer with the machine still having no address of its own.
//!
//! This is the *mechanism* half of the plan recorded in `NOTES.md`: the kernel
//! stopped being the one that decides the machine's address, and started
//! offering the door through which a userland client can ask for one. The test
//! plays that client by hand - a DHCPDISCOVER built octet by octet, sent to the
//! broadcast through `sendto(2)` with a `sockaddr_ll`, and the answer read back
//! off the same socket - because the day's promise is only that the door exists
//! and answers truthfully, not that a shipping `udhcpc` is wired into the boot
//! (that gap stays named where it is).
//!
//! The QEMU user-mode network carries a BOOTP server by design - it is what hands
//! out the 10.0.2.15 this machine's static boot default used to restate - so the
//! answer here comes from the host's slirp, and its shape is the host's promise:
//! a BOOTREPLY carrying our transaction id.
//!
//! Two fixtures move with the program, and both are the same rule - a test that
//! is *given* its fixture cannot go stale on the machine that serves it. The
//! no-std original wrote `ifindex = 2` as a literal, which is a number read off
//! the first machine's one interface; this version asks `SIOCGIFINDEX` what index
//! `eth0` has and binds to the answer. The MAC the DISCOVER's `chaddr` and
//! client-id carry is likewise read out of `getsockname` rather than assumed -
//! which is also the only way the `chaddr` can be a claim, because a BOOTP
//! server answers the hardware address it was asked about.
//!
//! The record the two addresses are carried in is spelled as
//! `linux/if_packet.h` lays it out, with the offsets as compile-time asserts
//! (`t-drm-args`'s rule): `libc` declares `sockaddr_ll` but not in a shape this
//! program can fill per-field on both machines, and a wrong `sll_halen` is a
//! frame the kernel never sends.
//!
//! Not made on a host, and for two reasons rather than one. `socket(AF_PACKET)`
//! needs `CAP_NET_RAW`, which this sandbox does not have (measured: `EPERM` for
//! an unprivileged `socket(AF_PACKET, SOCK_RAW)`), so a hosted run would stop at
//! the first line with an answer about the sandbox and not about any kernel. And
//! the one part that would succeed with the capability - sending a DISCOVER to
//! the link-layer broadcast - would put a real DHCP request on the host's own
//! LAN, which is not something a conformance test gets to do to the machine it
//! is running on. So a host run names that and claims nothing, `t-pty`'s rule: a
//! run that cannot build the fixture makes no claims rather than making the
//! host's ones.

use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use mpos_user_tests::{Report, host_run};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::net::{AddressFamily, SocketType, socket};
use rustix::time::Timespec;

/// `htons(ETH_P_IP)`: the caller's protocol number travels as the pattern the
/// wire's two type bytes spell, and the kernel compares patterns - which is what
/// `packet(7)` says it expects, on Linux too.
const ETH_P_IP: i32 = 0x0008;
/// `ARPHRD_ETHER`, the only hardware type this link has.
const ARPHRD_ETHER: u16 = 1;
/// `ETH_HLEN`, from `linux/if_ether.h`: two MACs and the type. A `SOCK_RAW`
/// send shorter than this is `EINVAL` (measured), and the offset a `SOCK_RAW`
/// read hands the payload at.
const ETH_HEADER_LEN: usize = 14;

/// `SIOCGIFINDEX` - `if_nametoindex(3)` is this request, and it is what says
/// which interface the DISCOVER goes out on.
const SIOCGIFINDEX: u64 = 0x8933;

/// The one's-complement sum every IPv4 header on this machine's wire needs.
fn cksum(buf: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < buf.len() {
        sum += u32::from(u16::from_be_bytes([buf[i], buf[i + 1]]));
        i += 2;
    }
    if i < buf.len() {
        sum += u32::from(buf[i]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// `struct sockaddr_ll`, per `linux/if_packet.h`. Twenty bytes, and nothing in
/// it is the same width twice - which is why the offsets are asserted rather
/// than counted.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockaddrLl {
    family: u16,
    protocol: u16,
    ifindex: i32,
    hatype: u16,
    pkttype: u8,
    halen: u8,
    addr: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<SockaddrLl>() == 20);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, family) == 0);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, protocol) == 2);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, ifindex) == 4);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, hatype) == 8);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, halen) == 11);
const _: () = assert!(core::mem::offset_of!(SockaddrLl, addr) == 12);

/// `struct ifreq` for the one request this program makes: a 16-byte name and a
/// union whose `ifr_ifindex` starts at 16.
#[repr(C)]
struct Ifreq {
    name: [u8; 16],
    data: [u8; 24],
}

impl Ifreq {
    /// A request naming `name` (at most 15 bytes, NUL-terminated by the zeroed
    /// field).
    fn named(name: &str) -> Self {
        let mut this = Ifreq {
            name: [0; 16],
            data: [0; 24],
        };
        let bytes = name.as_bytes();
        this.name[..bytes.len().min(15)].copy_from_slice(&bytes[..bytes.len().min(15)]);
        this
    }

    /// The union read as `ifr_ifindex`: the same byte offset, read as an `int`.
    fn ifindex(&self) -> i32 {
        i32::from_ne_bytes(self.data[0..4].try_into().expect("four bytes"))
    }
}

/// A raw call's answer with libc's `-1`/`errno` turned back into the negative
/// errno a bare syscall hands back, which is the shape every claim below is
/// written against. Called in the same expression as the call, because `errno`
/// is only valid until the next one.
fn as_raw(rc: isize) -> isize {
    if rc < 0 {
        -(std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as isize)
    } else {
        rc
    }
}

/// One claim that a call succeeded.
fn check_ok(r: &mut Report, name: &str, got: isize) {
    if got >= 0 {
        r.check(name, true);
    } else {
        r.check(&format!("{name} (got {got})"), false);
    }
}

fn main() {
    println!("t-dhcp");
    let mut r = Report::new();

    // A hosted run has neither the capability nor the right to put a DISCOVER
    // on the host's own LAN: see the header.
    if host_run() {
        println!(
            "note: not made on a host - socket(AF_PACKET) needs CAP_NET_RAW (this sandbox \
             answers EPERM), and sending a DISCOVER would put a real DHCP request on the \
             host's own link"
        );
        r.finish();
    }

    // --- the socket exists ---
    //
    // Raw `libc::socket` rather than `rustix`'s: the third argument is the
    // protocol *in network order*, and rustix's `socket` takes it as a typed
    // `Protocol` that has no slot for "the two bytes as they sit on the wire".
    let raw = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_DGRAM, ETH_P_IP) };
    check_ok(
        &mut r,
        "socket(AF_PACKET, SOCK_DGRAM) opens",
        as_raw(raw as isize),
    );
    if raw < 0 {
        r.finish();
    }
    let sock = unsafe { OwnedFd::from_raw_fd(raw) };

    // --- and it names itself like a link ---
    let mut name = SockaddrLl::default();
    let mut nlen: libc::socklen_t = core::mem::size_of::<SockaddrLl>() as libc::socklen_t;
    let got = unsafe {
        as_raw(libc::getsockname(
            sock.as_raw_fd(),
            (&raw mut name).cast::<libc::sockaddr>(),
            &raw mut nlen,
        ) as isize)
    };
    check_ok(&mut r, "getsockname answers", got);
    r.check(
        &format!(
            "the name is a sockaddr_ll (family {}, len {nlen})",
            name.family
        ),
        name.family == libc::AF_PACKET as u16,
    );
    let mac = [
        name.addr[0],
        name.addr[1],
        name.addr[2],
        name.addr[3],
        name.addr[4],
        name.addr[5],
    ];
    r.check(
        &format!("and its address is the device's real MAC ({mac:02x?})"),
        mac != [0u8; 6],
    );
    r.check(
        &format!(
            "with a hardware length a frame header can carry ({})",
            name.halen
        ),
        name.halen == 6,
    );

    // --- bind: the index the kernel reports for eth0, not a number remembered
    // from another machine ---
    let ifindex = match socket(AddressFamily::INET, SocketType::DGRAM, None) {
        Ok(probe) => {
            let mut req = Ifreq::named("eth0");
            let rc = unsafe {
                as_raw(libc::syscall(
                    libc::SYS_ioctl,
                    probe.as_raw_fd() as libc::c_long,
                    SIOCGIFINDEX,
                    (&raw mut req) as *mut c_void,
                ) as isize)
            };
            check_ok(&mut r, "SIOCGIFINDEX on eth0 answers", rc);
            if rc < 0 { 0 } else { req.ifindex() }
        }
        Err(e) => {
            r.check(
                &format!("a probe socket for SIOCGIFINDEX opens ({e:?})"),
                false,
            );
            0
        }
    };
    r.check(
        &format!("eth0 has an interface index (got {ifindex})"),
        ifindex > 0,
    );
    if ifindex <= 0 {
        r.finish();
    }

    let mut sll = SockaddrLl {
        family: libc::AF_PACKET as u16,
        protocol: ETH_P_IP as u16,
        ifindex,
        hatype: ARPHRD_ETHER,
        halen: 6,
        ..SockaddrLl::default()
    };
    let got = unsafe {
        as_raw(libc::bind(
            sock.as_raw_fd(),
            (&raw mut sll).cast::<libc::sockaddr>(),
            core::mem::size_of::<SockaddrLl>() as libc::socklen_t,
        ) as isize)
    };
    check_ok(&mut r, "bind to eth0's index succeeds", got);

    // --- the family's other shape: SOCK_RAW ---
    //
    // A `SOCK_RAW` socket differs from the `SOCK_DGRAM` one in exactly two
    // places, and this program checks both. On *receive* it is promised the
    // frame with its Ethernet header on, where `SOCK_DGRAM` is promised the
    // payload half. On *send* it carries the caller's own header, where
    // `SOCK_DGRAM` has the kernel build one. Opened before the DISCOVER below
    // so that its queue catches the same answer.
    let rawpkt = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, ETH_P_IP) };
    check_ok(
        &mut r,
        "socket(AF_PACKET, SOCK_RAW) opens",
        as_raw(rawpkt as isize),
    );
    if rawpkt < 0 {
        r.finish();
    }
    let rawpkt = unsafe { OwnedFd::from_raw_fd(rawpkt) };
    let got = unsafe {
        as_raw(libc::bind(
            rawpkt.as_raw_fd(),
            (&raw mut sll).cast::<libc::sockaddr>(),
            core::mem::size_of::<SockaddrLl>() as libc::socklen_t,
        ) as isize)
    };
    check_ok(&mut r, "and binds to eth0's index too", got);

    // A type the family does not serve is `ESOCKTNOSUPPORT`, not the
    // `EOPNOTSUPP` this door used to give every non-`SOCK_DGRAM` type: measured
    // on the host, `SOCK_STREAM`/`SOCK_SEQPACKET`/`SOCK_RDM` are all errno 94.
    let stream = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_STREAM, ETH_P_IP) };
    r.check(
        &format!(
            "AF_PACKET SOCK_STREAM is ESOCKTNOSUPPORT (got {})",
            as_raw(stream as isize)
        ),
        as_raw(stream as isize) == -libc::ESOCKTNOSUPPORT as isize,
    );

    // --- a DISCOVER, octet by octet: 20 IP + 8 UDP + 264 BOOTP+options ---
    const TOTAL: usize = 292;
    // The two bases the fields are named against. The IPv4 destination is
    // header-relative 16 (bytes 20..24 are the UDP port pair), and the BOOTP
    // message starts 28 bytes in - so its magic cookie, at BOOTP-relative 236,
    // is byte 264 of this buffer, not byte 236. Both were once written at the
    // wrong base, and the packet left the machine addressed to 0.0.0.0 with no
    // cookie in it: `sendto` returned the whole length - the kernel sends the
    // bytes it is handed - and the host's BOOTP server, having nothing to
    // recognise, never answered. The bug was in this construction, not in the
    // door it is testing.
    const IP_DST: usize = 16;
    const BOOTP: usize = 28;
    let mut pkt = [0u8; TOTAL];
    // IPv4 header
    pkt[0] = 0x45;
    pkt[2..4].copy_from_slice(&(TOTAL as u16).to_be_bytes());
    pkt[8] = 64; // ttl
    pkt[9] = 17; // UDP
    // src 0.0.0.0 (as an addressless client sends), dst 255.255.255.255
    pkt[IP_DST..IP_DST + 4].copy_from_slice(&[255, 255, 255, 255]);
    let ip_csum = cksum(&pkt[0..20]).to_be_bytes();
    pkt[10..12].copy_from_slice(&ip_csum);
    // UDP: bootp client port 68 to server port 67; a zero checksum is legal for
    // IPv4 UDP and what the real clients send.
    pkt[20..22].copy_from_slice(&68u16.to_be_bytes());
    pkt[22..24].copy_from_slice(&67u16.to_be_bytes());
    pkt[24..26].copy_from_slice(&((TOTAL - 20) as u16).to_be_bytes());
    // BOOTP request, starting at the UDP payload (offset 28)
    pkt[BOOTP] = 1; // op = BOOTREQUEST
    pkt[BOOTP + 1] = 1; // htype = Ethernet
    pkt[BOOTP + 2] = 6; // hlen
    pkt[BOOTP + 4..BOOTP + 8].copy_from_slice(&0x4d504f53u32.to_be_bytes()); // xid
    pkt[BOOTP + 10..BOOTP + 12].copy_from_slice(&0x8000u16.to_be_bytes()); // broadcast flag
    pkt[BOOTP + 28..BOOTP + 34].copy_from_slice(&mac); // chaddr
    pkt[BOOTP + 236..BOOTP + 240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie
    // options: message-type DISCOVER, client-id, end.
    pkt[BOOTP + 240] = 53;
    pkt[BOOTP + 241] = 1;
    pkt[BOOTP + 242] = 1;
    pkt[BOOTP + 243] = 61;
    pkt[BOOTP + 244] = 7;
    pkt[BOOTP + 245] = 1;
    pkt[BOOTP + 246..BOOTP + 252].copy_from_slice(&mac);
    pkt[BOOTP + 252] = 255;

    // The destination: the broadcast. With `halen` left zero the kernel reads
    // "no link address named", which for `SOCK_DGRAM` means the all-ones
    // address - and the frame header itself is the kernel's to build from the
    // device's own MAC.
    let mut dst = SockaddrLl {
        family: libc::AF_PACKET as u16,
        protocol: ETH_P_IP as u16,
        ifindex,
        ..SockaddrLl::default()
    };
    let sent = unsafe {
        as_raw(libc::sendto(
            sock.as_raw_fd(),
            pkt.as_ptr().cast::<c_void>(),
            TOTAL,
            0,
            (&raw mut dst).cast::<libc::sockaddr>(),
            core::mem::size_of::<SockaddrLl>() as libc::socklen_t,
        ) as isize)
    };
    r.check(
        &format!("the DISCOVER goes out whole (got {sent}, want {TOTAL})"),
        sent == TOTAL as isize,
    );

    // --- the answer: the host's BOOTP server replies, and we read it back ---
    const PATIENCE: Timespec = Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let mut waiting = [PollFd::new(&sock, PollFlags::IN)];
    let ready = poll(&mut waiting, Some(&PATIENCE));
    r.check(
        &format!("an answer arrives within five seconds ({ready:?})"),
        ready == Ok(1),
    );
    if ready != Ok(1) {
        r.finish();
    }

    // The look and the length word, before the read that takes the frame.
    // Measured on a current host first (`.workbuddy/probe_packet_peek.c`,
    // Linux 7.2.11): `MSG_PEEK` copies the frame's front and leaves it for the
    // next receive, and `MSG_TRUNC` makes the return that frame's *own* length
    // even through an 8-byte room - while the same 8-byte room without the
    // flag answers 8 and consumes the frame. Both flags used to be refused or
    // ignored on this family's doors.
    let mut look = [0u8; 8];
    let looked = unsafe {
        as_raw(libc::recvfrom(
            sock.as_raw_fd(),
            look.as_mut_ptr().cast::<c_void>(),
            look.len(),
            libc::MSG_PEEK | libc::MSG_TRUNC,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) as isize)
    };
    r.check(
        &format!("a peek with MSG_TRUNC names the frame's length through 8 bytes (got {looked})"),
        looked >= TOTAL as isize,
    );

    let mut rx = [0u8; 600];
    // The sender the DGRAM read names, kept so the raw read's header can be
    // checked against it below: the two doors answer about the same frame.
    let mut sender = SockaddrLl::default();
    let mut senderlen = core::mem::size_of::<SockaddrLl>() as libc::socklen_t;
    let got = unsafe {
        as_raw(libc::recvfrom(
            sock.as_raw_fd(),
            rx.as_mut_ptr().cast::<c_void>(),
            rx.len(),
            0,
            (&raw mut sender).cast::<libc::sockaddr>(),
            &raw mut senderlen,
        ) as isize)
    };
    r.check(
        &format!("the reply reads back whole (got {got}, want at least {TOTAL})"),
        got >= TOTAL as isize,
    );
    if got >= TOTAL as isize {
        // The peek's two halves, now that the taken frame is in hand: the look
        // consumed nothing (the read is the same frame, byte for byte) and its
        // return was that frame's own length.
        r.check(
            &format!("the peek left that frame for the read (looked {looked}, read {got})"),
            looked == got && look[..] == rx[..8],
        );
    }
    if got >= TOTAL as isize {
        r.check(
            &format!("the reply is a BOOTREPLY (got {})", rx[BOOTP]),
            rx[BOOTP] == 2,
        );
        let xid = u32::from_be_bytes([rx[BOOTP + 4], rx[BOOTP + 5], rx[BOOTP + 6], rx[BOOTP + 7]]);
        r.check(
            &format!("and it carries our transaction id (got {xid:#x})"),
            xid == 0x4d504f53,
        );
        // The magic cookie, which is the other half of "this is a BOOTP answer"
        // rather than an echo of what we sent: 99.130.83.99 at BOOTP+236.
        r.check(
            "and it carries the BOOTP magic cookie",
            rx[BOOTP + 236..BOOTP + 240] == [99, 130, 83, 99],
        );
        // And it is addressed to us: the chaddr a BOOTP server copies back is
        // the one it was asked about.
        r.check(
            &format!(
                "and it names our hardware address ({:02x?})",
                &rx[BOOTP + 28..BOOTP + 34]
            ),
            rx[BOOTP + 28..BOOTP + 34] == mac,
        );

        // The same answer again, through the raw socket that was open beside
        // this one when the DISCOVER went out: the *same* bytes with fourteen
        // more in front. The comparison is the claim - a raw read that
        // returned the payload would be short by exactly the header, and one
        // that returned a *different* frame would not end where this one does.
        let mut rxraw = [0u8; 640];
        let raw_got = unsafe {
            as_raw(libc::recvfrom(
                rawpkt.as_raw_fd(),
                rxraw.as_mut_ptr().cast::<c_void>(),
                rxraw.len(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ) as isize)
        };
        r.check(
            &format!(
                "the raw socket reads it too, with the header on (got {raw_got}, want {})",
                got + ETH_HEADER_LEN as isize
            ),
            raw_got == got + ETH_HEADER_LEN as isize,
        );
        r.check(
            &format!(
                "whose type is IPv4's 08 00 ({:02x} {:02x})",
                rxraw[12], rxraw[13]
            ),
            rxraw[12] == 0x08 && rxraw[13] == 0x00,
        );
        r.check(
            &format!(
                "whose source MAC is the sender the DGRAM read named ({:02x?})",
                &sender.addr[..6]
            ),
            rxraw[6..12] == sender.addr[..6],
        );
        if raw_got > ETH_HEADER_LEN as isize {
            let n = raw_got as usize - ETH_HEADER_LEN;
            r.check(
                "and whose payload half is the DGRAM reader's frame, byte for byte",
                rxraw[ETH_HEADER_LEN..raw_got as usize] == rx[..n],
            );
        }
    }

    // --- the same DISCOVER through the door a shipping client opens first ---
    //
    // `busybox udhcpc` builds no Ethernet frame: its first call is
    // `socket(AF_INET, SOCK_RAW, IPPROTO_RAW)` - measured on the machine, because
    // §4.5's boot stopped exactly there - and the packet it sends carries its
    // *own* IP header. The kernel's half is Linux's, measured on the host with
    // `.workbuddy/probe_rawip.c`: the total length becomes the sent length, the
    // header checksum is recomputed, the source is filled in when the caller left
    // it zero, the identification is assigned when the caller left it zero, and
    // the *header's* destination is the packet's (the argument is required all the
    // same - `EDESTADDRREQ` without it). This header leaves the source at 0.0.0.0,
    // so the answer arriving at all is that rule doing its work: a server can only
    // reply to an address.
    //
    // It also pins *where* the send goes: smoltcp's raw-socket emit drops a packet
    // whose IP protocol byte is not the socket's (`socket/raw.rs`: "wrong ip
    // protocol, dropping"), and this channel is bound to 255 while the packet is
    // UDP - so a kernel that sent it through that socket would queue it, return
    // the full length from `sendto`, and hear nothing. That was measured here once
    // (`Ok(0)` from the poll below); the door now reaches the device directly.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_RAW) };
    check_ok(
        &mut r,
        "AF_INET SOCK_RAW with IPPROTO_RAW opens",
        raw as isize,
    );
    if raw < 0 {
        r.finish();
    }
    // A fresh transaction id, so the answer below cannot be the first DISCOVER's
    // (a server is entitled to ignore a retransmit).
    const RAW_XID: u32 = 0x4d504f54;
    pkt[BOOTP + 4..BOOTP + 8].copy_from_slice(&RAW_XID.to_be_bytes());

    let refused = unsafe {
        as_raw(libc::sendto(
            raw,
            pkt.as_ptr().cast::<c_void>(),
            TOTAL,
            0,
            core::ptr::null(),
            0,
        ) as isize)
    };
    r.check(
        "sendto through the raw door without a destination is EDESTADDRREQ",
        refused == -(libc::EDESTADDRREQ as isize),
    );

    let mut broadcast = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes([255, 255, 255, 255]),
        },
        sin_zero: [0; 8],
    };
    let sent = unsafe {
        as_raw(libc::sendto(
            raw,
            pkt.as_ptr().cast::<c_void>(),
            TOTAL,
            0,
            (&raw mut broadcast).cast::<libc::sockaddr>(),
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) as isize)
    };
    r.check(
        &format!("and with one the whole packet goes (got {sent}, want {TOTAL})"),
        sent == TOTAL as isize,
    );

    let mut waiting_again = [PollFd::new(&sock, PollFlags::IN)];
    let ready_again = poll(&mut waiting_again, Some(&PATIENCE));
    r.check(
        &format!("the server answers through the raw door too ({ready_again:?})"),
        ready_again == Ok(1),
    );
    if ready_again == Ok(1) {
        // Read this one the way a shipping client does, and not the way the
        // first answer was read. `busybox udhcpc` asks for its replies with
        // **`recvmsg`** - measured on the host (`strace`: `recvmsg(5,
        // {msg_iovlen=1, msg_control=...})`) - because it wants
        // `PACKET_AUXDATA` beside the payload; its listen socket is the
        // `AF_PACKET` one. The `recvmsg` door had no branch for that family,
        // so the call fell through to the registry's "not a socket" answer and
        // the client printed `read error: Not a socket, reopening socket`
        // after every DISCOVER - while this program, reading the same socket
        // with `recvfrom`, stayed green. The two reads are the two doors, and
        // a green pair is the claim that both answer.
        // The look on this door too: a `recvmsg` with `MSG_PEEK` and
        // `MSG_TRUNC` through 8 bytes answers the frame's own length with the
        // cut named in `msg_flags`, and the read after it is the same frame -
        // measured with the `recvfrom` claims above
        // (`.workbuddy/probe_packet_peek.c`, Linux 7.2.11).
        let mut look = [0u8; 8];
        let mut look_iov = [libc::iovec {
            iov_base: look.as_mut_ptr().cast::<c_void>(),
            iov_len: look.len(),
        }];
        let mut look_msg: libc::msghdr = unsafe { core::mem::zeroed() };
        look_msg.msg_iov = look_iov.as_mut_ptr();
        look_msg.msg_iovlen = 1;
        let peeked = unsafe {
            as_raw(libc::recvmsg(
                sock.as_raw_fd(),
                &mut look_msg,
                libc::MSG_PEEK | libc::MSG_TRUNC,
            ) as isize)
        };
        r.check(
            &format!(
                "a recvmsg peek with MSG_TRUNC names the frame's length (got {peeked}, flags {})",
                look_msg.msg_flags
            ),
            peeked >= TOTAL as isize && look_msg.msg_flags & libc::MSG_TRUNC != 0,
        );

        let mut rx2 = [0u8; 600];
        let mut iov = [libc::iovec {
            iov_base: rx2.as_mut_ptr().cast::<c_void>(),
            iov_len: rx2.len(),
        }];
        // Room for a `tpacket_auxdata` control message, as the client leaves
        // it: the kernel filling none is the honest answer, and the client
        // only reads it to *skip* a checksum check it is prepared to make.
        let mut control = [0u8; 64];
        let mut name = [0u8; 20];
        let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
        msg.msg_name = name.as_mut_ptr().cast::<c_void>();
        msg.msg_namelen = name.len() as libc::socklen_t;
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast::<c_void>();
        msg.msg_controllen = control.len() as libc::socklen_t;
        let got = unsafe { as_raw(libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) as isize) };
        r.check(
            &format!("and recvmsg on the same socket reads it too (got {got}, want > 0)"),
            got > 0,
        );
        r.check(
            &format!("the peek left it for that read (peeked {peeked}, read {got})"),
            peeked == got && look[..] == rx2[..8],
        );
        r.check(
            &format!(
                "naming the sender in a sockaddr_ll (namelen {}, family {})",
                msg.msg_namelen,
                u16::from_ne_bytes([name[0], name[1]])
            ),
            msg.msg_namelen as usize == 20 && u16::from_ne_bytes([name[0], name[1]]) == 17,
        );
        if got > 0 {
            let at = BOOTP + 4;
            let xid = u32::from_be_bytes([rx2[at], rx2[at + 1], rx2[at + 2], rx2[at + 3]]);
            r.check(
                &format!("carrying the raw door's transaction id ({xid:#x})"),
                xid == RAW_XID,
            );
        }
    }

    // The receive half is this door's own queue - a filterless smoltcp socket
    // would steal the machine's UDP and TCP delivery (§4.5), so the NIC seam
    // copies every delivered IPv4 packet here instead. What `recvfrom` hands
    // back is the *complete* IP packet: header first, its source the sender.
    // Polled through the door's own descriptor first - a reply that reaches
    // the AF_PACKET tap must reach this one too, and a poll that times out
    // says the seam never filled it (a bare libc fd borrows as an `AsFd` for
    // `PollFd::new`; the read below then takes with `MSG_DONTWAIT`, because
    // this sweep has no watchdog to notice a park).
    let raw_fd = unsafe { BorrowedFd::borrow_raw(raw) };
    let mut waiting_door = [PollFd::new(&raw_fd, PollFlags::IN)];
    let door_ready = poll(&mut waiting_door, Some(&PATIENCE));
    r.check(
        &format!("the door goes readable on its own ({door_ready:?})"),
        door_ready == Ok(1),
    );
    // `rx3`/`again` live outside the block: the renewal claim below reads the
    // same packet back.
    let mut rx3 = [0u8; 600];
    let mut again = -1isize;
    if door_ready == Ok(1) {
        again = unsafe {
            as_raw(libc::recvfrom(
                raw,
                rx3.as_mut_ptr().cast::<c_void>(),
                rx3.len(),
                libc::MSG_DONTWAIT,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ) as isize)
        };
        r.check(
            &format!("the door itself reads one whole packet (got {again})"),
            (20..=600isize).contains(&again),
        );
        if again >= 20 {
            let version = rx3[0] >> 4;
            let ihl = (rx3[0] & 0x0f) as usize * 4;
            let total = u16::from_be_bytes([rx3[2], rx3[3]]) as usize;
            let proto = rx3[9];
            let src = [rx3[12], rx3[13], rx3[14], rx3[15]];
            r.check(
                &format!("starting with a v4 header, not a payload (version {version})"),
                version == 4 && ihl >= 20 && total <= again as usize,
            );
            r.check(
                &format!("whose protocol byte is the reply's ({proto}) from slirp ({src:?})"),
                proto == 17 && src == [10, 0, 2, 2],
            );
        }
    }

    // --- the unicast half: a destination that is not the broadcast ---
    //
    // Every socket but this one has always sent unicast, and the lookup it
    // needs - which neighbour, on the wire or by the router - is the ARP half
    // §4.5 named. `udhcpc`'s own next step after the discover is exactly that:
    // it renews with a *unicast* REQUEST aimed at the server that answered, so
    // the packet read above supplies the aim - rewrite the header's destination
    // with the OFFER's source (the kernel recomputes the checksum) and send.
    // The send resolves the next hop now: on-link the destination itself, off
    // it the router a matching route names, and no route at all is
    // `ENETUNREACH` (measured on the host). An unanswered neighbour is
    // `EHOSTUNREACH` - the one divergence from Linux, which queues and returns
    // a length; a synchronous stack has nowhere to queue, so it says the frame
    // did not leave rather than returning a length for one that never did.
    let mut unicast = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes([10, 0, 2, 2]),
        },
        sin_zero: [0; 8],
    };
    let req = if again >= 20 {
        let server = [rx3[12], rx3[13], rx3[14], rx3[15]];
        pkt[IP_DST..IP_DST + 4].copy_from_slice(&server);
        unsafe {
            as_raw(libc::sendto(
                raw,
                pkt.as_ptr().cast::<c_void>(),
                TOTAL,
                0,
                (&raw mut unicast).cast::<libc::sockaddr>(),
                core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) as isize)
        }
    } else {
        -1
    };
    r.check(
        &format!("a unicast through the raw door goes (got {req}, want {TOTAL})"),
        req == TOTAL as isize,
    );

    // And proof the frame actually left for a resolved neighbour, not merely
    // that the call returned a length: an ICMP echo whose reply the peer sends
    // back. The peer is beyond slirp, so the next hop is the *router* (the
    // default route's gateway) rather than the destination itself, which
    // exercises the route half of the lookup too. The IP header and the ICMP
    // checksum are built here - the door carries the caller's packet untouched
    // - and the reply is read off the raw-IP receive queue, which the seam
    // fills with every IPv4 packet it delivers. Only a frame the resolved link
    // address (the router's MAC, learned by the ARP probe the door ran) carried
    // can bring this back. The address is the one `t-rawicmp` pings, so both
    // tests share the one external dependency the `external` group already
    // accepts.
    const PEER: [u8; 4] = [110, 242, 74, 102];
    let mut peer = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_be_bytes(PEER),
        },
        sin_zero: [0; 8],
    };
    let mut echo = [0u8; 28]; // 20 IPv4 + 8 ICMP
    echo[0] = 0x45;
    echo[2..4].copy_from_slice(&28u16.to_be_bytes());
    echo[8] = 64; // ttl
    echo[9] = 1; // ICMP
    echo[12..16].copy_from_slice(&[10, 0, 2, 15]); // our source
    echo[16..20].copy_from_slice(&PEER); // the peer, off-link
    let ec = cksum(&echo[0..20]).to_be_bytes();
    echo[10..12].copy_from_slice(&ec);
    echo[20] = 8; // ICMP echo request
    echo[24..26].copy_from_slice(&0x504du16.to_be_bytes()); // identifier
    echo[26..28].copy_from_slice(&1u16.to_be_bytes()); // sequence
    let ic = cksum(&echo[20..28]).to_be_bytes();
    echo[22..24].copy_from_slice(&ic);
    let echo_sent = unsafe {
        as_raw(libc::sendto(
            raw,
            echo.as_ptr().cast::<c_void>(),
            echo.len(),
            0,
            (&raw mut peer).cast::<libc::sockaddr>(),
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) as isize)
    };
    r.check(
        &format!("a unicast ICMP echo through it is sent (got {echo_sent})"),
        echo_sent == echo.len() as isize,
    );

    // The raw-IP door holds earlier answers still unread (the unicast DISCOVER
    // may have earned one among them); drain them so the wait below is this
    // echo's own reply and no earlier packet's.
    let mut drain = [0u8; 640];
    while unsafe {
        as_raw(libc::recvfrom(
            raw,
            drain.as_mut_ptr().cast::<c_void>(),
            drain.len(),
            libc::MSG_DONTWAIT,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) as isize)
    } >= 0
    {}

    let raw_echo_fd = unsafe { BorrowedFd::borrow_raw(raw) };
    let mut waiting_echo = [PollFd::new(&raw_echo_fd, PollFlags::IN)];
    let echo_ready = poll(&mut waiting_echo, Some(&PATIENCE));
    r.check(
        &format!("and the peer answers it with an echo reply ({echo_ready:?})"),
        echo_ready == Ok(1),
    );
    if echo_ready == Ok(1) {
        // Scan every readable packet, not just the first: a late DHCP answer
        // may share the queue with the echo reply, and this claim is about the
        // echo's own.
        let mut reply = [0u8; 640];
        let mut from_peer = false;
        loop {
            let n = unsafe {
                as_raw(libc::recvfrom(
                    raw,
                    reply.as_mut_ptr().cast::<c_void>(),
                    reply.len(),
                    libc::MSG_DONTWAIT,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                ) as isize)
            };
            if n < 20 {
                break;
            }
            if reply[9] == 1 && reply[12..16] == PEER {
                from_peer = true;
                break;
            }
        }
        r.check(
            "whose protocol is ICMP and whose source is the peer",
            from_peer,
        );
    }

    // --- a DISCOVER sent through the SOCK_RAW socket, header and all ---
    //
    // Where the `SOCK_DGRAM` send above handed the kernel a payload, a
    // `SOCK_RAW` send carries the caller's own Ethernet header and the bytes go
    // to the wire untouched. Built here the way an ARP client or a bridge would,
    // with the broadcast destination a DISCOVER needs. The `sockaddr_ll` names
    // *no* address on purpose: a raw send takes the interface from it and the
    // destination from the frame's own header (measured - the header won when
    // the two disagreed), so a reply proves the header reached the wire intact.
    const RAWPKT_XID: u32 = 0x4d504f55;
    // The unicast section above rewrote the header's destination to the
    // server's address and left the checksum stale; this frame is a broadcast
    // DISCOVER again, so restore the broadcast address and recompute its
    // checksum - a server drops a packet whose IP checksum is wrong. The
    // checksum field is zeroed before the sum, the way the original build
    // computed it (summing the field over itself does not).
    pkt[IP_DST..IP_DST + 4].copy_from_slice(&[255, 255, 255, 255]);
    pkt[10..12].copy_from_slice(&[0, 0]);
    let ip_csum = cksum(&pkt[0..20]).to_be_bytes();
    pkt[10..12].copy_from_slice(&ip_csum);
    pkt[BOOTP + 4..BOOTP + 8].copy_from_slice(&RAWPKT_XID.to_be_bytes());

    // The raw socket has been open through every door above, so it holds each
    // earlier answer still unread (the first DISCOVER's was read, but the
    // raw-IP door's was read on the *other* socket). Drain them so the read
    // below is this send's own answer and no earlier frame's.
    let mut scratch = [0u8; 640];
    loop {
        let n = unsafe {
            as_raw(libc::recvfrom(
                rawpkt.as_raw_fd(),
                scratch.as_mut_ptr().cast::<c_void>(),
                scratch.len(),
                libc::MSG_DONTWAIT,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ) as isize)
        };
        if n < 0 {
            break;
        }
    }

    let mut rawdst = SockaddrLl {
        family: libc::AF_PACKET as u16,
        protocol: ETH_P_IP as u16,
        ifindex,
        ..SockaddrLl::default()
    };
    let mut built = [0u8; ETH_HEADER_LEN + TOTAL];
    built[0..6].copy_from_slice(&[0xff; 6]); // broadcast destination
    built[6..12].copy_from_slice(&mac); // the device's own source
    built[12] = 0x08;
    built[13] = 0x00; // ETH_P_IP
    built[ETH_HEADER_LEN..].copy_from_slice(&pkt);

    // A frame with no room for a header is not a frame: `EINVAL`, measured (a
    // 13-byte send with a real `sockaddr_ll` is refused, a 14-byte one is not).
    let runt = unsafe {
        as_raw(libc::sendto(
            rawpkt.as_raw_fd(),
            built.as_ptr().cast::<c_void>(),
            ETH_HEADER_LEN - 1,
            0,
            (&raw mut rawdst).cast::<libc::sockaddr>(),
            core::mem::size_of::<SockaddrLl>() as libc::socklen_t,
        ) as isize)
    };
    r.check(
        &format!("a raw send shorter than a header is EINVAL (got {runt})"),
        runt == -(libc::EINVAL as isize),
    );

    // And a raw send with no `sockaddr_ll` cannot name the interface at all,
    // even though the frame names every MAC it needs: `ENXIO`, measured. The
    // address check runs before the length one, so this is the whole frame.
    let headless = unsafe {
        as_raw(libc::sendto(
            rawpkt.as_raw_fd(),
            built.as_ptr().cast::<c_void>(),
            built.len(),
            0,
            core::ptr::null(),
            0,
        ) as isize)
    };
    r.check(
        &format!("a raw send with no sockaddr_ll is ENXIO (got {headless})"),
        headless == -(libc::ENXIO as isize),
    );

    let sent_raw = unsafe {
        as_raw(libc::sendto(
            rawpkt.as_raw_fd(),
            built.as_ptr().cast::<c_void>(),
            built.len(),
            0,
            (&raw mut rawdst).cast::<libc::sockaddr>(),
            core::mem::size_of::<SockaddrLl>() as libc::socklen_t,
        ) as isize)
    };
    r.check(
        &format!(
            "the whole frame goes through the raw socket (got {sent_raw}, want {})",
            built.len()
        ),
        sent_raw == built.len() as isize,
    );

    // The server answers it - proof the caller's frame reached the wire
    // unchanged, because only the header's broadcast destination and the BOOTP
    // transaction id we wrote can have brought a reply back.
    let mut waiting_raw = [PollFd::new(&rawpkt, PollFlags::IN)];
    let ready_raw = poll(&mut waiting_raw, Some(&PATIENCE));
    r.check(
        &format!("and the raw-built DISCOVER is answered ({ready_raw:?})"),
        ready_raw == Ok(1),
    );
    if ready_raw == Ok(1) {
        let mut rxr = [0u8; 640];
        let n = unsafe {
            as_raw(libc::recvfrom(
                rawpkt.as_raw_fd(),
                rxr.as_mut_ptr().cast::<c_void>(),
                rxr.len(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ) as isize)
        };
        let at = ETH_HEADER_LEN + BOOTP + 4;
        let xid = if n as usize >= at + 4 {
            u32::from_be_bytes([rxr[at], rxr[at + 1], rxr[at + 2], rxr[at + 3]])
        } else {
            0
        };
        r.check(
            &format!("carrying the transaction id we wrote ({xid:#x})"),
            xid == RAWPKT_XID,
        );
    }

    unsafe { libc::close(raw as i32) };

    r.finish();
}
