//! `AF_PACKET`: the link layer as a socket, so that userland owns what the
//! wire actually carries.
//!
//! Linux's `PF_PACKET` is how a program speaks Ethernet directly - the way
//! `udhcpc` gets a DHCP lease before the machine has an address, the way a
//!arp resolver and a bridge work at all. It is the *mechanism* the kernel
//! offers, and the policy - which frames matter, what to answer them with -
//! stays with the caller. This kernel used to refuse the family outright
//! (`EAFNOSUPPORT`), which left the address configuration to a boot-time
//! default nobody in userland could re-negotiate: the DHCP-shaped hole
//! recorded in `PLAN.md`.
//!
//! What is implemented here is the honest subset a DHCP client needs, with
//! the refusals Linux would give beyond it:
//!
//! * `SOCK_DGRAM` only. `SOCK_RAW` (frame *with* its Ethernet header) is
//!   `EOPNOTSUPP` - the caller would have to invent a source MAC the device
//!   does not own, and a header written by the caller's hope rather than by
//!   the stack's fact is the approximation this kernel's rules put above
//!   absence.
//! * The receive side taps every frame the NIC hands the stack at the seam
//!   itself (`NicDevice::receive`), filtering by the Ethernet type the
//!   socket was created with - `htons(0)` sees everything, as `ETH_P_ALL`
//!   means. The payload travels without its header, which is what
//!   `PACKET_SOCK_DGRAM` means on Linux: the header is the kernel's to add.
//! * The send side takes a `sockaddr_ll` destination per call and builds
//!   the header from the device's real MAC and the socket's protocol number.
//! * One interface, `eth0`: `bind` records the index it is given and every
//!   index is that one's, which is the machine's truth rather than a
//!   narrowing of it. A `PACKET_FANOUT`, a ring (`PACKET_TXRING`) or a
//!   filter (`SO_ATTACH_FILTER`) answers `EOPNOTSUPP` - not built, and the
//!   one door a caller can knock on to find that out.
//!
//! The queue is bounded (`RING_FRAMES`): Linux drops frames for a packet
//! socket whose ring is full and marks the loss (`PACKET_STATISTICS` would
//! report it); a socket here keeps the newest frames and loses the oldest,
//! which is the same shape of answer for a client that retransmits.

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::sched::policy::TaskId;
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// The family number, from Linux's `bits/socket.h`.
pub(crate) const AF_PACKET: u16 = 17;

/// Frames kept per socket before the oldest gives way. Two pages of DHCP
/// traffic by any measure; the number exists so a stopped reader cannot
/// make the kernel eat memory on its behalf.
const RING_FRAMES: usize = 64;
/// `sizeof(struct sockaddr_ll)`, per `linux/if_packet.h`: family 0, protocol
/// 2, ifindex 4, hatype 8, pkttype 10, halen 11, addr 12.
pub const SOCKADDR_LL_LEN: usize = 20;

/// One tapped frame: the payload half (the `SOCK_DGRAM` promise), and the
/// two header facts a `recvfrom` answer needs - the source MAC and the
/// Ethernet type, as their wire patterns.
type Frame = (Vec<u8>, [u8; 6], u16);

/// One open packet socket. The registry slot is its identity; the queue
/// holds payload halves of tapped frames, oldest first.
pub struct PacketSock {
    /// The Ethernet type this socket sees, as the caller's `htons` spelled
    /// it - compared byte-for-byte against the frame's own two type bytes,
    /// `0` (`ETH_P_ALL`) meaning everything.
    protocol: u16,
    /// The `sll_ifindex` a `bind` recorded. Recorded, not enforced: one
    /// interface exists, and see the module header for the claim.
    bound: Mutex<RefCell<i32>>,
    rx: Mutex<RefCell<VecDeque<Frame>>>,
    waiters: Mutex<RefCell<Vec<TaskId>>>,
}

impl PacketSock {
    fn new(protocol: u16) -> Arc<PacketSock> {
        Arc::new(PacketSock {
            protocol,
            bound: Mutex::new(RefCell::new(0)),
            rx: Mutex::new(RefCell::new(VecDeque::new())),
            waiters: Mutex::new(RefCell::new(Vec::new())),
        })
    }

    fn holds(&self) -> bool {
        critical_section::with(|cs| !self.rx.borrow(cs).borrow().is_empty())
    }

    /// Pop one queued frame: payload, source MAC, Ethernet type - parked
    /// until the tap hands one over, or until a catchable signal ends the
    /// wait. The signal's answer is `Interrupted` with nothing consumed, the
    /// same one every other blocking door here gives: a DHCP client's retry
    /// loop is built on it (`udhcpc` arms an `SIGALRM` and expects its blocked
    /// receive back), and a wait that slept through the alarm would hold the
    /// client until the reply it is waiting for happened to arrive.
    pub(crate) fn recv_blocking(&self) -> VfsResult<Frame> {
        match crate::sched::block_until_interruptible(|| self.holds()) {
            crate::sched::WaitEnd::Interrupted => return Err(VfsError::Interrupted),
            _ => {}
        }
        Ok(critical_section::with(|cs| {
            let mut q = self.rx.borrow(cs).borrow_mut();
            q.pop_front().unwrap_or_default()
        }))
    }

