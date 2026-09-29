//! t-dhcp - the link layer as a socket: an `AF_PACKET` client gets a DHCP
//! answer with the machine still having no address of its own.
//!
//! This is the *mechanism* half of the plan recorded in `PLAN.md`: the
//! kernel stopped being the one that decides the machine's address, and
//! started offering the door through which a userland client can ask for
//! one. The test plays that client by hand - a DHCPDISCOVER built octet by
//! octet, sent to the broadcast through `sendto(2)` with a `sockaddr_ll`,
//! and the answer read back off the same socket - because the day's promise
//! is only that the door exists and answers truthfully, not that a shipping
//! `udhcpc` is wired into the boot (that gap stays named where it is).
//!
//! The QEMU user-mode network carries a BOOTP server by design - it is what
//! hands out the 10.0.2.15 this machine's static boot default used to
//! restate - so the answer here comes from the host's slirp, and its shape
//! is the host's promise: a BOOTREPLY carrying our transaction id.
#![no_std]
#![no_main]

#[path = "../../../lib/lin.rs"]
mod lin;

use core::panic::PanicInfo;

const AF_PACKET: usize = 17;
const SOCK_DGRAM: usize = 2;
/// `htons(ETH_P_IP)`: the caller's protocol number travels as the pattern
/// the wire's two type bytes spell, and the kernel compares patterns.
const ETH_P_IP: usize = 0x0008;
const SOCKADDR_LL_LEN: usize = 20;

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

