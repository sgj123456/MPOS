//! t-rawicmp - what a raw socket is actually handed: header bytes and all.
//!
//! `ping`'s `ttl=` is either the IP header riding at the front of the
//! received packet or the `IP_RECVTTL` control message - and when the
//! numbers it prints wander between 9 and 207 for one steady peer, the
//! question is which of those paths lies. This test pings one address by
//! hand, then prints the received bytes as the kernel delivered them:
//! version/ihl, the ttl octet at offset 8, protocol and checksum, the
//! source address, the ICMP type behind the header, and separately
//! whatever `IP_RECVTTL` attached - so the two witnesses can be compared
//! against each other and against the stable answer the host's own ping
//! gets for the same address.

#![no_std]
#![no_main]

#[path = "../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

/// 110.242.74.102 - one of baidu's front addresses, the same one the
/// literal-address control experiment pings.
const PEER: lin::SockAddrIn = lin::SockAddrIn {
    family: 2,
    port: 0,
    addr: [110, 242, 74, 102],
    zero: [0; 8],
};

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

/// One integer's worth of control data, read from offset 16 of the
/// x86_64 `cmsghdr` - the first entry's payload.
fn cmsg_int(ctl: &[u8]) -> i32 {
    unsafe { (ctl.as_ptr().add(16) as *const i32).read_unaligned() }
}

/// Monotonic milliseconds, for a per-answer clock.
fn ms() -> u64 {
    let mut ts: lin::TimeSpec = unsafe { core::mem::zeroed() };
    lin::clock_gettime(lin::CLOCK_MONOTONIC, &mut ts);
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-rawicmp\n");
    let mut failed = false;

    let fd = lin::socket(
        lin::AF_INET as usize,
        3, /* SOCK_RAW */
        1, /* ICMP */
    ) as i32;
    lin::check_true(&mut failed, b"raw ICMP socket opens", fd >= 0);
    lin::check_eq(
        &mut failed,
        b"setsockopt(IP_RECVTTL) is accepted",
        lin::setsockopt_int(fd, 0 /* SOL_IP */, 12 /* IP_RECVTTL */, 1),
        0,
    );

    // Three echo requests in the shape Linux's raw(4) asks for without
    // `IP_HDRINCL`: the ICMP message alone, kernel-built IP header. The
    // answers arrive the other way round - header first, as on Linux's
    // receive side - and it is that header's ttl octet, and its agreement
    // with the `IP_RECVTTL` control message, that this test is about.
    for seq in 1i32..=3 {
        let mut pkt = [0u8; 64];
        pkt[0] = 8; // echo request
        let id: u16 = 0xBEEF;
        pkt[4] = (id >> 8) as u8;
        pkt[5] = id as u8;
        pkt[6] = (seq >> 8) as u8;
        pkt[7] = seq as u8;
        for i in 8..28 {
            pkt[i] = b'0' + ((i - 8) as u8 % 10);
        }
        let cs = icmp_cksum(&pkt[..28]);
        pkt[2] = (cs >> 8) as u8;
        pkt[3] = cs as u8;
        let t0 = ms();
        let sent = lin::sendto(fd, &pkt[..28], 0, Some(&PEER));
        if sent != 28 {
            lin::puts(b"FAIL: echo request did not leave: ");
            lin::puts_isize(sent);
            lin::puts(b"\n");
            failed = true;
            continue;
        }

        let mut buf = [0u8; 96];
        let mut name: lin::SockAddrIn = unsafe { core::mem::zeroed() };
        let mut ctl = [0u8; 64];
        let mut iov = lin::IoVec {
            base: buf.as_mut_ptr() as usize,
            len: buf.len(),
        };
        let mut hdr = lin::MsgHdr {
            msg_name: &mut name as *mut _ as usize,
            msg_namelen: 16,
            _pad: 0,
            msg_iov: &mut iov as *mut _ as usize,
            msg_iovlen: 1,
            msg_control: ctl.as_mut_ptr() as usize,
            msg_controllen: ctl.len(),
            msg_flags: 0,
        };
        let n = lin::recvmsg(fd, &mut hdr, 0);
        let rtt = ms().saturating_sub(t0);
        if n < 0 {
            lin::puts(b"FAIL: no echo answer: ");
            lin::puts_isize(n);
            lin::puts(b"\n");
            failed = true;
            continue;
        }
        lin::puts(b"seq ");
        lin::puts_isize(seq as isize);
        lin::puts(b": rtt=");
        lin::puts_isize(rtt as isize);
        lin::puts(b"ms n=");
        lin::puts_isize(n);
        lin::puts(b" ver_ihl=");
        lin::puts_isize(buf[0] as isize);
        lin::puts(b" ttl@8=");
        lin::puts_isize(buf[8] as isize);
        lin::puts(b" proto@9=");
        lin::puts_isize(buf[9] as isize);
        lin::puts(b" icmp@20=");
        lin::puts_isize(buf[20] as isize);
        lin::puts(b" echo@36=");
        lin::puts_isize(buf[36] as isize);
        lin::puts(b" cmsg_len=");
        lin::puts_isize(u64::from_ne_bytes(ctl[0..8].try_into().unwrap()) as isize);
        lin::puts(b" cmsg_ttl=");
        lin::puts_isize(cmsg_int(&ctl) as isize);
        lin::puts(b" peer=");
        lin::puts_isize(name.addr[0] as isize);
        lin::puts(b".");
        lin::puts_isize(name.addr[1] as isize);
        lin::puts(b".");
        lin::puts_isize(name.addr[2] as isize);
        lin::puts(b".");
        lin::puts_isize(name.addr[3] as isize);
        lin::puts(b"\n");

        // The bar: a complete IPv4 echo-reply packet - header first, the
        // reply's ICMP type at offset 20 with our id echoed behind it, and
        // the request's data pattern beginning the echo at 28 (the peer
        // answers with twenty echoed octets; that is what arrives, and the
        // offsets are what busybox's `ping` parses its ttl from).
        if buf[0] >> 4 != 4 || buf[9] != 1 || buf[20] != 0 || buf[24] != 0xBE || buf[28] != 48 {
            lin::puts(b"FAIL: not a delivered IPv4 echo-reply packet\n");
            failed = true;
        }
        if name.addr != [110, 242, 74, 102] {
            lin::puts(b"FAIL: msg_name names a different peer\n");
            failed = true;
        }
        let clen = u64::from_ne_bytes(ctl[0..8].try_into().unwrap());
        if clen == 24 && cmsg_int(&ctl) as u32 != buf[8] as u32 {
            lin::puts(b"FAIL: cmsg ttl disagrees with the header octet\n");
            failed = true;
        }
        if !(20..=255).contains(&(buf[8] as i32)) {
            lin::puts(b"FAIL: the delivered ttl octet is zero\n");
            failed = true;
        }
    }

    lin::close(fd);
    if failed {
        lin::puts(b"FAILED\n");
        lin::exit(1);
    }
    lin::puts(b"PASS\n");
    lin::exit(0)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-rawicmp: panicked\n")
}
