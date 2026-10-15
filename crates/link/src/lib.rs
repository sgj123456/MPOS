#![no_std]

//! The link layer's wire format: `AF_PACKET`'s address struct, the Ethernet
//! header, and the packet socket's bounded frame ring.
//!
//! This is what `AF_PACKET` - Linux's `PF_PACKET`, the family that lets a
//! program speak Ethernet directly - needs in order to read or write a frame on
//! this kernel. The socket itself lives in `src/net/packet.rs`, and it is welded
//! to the NIC seam (`NicDevice::receive` hands it every arriving frame before
//! the IP layer decides the frame is any of its business, which is how `udhcpc`
//! reads a BOOTREPLY before the machine owns an address). That weld makes the
//! whole module machine-only, and the three parsers below were stranded with
//! it: no host unit test could reach them and no `fuzz/` target could either,
//! because both stop at a `target_os = "none"` gate. `AGENTS.md` §3 asks every
//! parser of user- or wire-supplied bytes for a fuzz target, so the wire format
//! moved out here, where a host build names it - the crate boundary is what
//! makes that permanent rather than a promise, exactly as `evdev` and `uinput`
//! were extracted for the same reason (`docs/notes/` §4.5, `docs/uapi/` §5.4).
//!
//! What is here, and the one shape each piece promises:
//!
//! * [`SockaddrLl`] - Linux's `struct sockaddr_ll` at `linux/if_packet.h`'s real
//!   offsets (family 0, protocol 2, ifindex 4, hatype 8, pkttype 10, halen 11,
//!   address 12; 20 bytes). [`parse_sll`] reads a caller's buffer and
//!   [`encode_sll`] writes the one this kernel answers with.
//! * [`EthFrame`] - the 14-byte Ethernet header (destination and source MAC,
//!   then the type) split from the payload. [`parse_ethernet`] *refuses* a
//!   frame shorter than the header rather than reading past it, and
//!   [`encode_ethernet`] builds the one a `SOCK_DGRAM` send owns.
//! * [`Frames`] - the socket's receive ring, bounded and keeping the newest.
//!
//! Two endian facts are load-bearing and easy to get subtly wrong, which is why
//! they are stated once here rather than at each of their call sites:
//!
//! * A frame's Ethernet type is compared as *the two bytes on the wire*.
//!   [`IPV4_ETHERTYPE`] is therefore the number whose little-endian bytes are
//!   `08 00`, so `u16::from_le_bytes([frame[12], frame[13]]) == IPV4_ETHERTYPE`
//!   holds without a host-order detour. A second spelling of this constant
//!   written with the other detour (`0x0800u16.to_le()`, which is `0x0008`)
//!   compared `0x0800` against `0x0008` and made the raw-IP door's seam return
//!   on *every* frame - measured, and the reason this is one definition
//!   (`docs/notes/` §4.5).
//! * `struct sockaddr_ll`'s fields are the caller's own native-endian words -
//!   the protocol in particular arrives already in network order, as every
//!   caller's `htons` spelled it, and is handed back byte-for-byte.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// The `AF_PACKET` family number, from Linux's `bits/socket.h`.
pub const AF_PACKET: u16 = 17;

/// `sizeof(struct sockaddr_ll)`, per `linux/if_packet.h`: family 0, protocol
/// 2, ifindex 4, hatype 8, pkttype 10, halen 11, addr 12.
pub const SOCKADDR_LL_LEN: usize = 20;

/// `ARPHRD_ETHER`: the hardware type the one interface this kernel serves
/// reports.
pub const ARPHRD_ETHER: u16 = 1;

/// The length of an Ethernet header: a destination MAC, a source MAC and the
/// type, six bytes each of the first two and two of the last.
pub const ETH_HEADER_LEN: usize = 14;

/// The wire's `08 00` for `ETH_P_IP`, as the number whose bytes *in memory* are
/// the frame's own two type bytes. Spelled this way because every target here
/// is little-endian and the frame's type is compared without a host-order
/// detour; see the module header for the second spelling this replaced.
pub const IPV4_ETHERTYPE: u16 = 0x0800u16.to_be();

