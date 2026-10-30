//! t-dgram-size - a datagram's size line is **Linux's 65507**, and the line
//! above it is the link's own MTU rather than the size of the buffer that would
//! hold the datagram.
//!
//! A datagram socket's `EMSGSIZE` is the one answer a caller cannot work
//! around: a program that sends a 65300-byte telemetry frame and hears "message
//! too long" has no fallback, because Linux sends that datagram. This kernel
//! used to answer `EMSGSIZE` for anything wider than its own 16 KiB datagram
//! ring (`src/net/mod.rs`'s `UDP_PAYLOAD_BYTES`) - a limit the caller could read
//! in no header, and about which Linux's man page says the opposite: the
//! maximum is 65507 bytes (65535 less the 20-byte IPv4 and 8-byte UDP headers),
//! and a socket's `SO_SNDBUF` does not bound a single datagram at all.
//!
//! Measured on a current host first (Linux 7.2.11, `.workbuddy/probe_dgram*.c`):
//!
//! * on the loopback, `sendto` of **65507** bytes returns 65507 and the
//!   receiver reads 65507 back, while **65508** and 70000 answer `EMSGSIZE`;
//! * a socket whose `SO_SNDBUF` reads back 9216 still sends the 65507;
//! * the same 65507 bytes go through a **1500-byte wire** because Linux
//!   fragments them; with `IP_MTU_DISCOVER=IP_PMTUDISC_DO` (fragment nothing)
//!   the same send answers `EMSGSIZE`, and so does a 2000-byte one - so a
//!   link's MTU refuses with `EMSGSIZE`, never with truncation or silence;
//! * in a netns with `ip link set lo mtu 1500`, the 65507 bytes still arrive
//!   whole (Linux fragments on the way out and reassembles on the way in).
//!
//! # What this program pins
//!
//! The size line is exact: 65507 out and back on the loopback (the bytes
//! verified at both ends), 65508 and 70000 `EMSGSIZE`, a small send buffer
//! still sending the maximum, and `lo`'s MTU readable and settable through
//! `SIOCGIFMTU`/`SIOCSIFMTU` (the shape `ip link set lo mtu` expands to).
//!
//! This kernel has **no IP fragmentation**, so the second line is the link's:
//! with `lo`'s MTU lowered to 1500 the 65507-byte datagram is `EMSGSIZE` where
//! Linux fragments and reassembles, and on `eth0` (MTU 1500) the same datagram
//! to the gateway is `EMSGSIZE` while a 1400-byte one leaves. That is exactly
//! Linux's answer in the shape that forbids fragmentation - the divergence is
//! Linux's *default* (it fragments), and `docs/notes/` §4.6 names the gap and
//! the measured reason it is still a gap.
//!
//! # What is *not* here
//!
//! IP fragmentation itself: smoltcp's fragmenter keeps the whole datagram in a
//! 64 KiB array **inside** `Interface`, and this kernel builds an `Interface` on
//! 32-64 KiB task stacks - measured, and the reason §4.6 keeps the gap rather
//! than a corruption bug (which is what enabling it produced: three user-mode
//! faults across two lanes). `IP_MTU_DISCOVER`/`IP_MTU` are not served either
//! (named in `docs/uapi/` §5.1); the path-MTU rule a caller could ask for is
//! therefore not pinned here. A received datagram larger than the socket's
//! receive ring is still dropped - Linux keeps it if the ring can hold it, and
//! the ring this kernel can be asked for tops out at 64 KiB (`UDP_RX_MAX`),
//! which is what lets the 65507 claim above work at all.

use mpos_user_tests::{Report, host_run};
use rustix::io::Errno;
use rustix::net::sockopt::{
    set_socket_recv_buffer_size, set_socket_send_buffer_size, socket_send_buffer_size,
};
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketAddrV4, SocketType};
use rustix::net::{bind, getsockname, recvfrom, sendto, socket};
use std::net::Ipv4Addr;

/// The largest UDP payload over IPv4: 65535 bytes less the 8-byte UDP and
/// 20-byte IPv4 headers.
const MAX_DGRAM: usize = 65507;
/// One byte past it, and a comfortably past-it size, both from the host
/// measurement.
const TOO_BIG: usize = 65508;
const WAY_TOO_BIG: usize = 70000;

/// The loopback - the one address a datagram can be sent to and answered inside
/// this machine.
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// QEMU's user-mode gateway: the one address a guest can be sure a frame leaves
/// `eth0` for. Port 9 (discard) is not served there, so what comes back is at
/// worst an ICMP refusal - the send's own answer is what the claims read.
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// The wire's MTU, from `assets/configs/machine.toml`'s machine (and the stack's
/// `MTU`). It is the line the 65507-byte datagram crosses on its way to the
/// gateway, which is why a send wider than it cannot be served.
const WIRE_MTU: i32 = 1500;

