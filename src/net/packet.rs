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
//! recorded in `NOTES.md`.
//!
//! What is implemented here is the honest subset a DHCP client needs, with
//! the refusals Linux would give beyond it:
//!
//! * Both `AF_PACKET` shapes. `SOCK_DGRAM` (payload in, header built by the
//!   kernel) is the one a DHCP client's *listen* socket takes; `SOCK_RAW`
//!   (frame in, frame out, header on both) is the one an ARP resolver or a
//!   bridge takes. A socket of any other type is `ESOCKTNOSUPPORT`, measured on
//!   Linux.
//! * `SOCK_RAW` sends put the caller's frame on the wire *unchanged*: the
//!   header is the caller's to build, so `sll_addr` is ignored and the frame's
//!   own header names the destination - measured on the host (the header's
//!   destination won over `sll_addr`'s when the two disagreed). The one check
//!   is that a header is there to send: shorter than `ETH_HEADER_LEN` is
//!   `EINVAL` (measured; 14 passes, 13 does not). A send with no `sockaddr_ll`
//!   at all is `ENXIO`, because the frame alone cannot name the interface.
//! * The receive side taps every frame the NIC hands the stack at the seam
//!   itself (`NicDevice::receive`), filtering by the Ethernet type the
//!   socket was created with - `htons(0)` sees everything, as `ETH_P_ALL`
//!   means. The frame is stored whole; a `SOCK_DGRAM` reader is handed the
//!   payload half (the header is the kernel's to add back on a send, so it is
//!   dropped), a `SOCK_RAW` reader the frame with its header on.
//! * The `SOCK_DGRAM` send side takes a `sockaddr_ll` destination per call and
//!   builds the header from the device's real MAC and the socket's protocol
//!   number.
//! * One interface, `eth0`: `bind` records the index it is given and every
//!   index is that one's, which is the machine's truth rather than a
//!   narrowing of it. A `PACKET_FANOUT`, a ring (`PACKET_TXRING`) or a
//!   filter (`SO_ATTACH_FILTER`) answers `EOPNOTSUPP` - not built, and the
//!   one door a caller can knock on to find that out.
//!
//! The wire format itself - `struct sockaddr_ll`'s decode and encode, the
//! Ethernet header, and the bounded frame ring - is not here. It is the
//! `mpos-link` leaf (`crate::link`), because this module is welded to the NIC
//! seam and therefore `target_os = "none"`, and a parser behind that gate can be
//! neither host-tested nor fuzzed - which is exactly how `sockaddr_ll` and the
//! header came to have no host pin while `AGENTS.md` §3 asks every wire parser
//! for one (`docs/notes/` §4.5). What stays here is the part that needs the
//! machine: the tap on the seam, the device's own MAC, and the user-memory
//! boundary.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;

use crate::link::{Frame, SockaddrLl};
use crate::sched::policy::TaskId;
use crate::vfs::{Inode, Metadata, NodeKind, Ready, VfsError, VfsResult};

/// eth0's interface index, the one interface this kernel serves. `bind` records
/// whatever index it is given and every index is this one's, which is the
/// machine's truth rather than a narrowing of it (the module header).
const ETH0_IFINDEX: i32 = 2;

/// One open packet socket. The registry slot is its identity; the queue holds
/// tapped frames, oldest first, bounded and keeping the newest
/// (`crate::link::Frames`). A frame is stored whole (header first); `raw` is
/// what the reader does with the first fourteen bytes.
pub struct PacketSock {
    /// The Ethernet type this socket sees, as the caller's `htons` spelled
    /// it - compared byte-for-byte against the frame's own two type bytes,
    /// `0` (`ETH_P_ALL`) meaning everything.
    protocol: u16,
    /// `true` for `SOCK_RAW`: receives hand back the whole frame (header on)
    /// and sends carry the caller's own header. `false` for `SOCK_DGRAM`, the
    /// payload shape. This is the one thing the socket's type changes.
    raw: bool,
    /// The `sll_ifindex` a `bind` recorded. Recorded, not enforced: one
    /// interface exists, and see the module header for the claim.
    bound: Mutex<RefCell<i32>>,
    rx: Mutex<RefCell<crate::link::Frames>>,
    waiters: Mutex<RefCell<Vec<TaskId>>>,
}

impl PacketSock {
    fn new(protocol: u16, raw: bool) -> Arc<PacketSock> {
        Arc::new(PacketSock {
            protocol,
            raw,
            bound: Mutex::new(RefCell::new(0)),
            rx: Mutex::new(RefCell::new(crate::link::Frames::new())),
            waiters: Mutex::new(RefCell::new(Vec::new())),
        })
    }

    /// `true` for `SOCK_RAW`. The send door reads it to decide whether the
    /// caller's bytes are a payload to wrap or a whole frame to pass through.
    pub(crate) fn is_raw(&self) -> bool {
        self.raw
    }