    pub(crate) fn try_recv(&self) -> Option<Frame> {
        critical_section::with(|cs| self.rx.borrow(cs).borrow_mut().pop_front())
    }

    /// `bind` on a packet socket is one-time on Linux - a second bind of a
    /// different index is `EBUSY` - and one interface exists here, so the
    /// record is kept and re-checks answer from it.
    pub(crate) fn set_bound(&self, index: i32) -> bool {
        critical_section::with(|cs| {
            let mut bound = self.bound.borrow(cs).borrow_mut();
            if *bound != 0 && *bound != index {
                return false;
            }
            *bound = index;
            true
        })
    }
}

impl Inode for PacketSock {
    fn seekable(&self) -> bool {
        false
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            ino: 0x7000_A000,
            kind: NodeKind::CharDev,
            size: 0,
        }
    }

    fn poll_ready(&self) -> Ready {
        Ready {
            read: self.holds(),
            write: true,
        }
    }

    fn poll_subscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            let mut w = self.waiters.borrow(cs).borrow_mut();
            if !w.contains(&id) {
                w.push(id);
            }
        });
    }

    fn poll_unsubscribe(&self, id: TaskId) {
        critical_section::with(|cs| {
            self.waiters.borrow(cs).borrow_mut().retain(|&w| w != id);
        });
    }

    /// `read(2)` on the socket: the next frame's payload. A short buffer
    /// truncates, which is what a packet socket's read means on Linux too -
    /// and a signal that ends the wait is `EINTR`, the same answer the
    /// `recvfrom` door above gives for the same wait.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let (frame, _, _) = self.recv_blocking()?;
        let n = frame.len().min(buf.len());
        buf[..n].copy_from_slice(&frame[..n]);
        Ok(n)
    }

    /// `write(2)` has no destination to aim at; `sendto(2)` is the door.
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::Unsupported)
    }
}

/// The live sockets, so the tap can hand a frame to each that wants it.
/// Weak, because the last close is the retirement and nothing else.
static SOCKETS: Mutex<RefCell<Vec<Weak<PacketSock>>>> = Mutex::new(RefCell::new(Vec::new()));

/// Open a packet socket: `socket(AF_PACKET, SOCK_DGRAM, protocol)`. The
/// protocol arrives in *network* order, as every caller spells it; the
/// filter compares it against the frame's own two bytes, also network order,
/// so the comparison is of the two numbers as they sit on the wire.
pub(crate) fn open(protocol_be: u16) -> Arc<PacketSock> {
    let sock = PacketSock::new(protocol_be);
    critical_section::with(|cs| {
        let mut v = SOCKETS.borrow(cs).borrow_mut();
        if let Some(dead) = v.iter_mut().find(|w| w.weak_count() == 0) {
            *dead = Arc::downgrade(&sock);
        } else {
            v.push(Arc::downgrade(&sock));
        }
    });
    sock
}

/// The last close: take the socket out of the tap's reach. (Weak entries
/// clean themselves at delivery; this keeps the list from carrying corpses
/// between one delivery and the next.)
pub(crate) fn retire_for(sock: &PacketSock) {
    critical_section::with(|cs| {
        let mut v = SOCKETS.borrow(cs).borrow_mut();
        let target = sock as *const PacketSock as usize;
        v.retain(|w| w.as_ptr() as usize != target);
    });
}

/// The tap. Called from the NIC seam with one complete Ethernet frame,
/// before the stack has said whether the frame is any of *its* business -
/// which is exactly the point: a DHCP reply for a machine with no address
/// is nobody's business in the IP sense, and every business there is in
/// the link sense.
pub(crate) fn deliver(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    // The two bytes *as a byte pattern*: the socket's `htons` number is the
    // same pattern, and comparing the two without a host-order detour is
    // what makes `htons(ETH_P_IP)` match the `08 00` on the wire.
    let ethertype = u16::from_le_bytes([frame[12], frame[13]]);
    let payload = &frame[14..];
    let src: [u8; 6] = frame[6..12].try_into().unwrap_or([0; 6]);
    let woke: Vec<TaskId> = critical_section::with(|cs| {
        let mut woke = Vec::new();
        let mut v = SOCKETS.borrow(cs).borrow_mut();
        v.retain(|w| w.strong_count() > 0);
        for weak in v.iter() {
            let Some(sock) = weak.upgrade() else {
                continue;
            };
            if sock.protocol != 0 && sock.protocol != ethertype {
                continue;
            }
            let mut q = sock.rx.borrow(cs).borrow_mut();
            q.push_back((payload.to_vec(), src, ethertype));
            while q.len() > RING_FRAMES {
                q.pop_front();
            }
            drop(q);
            woke.extend(sock.waiters.borrow(cs).borrow().iter().copied());
        }
        woke
    });
    for id in woke {
        crate::sched::wake(id);
    }
}