/// Frames kept per socket before the oldest gives way. Two pages of DHCP
/// traffic by any measure; the number exists so a stopped reader cannot make
/// the kernel eat memory on its behalf. Linux drops frames for a packet socket
/// whose ring is full and marks the loss (`PACKET_STATISTICS` would report it);
/// keeping the newest is the same shape of answer for a client that
/// retransmits.
pub const RING_FRAMES: usize = 64;

/// A `sockaddr_ll`, decoded: the interface a packet socket is bound to, the
/// link type, the address length and the address itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockaddrLl {
    pub ifindex: i32,
    pub hatype: u16,
    pub halen: u8,
    pub addr: [u8; 8],
}

/// Parse the caller's `sockaddr_ll`, reading the `linux/if_packet.h` offsets.
///
/// `None` on a family mismatch or a buffer shorter than the struct - Linux's
/// `EINVAL`, which the caller turns into the return. A *longer* buffer is
/// accepted: the struct is all this reads, and a caller that passed more has a
/// version of the ABI with fields after the ones this interface knows.
pub fn parse_sll(raw: &[u8]) -> Option<SockaddrLl> {
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

/// Write one `sockaddr_ll`: family, the socket's own `htons` protocol, the
/// interface index the caller named, `ARPHRD_ETHER`, a six-byte address, and
/// the address itself.
///
/// Returned rather than written through a pointer so the encode is pure: the
/// caller - which is the only side that knows whether the destination is user
/// memory - copies it out. `ifindex` is an argument rather than a constant
/// because it is the machine's fact, not the wire format's.
pub fn encode_sll(protocol: u16, ifindex: i32, addr: &[u8; 6]) -> [u8; SOCKADDR_LL_LEN] {
    let mut out = [0u8; SOCKADDR_LL_LEN];
    out[0..2].copy_from_slice(&AF_PACKET.to_ne_bytes());
    out[2..4].copy_from_slice(&protocol.to_ne_bytes());
    out[4..8].copy_from_slice(&ifindex.to_ne_bytes());
    out[8..10].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
    out[10] = 0; // sll_pkttype: unclassified here
    out[11] = 6; // sll_halen
    out[12..18].copy_from_slice(addr);
    out
}

/// One Ethernet frame's header, split from the payload it carries. The
/// destination is kept even though the tap does not yet read it: it is what a
/// frame *has*, and the `SOCK_RAW` half of `AF_PACKET` - frames with their
/// header on both sides - will hand it back whole when it lands
/// (`docs/uapi/` §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EthFrame<'a> {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

/// Split an Ethernet frame at its header.
///
/// `None` for a frame shorter than [`ETH_HEADER_LEN`]: a caller-fed length may
/// not be trusted to have a header, and the honest answer to a runt is that
/// there is no frame here rather than a read past the end.
pub fn parse_ethernet(frame: &[u8]) -> Option<EthFrame<'_>> {
    if frame.len() < ETH_HEADER_LEN {
        return None;
    }
    let mut dst = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    let mut src = [0u8; 6];
    src.copy_from_slice(&frame[6..12]);
    Some(EthFrame {
        dst,
        src,
        ethertype: u16::from_le_bytes([frame[12], frame[13]]),
        payload: &frame[ETH_HEADER_LEN..],
    })
}