    /// The bytes a reader of this socket is promised, read out of one stored
    /// frame: the whole frame (header first) for a `SOCK_RAW` socket, the
    /// payload half for a `SOCK_DGRAM` one. The tap stores the whole frame for
    /// both, so the two shapes differ only in where this starts.
    pub(crate) fn received<'a>(&self, whole: &'a [u8]) -> &'a [u8] {
        if self.raw {
            whole
        } else {
            crate::link::frame_payload(whole)
        }
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
            q.pop().unwrap_or_default()
        }))
    }

    pub(crate) fn try_recv(&self) -> Option<Frame> {
        critical_section::with(|cs| self.rx.borrow(cs).borrow_mut().pop())
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

    /// `read(2)` on the socket: the next frame, as this socket's type promises
    /// it. A short buffer truncates, which is what a packet socket's read means
    /// on Linux too - and a signal that ends the wait is `EINTR`, the same
    /// answer the `recvfrom` door above gives for the same wait.
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let (whole, _, _) = self.recv_blocking()?;
        let frame = self.received(&whole);
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

/// Open a packet socket: `socket(AF_PACKET, typ, protocol)`. The protocol
/// arrives in *network* order, as every caller spells it; the filter compares
/// it against the frame's own two bytes, also network order, so the comparison
/// is of the two numbers as they sit on the wire. `raw` is `SOCK_RAW` rather
/// than `SOCK_DGRAM`, the only thing the two shapes differ in.
pub(crate) fn open(protocol_be: u16, raw: bool) -> Arc<PacketSock> {
    let sock = PacketSock::new(protocol_be, raw);
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
///
/// Splitting the header is `crate::link::parse_ethernet`'s job, and the
/// comparison it enables is of the two type numbers as they sit on the wire -
/// no host-order detour, which is the spelling a second copy of the constant
/// got wrong once (`docs/notes/` §4.5).
pub(crate) fn deliver(frame: &[u8]) {
    let Some(eth) = crate::link::parse_ethernet(frame) else {
        return;
    };
    let ethertype = eth.ethertype;
    let src = eth.src;
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
            // The whole frame is stored for both types; a `SOCK_RAW` reader
            // takes it as it sits, a `SOCK_DGRAM` reader drops the header
            // (`PacketSock::received`).
            sock.rx
                .borrow(cs)
                .borrow_mut()
                .push((frame.to_vec(), src, ethertype));
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
    let ethertype = if sock.protocol == 0 {
        // `ETH_P_ALL` cannot be a frame's type; an untyped sender means the
        // payload's own kind, and a DHCP client is a `htons(ETH_P_IP)` one.
        crate::link::IPV4_ETHERTYPE
    } else {
        sock.protocol
    };
    send_frame(dst_mac, ethertype, payload)
}

/// `sendto` on a `SOCK_RAW` socket: the caller built the whole frame, header
/// and all, so it goes to the device byte for byte. The kernel's one job is to
/// see that a header is actually there - a frame shorter than
/// [`crate::link::ETH_HEADER_LEN`] is `EINVAL`, measured on the host - and it
/// does *not* touch the header: `sll_addr` named no destination the frame did
/// not already carry (measured: the header won when the two disagreed), so
/// changing it here would be the kernel overruling the caller.
pub(crate) fn send_raw(frame: &[u8]) -> Result<usize, isize> {
    if frame.len() < crate::link::ETH_HEADER_LEN {
        return Err(crate::err::EINVAL);
    }
    crate::net::packet_transmit(frame)?;
    Ok(frame.len())
}

/// Put one frame on the wire: the header the kernel owns - destination as given,
/// source the device's real MAC - and then the payload.
///
/// The header is built by `crate::link::encode_ethernet` rather than by each
/// caller because there is more than one now: `send` above for a packet socket,
/// whose protocol the caller chose, and the raw-IP door (`src/net/socket.rs`),
/// which carries a complete IPv4 packet and therefore has to reach the device
/// *without* smoltcp's raw-socket emit - that emit drops a queued packet whose
/// IP protocol byte is not the socket's, and a DHCP client's packet is UDP while
/// the channel is `IPPROTO_RAW` (§4.5).
pub(crate) fn send_frame(dst_mac: [u8; 6], ethertype: u16, payload: &[u8]) -> Result<usize, isize> {
    let mac = crate::net::packet_mac();
    let Some(mac) = mac else {
        return Err(crate::err::ENETDOWN);
    };
    let frame = crate::link::encode_ethernet(dst_mac, mac, ethertype, payload);
    crate::net::packet_transmit(&frame)?;
    Ok(payload.len())
}

/// Parse a caller-supplied `sockaddr_ll`, reading it through the user-memory
/// boundary and handing the bytes to `crate::link`'s decoder. `EINVAL` is
/// Linux's answer for a short or wrong-family address.
pub(crate) fn parse_sll_from_user(
    ptr: usize,
    len: usize,
    from_user: bool,
) -> Result<SockaddrLl, isize> {
    use crate::err;
    if len < crate::link::SOCKADDR_LL_LEN || !crate::uaccess::buffer_ok(ptr, len, false, from_user)
    {
        return Err(err::EINVAL);
    }
    let mut raw = [0u8; crate::link::SOCKADDR_LL_LEN];
    unsafe {
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = (ptr as *const u8).add(i).read_unaligned();
        }
    }
    crate::link::parse_sll(&raw).ok_or(err::EINVAL)
}

/// Write one `sockaddr_ll` into the caller's buffer: `crate::link`'s encode
/// plus the fact only this side knows - which interface index `eth0` has.
pub(crate) fn write_sll(protocol: u16, addr: &[u8; 6], buf: usize) {
    let bytes = crate::link::encode_sll(protocol, ETH0_IFINDEX, addr);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
    }
}

/// `getsockname` on the socket: its `htons` protocol, eth0's index, and the
/// device's own MAC.
pub(crate) fn sockname(sock: &PacketSock, addr: usize, len_ptr: usize) -> isize {
    use crate::err;
    if addr == 0
        || len_ptr == 0
        || !crate::uaccess::buffer_ok(addr, crate::link::SOCKADDR_LL_LEN, true, true)
        || !crate::uaccess::buffer_ok(len_ptr, 4, true, true)
    {
        return err::EFAULT;
    }
    let mac = crate::net::packet_mac().unwrap_or([0u8; 6]);
    write_sll(sock.protocol, &mac, addr);
    unsafe {
        (len_ptr as *mut u32).write_unaligned(crate::link::SOCKADDR_LL_LEN as u32);
    }
    0
}