/// `sendto` on the socket: build the header the `SOCK_DGRAM` promise says
/// the kernel owns - destination from the caller's `sockaddr_ll`, source
/// the device's real MAC, type the socket's own protocol - and hand the
/// frame to the device. `Ok(())` means the frame left; a full queue is the
/// device's `Err`, which the caller reads as `EIO`-worthy refusal.
pub(crate) fn send(sock: &PacketSock, dst_mac: [u8; 6], payload: &[u8]) -> Result<usize, isize> {
    let mac = crate::net::packet_mac();
    let Some(mac) = mac else {
        return Err(crate::err::ENETDOWN);
    };
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&mac);
    let ethertype = if sock.protocol == 0 {
        // `ETH_P_ALL` cannot be a frame's type; an untyped sender means the
        // payload's own kind, and a DHCP client is a `htons(ETH_P_IP)` one.
        // Written as the wire's `08 00` - the pattern, in memory order.
        0x0800u16.to_be()
    } else {
        sock.protocol
    };
    frame.extend_from_slice(&ethertype.to_le_bytes());
    frame.extend_from_slice(payload);
    crate::net::packet_transmit(&frame)?;
    Ok(payload.len())
}

/// Read a `sockaddr_ll` out of the caller's buffer: the destination MAC
/// (first six address bytes) and, for `bind`, the interface index.
pub(crate) struct SockaddrLl {
    pub ifindex: i32,
    pub hatype: u16,
    pub halen: u8,
    pub addr: [u8; 8],
}

/// Parse the caller's `sockaddr_ll`, reading the `linux/if_packet.h` offsets.
/// `None` on a family mismatch or a short struct - Linux's `EINVAL`, which
/// the caller turns into the return.
pub(crate) fn parse_sll(raw: &[u8]) -> Option<SockaddrLl> {
    if raw.len() < SOCKADDR_LL_LEN {
        return None;
    }
    if u16::from_ne_bytes([raw[0], raw[1]]) != AF_PACKET {
        return None;
    }
    let mut addr = [0u8; 8];
    addr.copy_from_slice(&raw[12..20]);
    Some(SockaddrLl {
        ifindex: i32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]),
        hatype: u16::from_ne_bytes([raw[8], raw[9]]),
        halen: raw[11],
        addr,
    })
}

/// Write one `sockaddr_ll` into the caller's buffer: family, the socket's
/// `htons` protocol, `eth0`'s index, `ARPHRD_ETHER` and the given address -
/// the device's own MAC for `getsockname`, a frame's source for `recvfrom`.
pub(crate) fn write_sll(protocol: u16, addr: &[u8; 6], buf: usize) {
    unsafe {
        let base = buf as *mut u8;
        core::ptr::write_bytes(base, 0, SOCKADDR_LL_LEN);
        let at = |offset: usize| (buf + offset) as *mut u8;
        (at(0) as *mut u16).write_unaligned(AF_PACKET);
        (at(2) as *mut u16).write_unaligned(protocol);
        (at(4) as *mut i32).write_unaligned(2);
        (at(8) as *mut u16).write_unaligned(1); // ARPHRD_ETHER
        at(10).write_unaligned(0u8); // sll_pkttype: unclassified here
        at(11).write_unaligned(6u8); // sll_halen
        for (i, byte) in addr.iter().enumerate() {
            at(12 + i).write_unaligned(*byte);
        }
    }
}

/// Parse a caller-supplied `sockaddr_ll`. `EINVAL` is Linux's answer for a
/// short or wrong-family address.
pub(crate) fn parse_sll_from_user(
    ptr: usize,
    len: usize,
    from_user: bool,
) -> Result<SockaddrLl, isize> {
    use crate::err;
    if len < SOCKADDR_LL_LEN || !crate::uaccess::buffer_ok(ptr, len, false, from_user) {
        return Err(err::EINVAL);
    }
    let mut raw = [0u8; SOCKADDR_LL_LEN];
    unsafe {
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = (ptr as *const u8).add(i).read_unaligned();
        }
    }
    parse_sll(&raw).ok_or(err::EINVAL)
}

/// `getsockname` on the socket: its `htons` protocol, eth0's index, and the
/// device's own MAC.
pub(crate) fn sockname(sock: &PacketSock, addr: usize, len_ptr: usize) -> isize {
    use crate::err;
    if addr == 0
        || len_ptr == 0
        || !crate::uaccess::buffer_ok(addr, SOCKADDR_LL_LEN, true, true)
        || !crate::uaccess::buffer_ok(len_ptr, 4, true, true)
    {
        return err::EFAULT;
    }
    let mac = crate::net::packet_mac().unwrap_or([0u8; 6]);
    write_sll(sock.protocol, &mac, addr);
    unsafe {
        (len_ptr as *mut u32).write_unaligned(SOCKADDR_LL_LEN as u32);
    }
    0
}