/// A loopback receiver and a sender aimed at it, both fresh. The receiver's
/// `SO_RCVBUF` asks for Linux's default-sized queue, because a 65507-byte
/// datagram has to fit the ring before it can be read.
fn pair() -> Option<(std::os::fd::OwnedFd, std::os::fd::OwnedFd, u16)> {
    let rx = socket(AddressFamily::INET, SocketType::DGRAM, None).ok()?;
    set_socket_recv_buffer_size(&rx, 212992).ok()?;
    bind(&rx, &SocketAddrV4::new(LOOPBACK, 0)).ok()?;
    let port = SocketAddrV4::try_from(getsockname(&rx).ok()?).ok()?.port();
    let tx = socket(AddressFamily::INET, SocketType::DGRAM, None).ok()?;
    Some((rx, tx, port))
}

/// Wait for one datagram up to `want` bytes, for as long as the program's own
/// budget allows, and answer what arrived (and whether every byte matches).
fn receive(rx: &std::os::fd::OwnedFd, want: &[u8], tries: u32) -> Option<Vec<u8>> {
    let mut got = vec![0u8; want.len() + 4096];
    for _ in 0..tries {
        match recvfrom(rx, &mut got, RecvFlags::DONTWAIT) {
            Ok((n, _truncated, _from)) => {
                got.truncate(n);
                return Some(got);
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
    None
}

/// `SIOCGIFMTU` and `SIOCSIFMTU` - the classic `ifconfig mtu` pair, and what
/// `ip link set ... mtu` reaches through libc. Neither has a rustix wrapper
/// (clause (c): the shape is an `ifreq`, a name at offset 0 and the value at
/// offset 16, the shape `src/syscall/linux/fs/ioctl.rs` reads). Answers the
/// value the request read back.
fn mtu_ioctl(request: libc::c_ulong, name: &str, value: i32) -> Result<i32, i32> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let mut req = [0u8; 40];
    let bytes = name.as_bytes();
    let n = bytes.len().min(15);
    req[..n].copy_from_slice(&bytes[..n]);
    req[16..20].copy_from_slice(&value.to_ne_bytes());
    let rc = unsafe { libc::ioctl(fd, request as _, &mut req as *mut [u8; 40]) };
    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    unsafe { libc::close(fd) };
    if rc < 0 {
        return Err(err);
    }
    Ok(i32::from_ne_bytes([req[16], req[17], req[18], req[19]]))
}

fn get_mtu(name: &str) -> Result<i32, i32> {
    mtu_ioctl(0x8921, name, 0)
}

fn set_mtu(name: &str, mtu: i32) -> Result<i32, i32> {
    mtu_ioctl(0x8922, name, mtu)
}

fn main() {
    let mut r = Report::new();
    let on_host = host_run();

    // ---- the loopback: the size line itself -----------------------------
    let Some((rx, tx, port)) = pair() else {
        r.check("a loopback datagram pair is created", false);
        r.finish();
    };
    let to_rx = SocketAddrV4::new(LOOPBACK, port);
    let mut payload = vec![0u8; MAX_DGRAM];
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte = b'a' + (i % 26) as u8;
    }

    match sendto(&tx, &payload, SendFlags::empty(), &to_rx) {
        Ok(n) => r.check(
            &format!("a 65507-byte datagram is accepted (sent {n})"),
            n == MAX_DGRAM,
        ),
        Err(err) => r.check(
            &format!("a 65507-byte datagram is accepted (got {err})"),
            false,
        ),
    }

    let whole = receive(&rx, &payload, 400);
    r.check(
        &format!(
            "it arrives whole and unchanged (got {:?} bytes)",
            whole.as_ref().map(Vec::len)
        ),
        whole.as_deref() == Some(payload.as_slice()),
    );

    // The sender's own buffer is not the line: Linux sends the maximum from a
    // socket reading 9216, and this kernel's floor is 4608.
    let (rx2, tx2, port2) = match pair() {
        Some(p) => p,
        None => {
            r.check("a second loopback datagram pair is created", false);
            r.finish();
        }
    };
    if set_socket_send_buffer_size(&tx2, 4608).is_err() {
        r.check("the send buffer is asked to be small", false);
        r.finish();
    }
    let read_back: usize = socket_send_buffer_size(&tx2).unwrap_or(0);
    match sendto(
        &tx2,
        &payload,
        SendFlags::empty(),
        &SocketAddrV4::new(LOOPBACK, port2),
    ) {
        Ok(n) => r.check(
            &format!("a small send buffer (reads {read_back}) still sends it (sent {n})"),
            n == MAX_DGRAM,
        ),
        Err(err) => r.check(
            &format!("a small send buffer still sends it (got {err})"),
            false,
        ),
    }
    drop((rx2, tx2));

    // The refusals: one byte lines, both of them `EMSGSIZE`, not `EAGAIN` and
    // not a truncation.
    for size in [TOO_BIG, WAY_TOO_BIG] {
        let one = vec![b'x'; size];
        let answer = sendto(&tx, &one, SendFlags::empty(), &to_rx);
        r.check(
            &format!("{size} bytes is EMSGSIZE (got {answer:?})"),
            matches!(answer, Err(e) if e == Errno::MSGSIZE),
        );
    }

    // The socket still works after a refusal, and after the maximum.
    let mut small = [0u8; 5];
    let small_ok = sendto(&tx, b"12345", SendFlags::empty(), &to_rx).is_ok_and(|n| n == 5)
        && (0..200).any(|_| match recvfrom(&rx, &mut small, RecvFlags::DONTWAIT) {
            Ok((n, _, _)) => n == 5 && &small == b"12345",
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                false
            }
        });
    r.check("a small datagram still goes through afterwards", small_ok);

    // ---- lo's own MTU: the second line, and the one Linux makes visible ----
    //
    // Linux's instrument for the MTU line is `ip link set lo mtu 1500`, which is
    // `SIOCSIFMTU` underneath. Measured in a netns, the 65507-byte datagram then
    // still arrives whole, because Linux fragments it. This kernel cannot, so
    // the same send is `EMSGSIZE` - the answer Linux gives the same datagram
    // when a caller sets `IP_PMTUDISC_DO` - and restoring 65536 makes it work
    // again. Both halves are the measurement §4.6 records.
    let default_lo = get_mtu("lo");
    r.check(
        &format!("lo's MTU is Linux's 65536 (got {default_lo:?})"),
        default_lo == Ok(65536),
    );
    if on_host {
        r.check(
            "changing lo's MTU is the machine's own privileged call - measured in a netns instead",
            true,
        );
    } else {
        let set = set_mtu("lo", 1500);
        r.check(
            &format!("lo's MTU is set to 1500 (got {set:?})"),
            set == Ok(1500),
        );
        match pair() {
            None => r.check("a loopback pair for the MTU line is created", false),
            Some((rx3, tx3, port3)) => {
                let to3 = SocketAddrV4::new(LOOPBACK, port3);
                let sent3 = sendto(&tx3, &payload, SendFlags::empty(), &to3);
                r.check(
                    &format!(
                        "at lo's 1500 a 65507-byte datagram is EMSGSIZE, the answer Linux gives \
                         when it may not fragment (got {sent3:?})"
                    ),
                    matches!(sent3, Err(e) if e == Errno::MSGSIZE),
                );
                drop((rx3, tx3));
            }
        }
        let eth_mtu = get_mtu("eth0");
        r.check(
            &format!("setting lo's MTU leaves eth0's alone (got {eth_mtu:?})"),
            eth_mtu == Ok(WIRE_MTU),
        );
        let restore = set_mtu("lo", 65536);
        r.check(
            &format!("lo's MTU is restored to 65536 (got {restore:?})"),
            restore == Ok(65536),
        );
        match pair() {
            None => r.check("a loopback pair after the restore is created", false),
            Some((rx4, tx4, port4)) => {
                let sent4 = sendto(
                    &tx4,
                    &payload,
                    SendFlags::empty(),
                    &SocketAddrV4::new(LOOPBACK, port4),
                );
                r.check(
                    &format!("and the maximum datagram is accepted again (sent {sent4:?})"),
                    sent4.is_ok_and(|n| n == MAX_DGRAM),
                );
                let back = receive(&rx4, &payload, 400);
                r.check(
                    "it comes back whole after the MTU is restored",
                    back.as_deref() == Some(payload.as_slice()),
                );
                drop((rx4, tx4));
            }
        }
    }

    // ---- the wire: a datagram wider than the link ------------------------
    if on_host {
        r.check(
            "the wire half is measured on the payload machine, not on this host",
            true,
        );
    } else {
        let wide = vec![b'y'; MAX_DGRAM];
        let sent = sendto(
            &tx,
            &wide,
            SendFlags::empty(),
            &SocketAddrV4::new(GATEWAY, 9),
        );
        r.check(
            &format!(
                "a 65507-byte datagram to the 1500-byte wire is EMSGSIZE, the answer Linux \
                 gives when it may not fragment (got {sent:?})"
            ),
            matches!(sent, Err(e) if e == Errno::MSGSIZE),
        );
        let sent = sendto(
            &tx,
            &[b'z'; 1400],
            SendFlags::empty(),
            &SocketAddrV4::new(GATEWAY, 9),
        );
        r.check(
            &format!("a 1400-byte datagram still leaves for the wire (got {sent:?})"),
            sent.is_ok_and(|n| n == 1400),
        );
    }

    r.finish();
}
