//! t-rawicmp - what a raw socket is actually handed: header bytes and all.
//!
//! `ping`'s `ttl=` is either the IP header riding at the front of the received
//! packet or the `IP_RECVTTL` control message - and when the numbers it prints
//! wander between 9 and 207 for one steady peer, the question is which of those
//! paths lies. This test pings one address by hand, then prints the received
//! bytes as the kernel delivered them: version/ihl, the ttl octet at offset 8,
//! protocol and checksum, the source address, the ICMP type behind the header,
//! and separately whatever `IP_RECVTTL` attached - so the two witnesses can be
//! compared against each other and against the stable answer the host's own
//! ping gets for the same address.
//!
//! # The doors, and why this file has three
//!
//! The socket is rustix's - `socket(AF_INET, SOCK_RAW, Some(ipproto::ICMP))`
//! names the protocol (`t-netlat`'s door, the same three words). Both wire
//! claims ride `libc::syscall` with the numbers from `libc`'s maintained
//! per-machine tables (`SYS_sendto` 44/206, `SYS_recvmsg` 47/212), clause (b)
//! measured on rustix 1.1.5: its `recvfrom` decodes the sender out of its own
//! fixed 128-byte `sockaddr_storage`, whose length the wrapper hands the kernel
//! as the in-out word, so "the kernel wrote back *sixteen* into the slot I
//! named" cannot be claimed through it; and its `RecvAncillaryMessage` names
//! only `SCM_RIGHTS` and `SCM_CREDENTIALS` - `IpTtl` arrived in a later minor
//! than this workspace pins - while this file's subject *is* that record's
//! wire shape: level, type, the data at offset 16, and the kernel's write-back
//! of `msg_controllen`. The clock is `rustix::time::clock_gettime(ClockId::
//! Monotonic)` in milliseconds, and `setsockopt(IP_RECVTTL)` rides
//! `libc::setsockopt` with both words from `libc`'s table (`SOL_IP`,
//! `IP_RECVTTL`).
//!
//! One design fact moves with the program: a raw socket on this kernel lives on
//! eth0, never on the loopback interface (`src/net/mod.rs`'s two-fabrics note),
//! so the peer is a real address beyond slirp - which makes the internet half
//! of this test exactly what batch 34 refused to pretend otherwise about. A
//! host run answers about its own route, its own ttl and its own CAP_NET_RAW;
//! the pin is the two sweeps, where the same slirp answers either machine's
//! guest identically.

use std::net::Ipv4Addr;

use mpos_user_tests::{Report, host_run};
use rustix::fd::{AsFd, AsRawFd, OwnedFd};
use rustix::net::{AddressFamily, SocketType, ipproto, socket};
use rustix::time::{ClockId, clock_gettime};

/// 110.242.74.102 - one of baidu's front addresses, the same one the
/// literal-address control experiment pings.
const PEER: Ipv4Addr = Ipv4Addr::new(110, 242, 74, 102);

const ECHO_ID: u16 = 0xBEEF;