/// Build one Ethernet frame: destination, source, the type, then the payload.
///
/// The header is the kernel's to build for a `SOCK_DGRAM` send, which is what
/// that shape of `AF_PACKET` promises and what keeps the caller from inventing
/// a source MAC the device does not own.
pub fn encode_ethernet(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETH_HEADER_LEN + payload.len());
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&src);
    frame.extend_from_slice(&ethertype.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// One tapped frame: the **whole** Ethernet frame, header first, beside the two
/// header facts a `recvfrom` answer needs - the source MAC and the Ethernet
/// type, as their wire patterns.
///
/// The frame is kept whole because the two `AF_PACKET` receive shapes differ
/// only in where the reader starts: `SOCK_DGRAM` is promised the payload half
/// and drops the header with [`frame_payload`], while `SOCK_RAW` is promised the
/// frame with its header on and hands back these same bytes. The tap knows the
/// frame and not which type each socket wanted, so it stores one shape and lets
/// the reader slice it - the alternative, a tap that split per socket, would
/// mean dropping and re-splitting the same header once per listener.
pub type Frame = (Vec<u8>, [u8; 6], u16);

/// The payload half of a tapped frame: everything after the Ethernet header,
/// which is what a `SOCK_DGRAM` reader is promised. An empty slice when there is
/// no complete header to skip - the tap never stores such a frame (it splits
/// with [`parse_ethernet`] first), but a test may, and a read past the end is
/// the one answer it may not give.
pub fn frame_payload(frame: &[u8]) -> &[u8] {
    frame.get(ETH_HEADER_LEN..).unwrap_or(&[])
}

/// The receive ring one packet socket keeps: bounded, oldest-first, and full of
/// the newest frames when it has overflowed.
pub struct Frames {
    queue: VecDeque<Frame>,
    capacity: usize,
}

impl Frames {
    /// A ring of [`RING_FRAMES`], the capacity a packet socket runs with.
    pub fn new() -> Self {
        Self::with_capacity(RING_FRAMES)
    }

    /// A ring of `capacity` frames. The kernel uses [`Frames::new`]; a smaller
    /// bound is a test's or a fuzz target's way to exercise the overflow rule
    /// without 64 frames of setup.
    pub fn with_capacity(capacity: usize) -> Self {
        Frames {
            queue: VecDeque::new(),
            capacity,
        }
    }

    /// Queue one frame, dropping the oldest while the ring is over capacity.
    pub fn push(&mut self, frame: Frame) {
        self.queue.push_back(frame);
        while self.queue.len() > self.capacity {
            self.queue.pop_front();
        }
    }

    /// Take the oldest queued frame, if any.
    pub fn pop(&mut self) -> Option<Frame> {
        self.queue.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_ipv4_ethertype_is_the_wires_two_bytes() {
        // The pin for the measured bug (§4.5): the constant must be the number
        // whose little-endian bytes are the frame's `08 00`, so that a
        // comparison via `from_le_bytes` needs no host-order detour. A second
        // spelling written with the other detour was `0x0800`, compared
        // against `0x0008`, and made a seam return on every frame.
        assert_eq!(IPV4_ETHERTYPE, u16::from_le_bytes([0x08, 0x00]));
        assert_eq!(IPV4_ETHERTYPE.to_le_bytes(), [0x08, 0x00]);
    }

    #[test]
    fn a_short_or_foreign_address_is_refused() {
        assert!(parse_sll(&[]).is_none());
        assert!(parse_sll(&[0u8; SOCKADDR_LL_LEN - 1]).is_none());

        let mut foreign = encode_sll(0, 0, &[0; 6]);
        foreign[0..2].copy_from_slice(&2u16.to_ne_bytes()); // AF_INET
        assert!(parse_sll(&foreign).is_none());
    }

    #[test]
    fn the_address_fields_sit_where_linux_puts_them() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let raw = encode_sll(0x0008, 2, &mac);

        assert_eq!(u16::from_ne_bytes([raw[0], raw[1]]), AF_PACKET);
        assert_eq!(u16::from_ne_bytes([raw[2], raw[3]]), 0x0008);
        assert_eq!(i32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]), 2);
        assert_eq!(u16::from_ne_bytes([raw[8], raw[9]]), ARPHRD_ETHER);
        assert_eq!(raw[10], 0, "sll_pkttype");
        assert_eq!(raw[11], 6, "sll_halen");
        assert_eq!(&raw[12..18], &mac);
        assert_eq!(&raw[18..20], &[0, 0], "the two bytes past a six-byte MAC");

        let parsed = parse_sll(&raw).expect("its own encode parses");
        assert_eq!(parsed.ifindex, 2);
        assert_eq!(parsed.hatype, ARPHRD_ETHER);
        assert_eq!(parsed.halen, 6);
        assert_eq!(&parsed.addr[..6], &mac);
        assert_eq!(&parsed.addr[6..], &[0, 0]);
    }

    #[test]
    fn a_longer_buffer_still_parses() {
        // A caller that passed more than the struct has a longer version of the
        // ABI; the struct is all this reads, so it must not be refused.
        let mut raw = [0u8; SOCKADDR_LL_LEN + 8];
        raw[..SOCKADDR_LL_LEN].copy_from_slice(&encode_sll(0x0008, 2, &[1; 6]));
        assert!(parse_sll(&raw).is_some());
    }

    #[test]
    fn a_runt_has_no_ethernet_header() {
        assert!(parse_ethernet(&[]).is_none());
        assert!(parse_ethernet(&[0u8; ETH_HEADER_LEN - 1]).is_none());
        assert!(parse_ethernet(&[0u8; ETH_HEADER_LEN]).is_some());
    }

    #[test]
    fn the_header_splits_where_the_wire_says() {
        let mut frame = vec![];
        frame.extend_from_slice(&[1, 2, 3, 4, 5, 6]); // destination
        frame.extend_from_slice(&[7, 8, 9, 10, 11, 12]); // source
        frame.extend_from_slice(&[0x08, 0x00]); // ETH_P_IP, on the wire
        frame.extend_from_slice(&[0xaa, 0xbb]); // payload

        let eth = parse_ethernet(&frame).expect("a whole header");
        assert_eq!(eth.dst, [1, 2, 3, 4, 5, 6]);
        assert_eq!(eth.src, [7, 8, 9, 10, 11, 12]);
        assert_eq!(eth.ethertype, IPV4_ETHERTYPE);
        assert_eq!(eth.payload, &[0xaa, 0xbb]);
    }

    #[test]
    fn a_built_frame_reads_back_as_built() {
        let built = encode_ethernet(
            [1, 2, 3, 4, 5, 6],
            [7, 8, 9, 10, 11, 12],
            IPV4_ETHERTYPE,
            &[0xaa, 0xbb],
        );
        assert_eq!(built.len(), ETH_HEADER_LEN + 2);

        let back = parse_ethernet(&built).expect("its own encode parses");
        assert_eq!(back.dst, [1, 2, 3, 4, 5, 6]);
        assert_eq!(back.src, [7, 8, 9, 10, 11, 12]);
        assert_eq!(back.ethertype, IPV4_ETHERTYPE);
        assert_eq!(back.payload, &[0xaa, 0xbb]);
    }

    #[test]
    fn the_payload_half_drops_the_header() {
        // The `SOCK_DGRAM` promise: what a reader of the payload shape gets is
        // the frame minus its header, and nothing else - the same bytes the
        // pre-`SOCK_RAW` tap stored directly.
        let frame = encode_ethernet(
            [1, 2, 3, 4, 5, 6],
            [7, 8, 9, 10, 11, 12],
            IPV4_ETHERTYPE,
            &[0xaa, 0xbb],
        );
        assert_eq!(frame_payload(&frame), &[0xaa, 0xbb]);
        assert_eq!(frame_payload(&frame).len(), frame.len() - ETH_HEADER_LEN);

        // A frame with no complete header has no payload to hand a reader: the
        // empty slice, not a read past the end.
        assert_eq!(frame_payload(&[]), &[] as &[u8]);
        assert_eq!(frame_payload(&[0u8; ETH_HEADER_LEN - 1]), &[] as &[u8]);
        assert_eq!(frame_payload(&[0u8; ETH_HEADER_LEN]), &[] as &[u8]);
    }

    #[test]
    fn the_ring_hands_frames_back_in_order() {
        let mut ring = Frames::new();
        assert!(ring.is_empty());

        ring.push((vec![1], [0; 6], 1));
        ring.push((vec![2], [0; 6], 2));

        assert_eq!(ring.pop().expect("the oldest").0, vec![1]);
        assert_eq!(ring.pop().expect("the newest").0, vec![2]);
        assert!(ring.pop().is_none());
        assert!(ring.is_empty());
    }

    #[test]
    fn the_ring_bounds_its_length_and_keeps_the_newest() {
        let mut ring = Frames::new();
        for i in 0..(RING_FRAMES + 5) {
            ring.push((vec![i as u8], [0; 6], 0));
        }
        assert_eq!(ring.len(), RING_FRAMES);

        // The five oldest are gone; the survivors are the last RING_FRAMES
        // pushed, oldest first.
        assert_eq!(ring.pop().expect("a survivor").0, vec![5]);
    }

    #[test]
    fn a_ring_of_one_keeps_only_the_newest() {
        let mut ring = Frames::with_capacity(1);
        ring.push((vec![1], [0; 6], 0));
        ring.push((vec![2], [0; 6], 0));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.pop().expect("the newest").0, vec![2]);
        assert!(ring.is_empty());
    }
}