#[unsafe(no_mangle)]
extern "C" fn mpos_user_main(_argc: usize, _argv: *const *const u8) -> ! {
    lin::puts(b"t-dhcp\n");
    let mut failed = false;

    // --- the socket exists, and names itself like a link ---
    let sock = lin::raw_sys(lin::nr::SOCKET, AF_PACKET, SOCK_DGRAM, ETH_P_IP);
    lin::check_true(
        &mut failed,
        b"socket(AF_PACKET, SOCK_DGRAM) opens",
        sock >= 0,
    );
    let sock = sock as i32;

    let mut name = [0u8; SOCKADDR_LL_LEN];
    let mut nlen: u32 = SOCKADDR_LL_LEN as u32;
    let r = lin::raw_sys(
        lin::nr::GETSOCKNAME,
        sock as usize,
        &raw mut name as usize,
        &raw mut nlen as usize,
    );
    lin::check_eq(&mut failed, b"getsockname answers", r, 0);
    lin::check_eq(
        &mut failed,
        b"the name is a sockaddr_ll",
        u16::from_ne_bytes([name[0], name[1]]) as isize,
        AF_PACKET as isize,
    );
    let mac = [name[12], name[13], name[14], name[15], name[16], name[17]];
    lin::check_true(
        &mut failed,
        b"and its address is the device's real MAC",
        mac != [0u8; 6],
    );

    // --- bind: eth0's index is the only one there is ---
    let mut sll = [0u8; SOCKADDR_LL_LEN];
    sll[0..2].copy_from_slice(&(AF_PACKET as u16).to_ne_bytes());
    sll[2..4].copy_from_slice(&(ETH_P_IP as u16).to_ne_bytes());
    sll[4..8].copy_from_slice(&2i32.to_ne_bytes());
    sll[8..10].copy_from_slice(&1u16.to_ne_bytes()); // ARPHRD_ETHER
    sll[11] = 6;
    let r = lin::raw_sys(
        lin::nr::BIND,
        sock as usize,
        &raw mut sll as usize,
        SOCKADDR_LL_LEN,
    );
    lin::check_eq(&mut failed, b"bind to eth0's index succeeds", r, 0);

    // --- a DISCOVER, octet by octet: 20 IP + 8 UDP + 264 BOOTP+options ---
    const TOTAL: usize = 292;
    let mut pkt = [0u8; TOTAL];
    // IPv4 header
    pkt[0] = 0x45;
    pkt[2..4].copy_from_slice(&(TOTAL as u16).to_be_bytes());
    pkt[8] = 64; // ttl
    pkt[9] = 17; // UDP
    // src 0.0.0.0 (as an addressless client sends), dst 255.255.255.255
    pkt[20..24].copy_from_slice(&[255, 255, 255, 255]);
    let ip_csum = cksum(&pkt[0..20]).to_be_bytes();
    pkt[10..12].copy_from_slice(&ip_csum);
    // UDP: bootp client port 68 to server port 67; a zero checksum is legal
    // for IPv4 UDP and what the real clients send.
    pkt[20 + 0..20 + 2].copy_from_slice(&68u16.to_be_bytes());
    pkt[20 + 2..20 + 4].copy_from_slice(&67u16.to_be_bytes());
    pkt[20 + 4..20 + 6].copy_from_slice(&((TOTAL - 20) as u16).to_be_bytes());
    // BOOTP request, starting at the UDP payload (offset 28)
    pkt[28] = 1; // op = BOOTREQUEST
    pkt[29] = 1; // htype = Ethernet
    pkt[30] = 6; // hlen
    pkt[32..36].copy_from_slice(&0x4d504f53u32.to_be_bytes()); // xid
    pkt[38..40].copy_from_slice(&0x8000u16.to_be_bytes()); // broadcast flag
    pkt[56..62].copy_from_slice(&mac); // chaddr
    pkt[236..240].copy_from_slice(&[99, 130, 83, 99]); // magic cookie
    // options: message-type DISCOVER, client-id, end.
    pkt[240] = 53;
    pkt[241] = 1;
    pkt[242] = 1;
    pkt[243] = 61;
    pkt[244] = 7;
    pkt[245] = 1;
    pkt[246..252].copy_from_slice(&mac);
    pkt[252] = 255;

    // The destination: the broadcast. With `halen` left zero the kernel
    // reads "no link address named", which for `SOCK_DGRAM` means the
    // all-ones address - and the frame header itself is the kernel's to
    // build from the device's own MAC.
    let mut dst = [0u8; SOCKADDR_LL_LEN];
    dst[0..2].copy_from_slice(&(AF_PACKET as u16).to_ne_bytes());
    dst[2..4].copy_from_slice(&(ETH_P_IP as u16).to_ne_bytes());
    dst[4..8].copy_from_slice(&2i32.to_ne_bytes());
    let sent = lin::raw_sys6(
        lin::nr::SENDTO,
        sock as usize,
        &raw const pkt as usize,
        TOTAL,
        0,
        &raw const dst as usize,
        SOCKADDR_LL_LEN,
    );
    lin::check_eq(
        &mut failed,
        b"the DISCOVER goes out whole",
        sent,
        TOTAL as isize,
    );

    // --- the answer: the host's BOOTP server replies, and we read it back ---
    let mut rfds = [lin::PollFd {
        fd: sock,
        events: 1, // POLLIN
        revents: 0,
    }];
    let ready = lin::poll(&mut rfds, 5000);
    lin::check_true(
        &mut failed,
        b"an answer arrives within five seconds",
        ready > 0,
    );
    if ready > 0 {
        let mut rx = [0u8; 600];
        let cap = rx.len();
        let got = lin::raw_sys5(
            lin::nr::RECVFROM,
            sock as usize,
            &raw mut rx as usize,
            cap,
            0,
            0,
        );
        lin::check_true(
            &mut failed,
            b"the reply reads back whole",
            got >= TOTAL as isize,
        );
        if got >= 292 {
            lin::check_eq(&mut failed, b"the reply is a BOOTREPLY", rx[28] as isize, 2);
            let xid = u32::from_be_bytes([rx[32], rx[33], rx[34], rx[35]]);
            lin::check_eq(
                &mut failed,
                b"and it carries our transaction id",
                xid as isize,
                0x4d504f53u32 as isize,
            );
        }
    }

    let _ = lin::close(sock);
    lin::done(failed)
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    lin::die(b"t-dhcp: panicked\n")
}