fn icmp_cksum(buf: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < buf.len() {
        sum += u16::from_be_bytes([buf[i], buf[i + 1]]) as u32;
        i += 2;
    }
    if i < buf.len() {
        sum += (buf[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Monotonic milliseconds, for a per-answer clock.
fn ms() -> u64 {
    let ts = clock_gettime(ClockId::Monotonic);
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

/// musl's protocol read back beside the return code rather than through a
/// global - `t-ioctl`'s door shape.
fn errno_of(rc: isize) -> i32 {
    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` on the raw door: `(count,
/// errno)`. The number comes from `libc::SYS_sendto`, the maintained per-machine
/// table (44 here, 206 on the second machine).
fn sendto_raw(fd: &OwnedFd, buf: &[u8], addr: &WireAddr) -> (isize, i32) {
    // SAFETY: `buf` and `addr` are caller-owned and live for the call; the
    // lengths passed are their own sizes.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sendto,
            fd.as_raw_fd() as libc::c_long,
            buf.as_ptr() as libc::c_ulong as libc::c_long,
            buf.len() as libc::c_ulong as libc::c_long,
            0 as libc::c_long,
            addr as *const WireAddr as libc::c_ulong as libc::c_long,
            // The length word, at the struct's real size. rustix's typed
            // `sendto` passes exactly this (16), which is how batch 41's
            // `t-netlat` reaches the same door; measured, this kernel's reader
            // (`parse_sockaddr_in`) compares its own struct's size against the
            // caller's word and answers EINVAL for a shorter one - where
            // Linux's `move_addr_from_user` ignores the in-length entirely. So
            // the sixteen-byte word is Linux's shape *and* this kernel's, and
            // it is what both machines' guests see.
            std::mem::size_of::<WireAddr>() as libc::c_long,
        )
    };
    (rc as isize, errno_of(rc as isize))
}

/// The msghdr's field offsets, in the layout `src/net/socket.rs`'s reader
/// takes: name@0, namelen@8 (a `socklen_t` word), iov@16, iovlen@24,
/// control@32, controllen@40, flags@48. Spelled here rather than reached
/// through `libc::msghdr`: measured on musl targets, libc binds
/// `msg_controllen` as a `socklen_t` (u32) where musl itself - and this
/// kernel's reader - use a pointer-sized word, so the crate's record is not
/// the struct the syscall speaks. The claims below are literally about what
/// the kernel writes back into those two words, which is why the header is
/// read as bytes with these names instead of through any wrapper.
const MSG_NAME: usize = 0;
const MSG_NAMELEN: usize = 8;
const MSG_IOV: usize = 16;
const MSG_IOVLEN: usize = 24;
const MSG_CONTROL: usize = 32;
const MSG_CONTROLLEN: usize = 40;
const MSG_FLAGS: usize = 48;

fn msg_word(msg: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(msg[at..at + 8].try_into().unwrap())
}

fn put_msg_word(msg: &mut [u8], at: usize, value: u64) {
    msg[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// `recvmsg(fd, msg, flags)` on the raw door, with the caller's *own* name and
/// control slots - the two in-out words are the subject, so they stay visible
/// here. Answers `(count, errno, msg_namelen, msg_controllen, msg_flags)` as
/// the kernel left them.
fn recvmsg_raw(
    fd: &OwnedFd,
    buf: &mut [u8],
    name: &mut WireAddr,
    ctl: &mut [u8],
    dontwait: bool,
) -> (isize, i32, u32, u64, i32) {
    let mut iov = WireIovec {
        base: buf.as_mut_ptr() as usize,
        len: buf.len() as u64,
    };
    // Sixty-four bytes of zero: the 56-byte header plus slack, because the
    // fields sit at byte offsets above, not at a struct's natural ones.
    let mut msg = [0u8; 64];
    put_msg_word(&mut msg, MSG_NAME, name as *mut WireAddr as u64);
    put_msg_word(
        &mut msg,
        MSG_NAMELEN,
        std::mem::size_of::<WireAddr>() as u64,
    );
    put_msg_word(&mut msg, MSG_IOV, std::ptr::from_mut(&mut iov) as u64);
    put_msg_word(&mut msg, MSG_IOVLEN, 1);
    put_msg_word(&mut msg, MSG_CONTROL, ctl.as_mut_ptr() as u64);
    put_msg_word(&mut msg, MSG_CONTROLLEN, ctl.len() as u64);
    // Blocking by default, as the no-std version was: measured, slirp answers
    // an ICMP echo some hundreds of microseconds *after* the guest's send
    // returns, so a `MSG_DONTWAIT` (0x40) probe reads EAGAIN on every round -
    // the flag is what broke batch 42's first draft, not the kernel. The wait
    // ends on the stack's own wakeup call (§10.2), which is exactly the shape
    // this round measures.
    // SAFETY: the header names the caller's own buffers, sized above; the
    // kernel reads the first three words and writes words 1, 5 and 6 back.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_recvmsg,
            fd.as_raw_fd() as libc::c_long,
            msg.as_mut_ptr() as libc::c_ulong as libc::c_long,
            if dontwait { 0x40 } else { 0 } as libc::c_long,
        )
    };
    (
        rc as isize,
        errno_of(rc as isize),
        msg_word(&msg, MSG_NAMELEN) as u32,
        msg_word(&msg, MSG_CONTROLLEN),
        i32::from_ne_bytes(msg[MSG_FLAGS..MSG_FLAGS + 4].try_into().unwrap()),
    )
}

/// `struct sockaddr_in` as the kernel reads it: family at zero, port
/// big-endian at two, the four octets at four, sixteen bytes total. Spelled
/// here for the same reason `t-fbdev` spells its records - the shape is the
/// subject, and the name slot's contents are claims below.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WireAddr {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

/// `struct iovec` with the kernel's pointer-sized length word - which is what
/// `src/net/socket.rs`'s reader takes at the iov slot.
#[repr(C)]
struct WireIovec {
    base: usize,
    len: u64,
}

/// One integer's worth of control data, read from offset 16 of the
/// x86_64 `cmsghdr` - the first entry's payload.
fn cmsg_int(ctl: &[u8]) -> i32 {
    cmsg_at(ctl, 16)
}

/// One word of the `cmsghdr` itself, at its byte offset.
fn cmsg_at(ctl: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(ctl[at..at + 4].try_into().unwrap())
}

fn main() {
    println!("t-rawicmp");
    let mut r = Report::new();

    // A hosted run measures its own routing, its own ttl and its own
    // privileges; the fabric this test pins is slirp's, identical under either
    // machine's guest. Named and not made, batch 34's rule.
    if host_run() {
        println!(
            "note: the public-address trip through slirp is this kernel's fabric; a host run answers about its own route, ttl and CAP_NET_RAW - named and not made"
        );
        r.finish();
    }

    let fd = match socket(AddressFamily::INET, SocketType::RAW, Some(ipproto::ICMP)) {
        Ok(fd) => fd,
        Err(e) => {
            r.check(&format!("raw ICMP socket opens ({e:?})"), false);
            r.finish();
        }
    };
    r.check("raw ICMP socket opens", true);
    let _ = fd.as_fd();

    let one: libc::c_int = 1;
    // SAFETY: SOL_IP/IP_RECVTTL with an int argument of the size declared.
    let accepted = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_IP,
            libc::IP_RECVTTL,
            std::ptr::from_ref(&one).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    r.check("setsockopt(IP_RECVTTL) is accepted", accepted == 0);

    // The peer as the kernel reads it: family at zero, port big-endian at two,
    // the four octets at four - spelled once because both doors share it, and
    // spelled *here* because the receive's name claims are about these very
    // bytes being overwritten.
    let mut addr = WireAddr {
        family: 2,
        port: 0,
        addr: PEER.octets(),
        zero: [0; 8],
    };

    // Three echo requests in the shape Linux's raw(4) asks for without
    // `IP_HDRINCL`: the ICMP message alone, kernel-built IP header. The answers
    // arrive the other way round - header first, as on Linux's receive side -
    // and it is that header's ttl octet, and its agreement with the
    // `IP_RECVTTL` control message, that this test is about.
    for seq in 1..=3u16 {
        let mut pkt = [0u8; 64];
        pkt[0] = 8; // echo request
        pkt[4] = (ECHO_ID >> 8) as u8;
        pkt[5] = ECHO_ID as u8;
        pkt[6] = (seq >> 8) as u8;
        pkt[7] = seq as u8;
        for (i, byte) in pkt[8..28].iter_mut().enumerate() {
            *byte = b'0' + (i as u8 % 10);
        }
        let cs = icmp_cksum(&pkt[..28]);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;

        let t0 = ms();
        let (sent, serr) = sendto_raw(&fd, &pkt[..28], &addr);
        if sent != 28 {
            r.check(
                &format!("echo request {seq} leaves (got {sent}, errno {serr})"),
                false,
            );
            continue;
        }

        let mut buf = [0u8; 96];
        let mut ctl = [0u8; 64];
        let (n, errno, namelen_out, controllen, msg_flags) =
            recvmsg_raw(&fd, &mut buf, &mut addr, &mut ctl, false);
        let rtt = ms().saturating_sub(t0);
        if n < 0 {
            r.check(
                &format!("round {seq} gets an echo answer (errno {errno})"),
                false,
            );
            continue;
        }
        let n = n as usize;
        println!(
            "seq {seq}: rtt={rtt}ms n={n} ver_ihl={:#04x} ttl@8={} proto@9={} icmp@20={} echo@36={} cmsg_len={controllen} cmsg_ttl={} flags={msg_flags} namelen={namelen_out} peer={}.{}.{}.{}",
            buf[0],
            buf[8],
            buf[9],
            buf[20],
            buf[36],
            cmsg_int(&ctl),
            addr.addr[0],
            addr.addr[1],
            addr.addr[2],
            addr.addr[3],
        );

        // The bar: a complete IPv4 echo-reply packet - header first, the
        // reply's ICMP type at offset 20 with our id echoed behind it, and the
        // request's data pattern beginning the echo at 28 (the peer answers
        // with twenty echoed octets; that is what arrives, and the offsets are
        // what busybox's `ping` parses its ttl from).
        r.check(
            &format!("round {seq} delivers a full IPv4 echo-reply packet"),
            n >= 37
                && buf[0] >> 4 == 4
                && buf[9] == 1
                && buf[20] == 0
                && buf[24] == 0xBE
                && buf[28] == 48,
        );
        // The kernel wrote the reply's *source* back into the name slot - the
        // IP header's sender, which is also what Linux's recvfrom names for a
        // raw socket (`src/net/socket.rs`'s raw_recv says so in its own comment)
        // - and trimmed the in-out word to the struct's sixteen bytes. Through
        // slirp the answer comes from the gateway address, not from the peer
        // itself, so what must hold on both machines is the write-back: the
        // slot left as sent would read as "no source reported".
        r.check(
            &format!("round {seq}'s msg_name is written back as an AF_INET sender"),
            addr.family == 2 && namelen_out == 16,
        );
        // The two witnesses agree. Where control data arrived it is exactly one
        // 24-byte IP_RECVTTL record - the kernel writes the length back, so a
        // nonzero `msg_controllen` with any other shape is a lie about the
        // buffer - and its int equals the header octet. Where none arrived, the
        // honest answer is zero, never a stale word off the caller's own
        // buffer.
        r.check(
            &format!("round {seq}'s control data is one well-formed IP_RECVTTL record or none"),
            controllen == 0
                || (controllen == 24
                    && cmsg_at(&ctl, 0) == 24
                    && cmsg_at(&ctl, 8) == libc::SOL_IP
                    && cmsg_at(&ctl, 12) == libc::IP_RECVTTL),
        );
        r.check(
            &format!("round {seq}'s cmsg ttl agrees with the header octet"),
            controllen != 24 || cmsg_int(&ctl) as u32 == buf[8] as u32,
        );
        r.check(
            &format!("round {seq} delivers a nonzero ttl octet"),
            (20..=255).contains(&(buf[8] as i32)),
        );
        // `msg_flags` is cleared on a delivery that fit: this kernel writes the
        // word either way, so a caller never reads back a stale one, and it is
        // the flag `ping` would have to honour if the reply were cut.
        r.check(
            &format!("round {seq} reports no truncated delivery"),
            msg_flags == 0,
        );
    }

    r.finish()
}
