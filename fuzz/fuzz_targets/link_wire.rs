#![no_main]

//! The link layer's wire format, read out of bytes a caller or the wire chose.
//!
//! `crates/link` holds `AF_PACKET`'s three parsers: `struct sockaddr_ll`'s
//! decode and encode, the Ethernet header, and the packet socket's bounded
//! frame ring. They live in a leaf crate precisely so a host build can reach
//! them - inside `src/net/packet.rs` they sat behind `net`'s
//! `target_os = "none"` gate, with no host test and no fuzz target, which is
//! what `AGENTS.md` §3's "every new parser gets a fuzz target" forbids
//! (`docs/notes/` §4.5).
//!
//! The questions here are the ones a wrong answer turns into a frame the
//! kernel misreads rather than a crash: does a short or foreign `sockaddr_ll`
//! stay refused, do the decoded fields sit at `linux/if_packet.h`'s offsets and
//! equal the bytes given, does a runt frame get refused rather than read past,
//! is the Ethernet type the *two bytes on the wire* (the endianness that a
//! second spelling got wrong once), do both encodes round-trip, and does the
//! ring keep the newest without ever growing past its bound.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::link::{
    self, Frames, AF_PACKET, ETH_HEADER_LEN, RING_FRAMES, SOCKADDR_LL_LEN,
};

#[derive(Arbitrary, Debug)]
struct Input {
    /// Raw bytes handed to both parsers, unexamined.
    bytes: Vec<u8>,
    /// A protocol word for `encode_sll`/`encode_ethernet` - any `u16` must
    /// survive the round trip.
    protocol: u16,
    /// An interface index for `encode_sll` - signed, so a negative is legal.
    ifindex: i32,
    /// Payloads to push through the ring, in order.
    pushes: Vec<Vec<u8>>,
}

/// Six bytes from anywhere in the buffer, zero-filled past its end: a MAC to
/// encode with, without caring whether the input was long enough to hold one.
fn six(bytes: &[u8]) -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (i, slot) in mac.iter_mut().enumerate() {
        *slot = bytes.get(i).copied().unwrap_or(0);
    }
    mac
}

fuzz_target!(|input: Input| {
    // --- `sockaddr_ll`, by length and family ----------------------------
    //
    // A struct shorter than the one this interface speaks is refused, and so is
    // one whose family is not `AF_PACKET`; nothing else may be. That is the
    // whole of the rule, and a caller cannot read the ordering off one refusal.
    let parsed = link::parse_sll(&input.bytes);
    let long_enough = input.bytes.len() >= SOCKADDR_LL_LEN;
    let right_family = long_enough
        && u16::from_ne_bytes([input.bytes[0], input.bytes[1]]) == AF_PACKET;
    assert_eq!(
        parsed.is_some(),
        right_family,
        "a {}-byte, family-{:?} address answered {}",
        input.bytes.len(),
        input.bytes.first().copied(),
        if parsed.is_some() { "yes" } else { "no" },
    );

    if let Some(sll) = parsed {
        // Every field is the bytes at its own offset, native-endian.
        assert_eq!(
            sll.ifindex,
            i32::from_ne_bytes([
                input.bytes[4],
                input.bytes[5],
                input.bytes[6],
                input.bytes[7]
            ]),
        );
        assert_eq!(sll.hatype, u16::from_ne_bytes([input.bytes[8], input.bytes[9]]));
        assert_eq!(sll.halen, input.bytes[11]);
        assert_eq!(&sll.addr[..], &input.bytes[12..20]);
    }

    // --- `encode_sll` round-trips ---------------------------------------
    let mac = six(&input.bytes);
    let encoded = link::encode_sll(input.protocol, input.ifindex, &mac);
    assert_eq!(encoded.len(), SOCKADDR_LL_LEN);
    let back = link::parse_sll(&encoded).expect("its own encode parses");
    assert_eq!(back.ifindex, input.ifindex);
    assert_eq!(&back.addr[..6], &mac);
    assert_eq!(&encoded[2..4], &input.protocol.to_ne_bytes());

    // --- the Ethernet header --------------------------------------------
    let eth = link::parse_ethernet(&input.bytes);
    assert_eq!(
        eth.is_some(),
        input.bytes.len() >= ETH_HEADER_LEN,
        "a {}-byte frame answered {}",
        input.bytes.len(),
        if eth.is_some() { "a header" } else { "no header" },
    );

    if let Some(frame) = eth {
        assert_eq!(frame.payload.len(), input.bytes.len() - ETH_HEADER_LEN);
        assert_eq!(
            frame.ethertype,
            u16::from_le_bytes([input.bytes[12], input.bytes[13]]),
            "the type is the two bytes on the wire",
        );
        assert_eq!(&frame.dst[..], &input.bytes[0..6]);
        assert_eq!(&frame.src[..], &input.bytes[6..12]);
    }

    // --- `encode_ethernet` round-trips ----------------------------------
    let dst = six(&input.bytes);
    let src = six(input.bytes.get(6..).unwrap_or_default());
    let built = link::encode_ethernet(dst, src, input.protocol, &input.bytes);
    assert_eq!(built.len(), ETH_HEADER_LEN + input.bytes.len());
    let read_back = link::parse_ethernet(&built).expect("its own encode parses");
    assert_eq!(read_back.dst, dst);
    assert_eq!(read_back.src, src);
    assert_eq!(read_back.ethertype, input.protocol);
    assert_eq!(read_back.payload, &input.bytes[..]);

    // --- the ring bounds, and keeps the newest --------------------------
    let pushed = input.pushes.len().min(RING_FRAMES + 8);
    let mut ring = Frames::new();
    for i in 0..pushed {
        ring.push((input.pushes[i].clone(), [0x11; 6], i as u16));
        assert!(
            ring.len() <= RING_FRAMES,
            "a {}-frame ring held {} frames",
            RING_FRAMES,
            ring.len(),
        );
    }
    assert_eq!(ring.len(), pushed.min(RING_FRAMES));

    // Whatever overflowed is the oldest; the survivors are the last
    // `RING_FRAMES` pushed, handed back oldest-first.
    let dropped = pushed.saturating_sub(RING_FRAMES);
    for i in dropped..pushed {
        let (payload, src, proto) = ring.pop().expect("a survivor");
        assert_eq!(payload, input.pushes[i]);
        assert_eq!(src, [0x11; 6]);
        assert_eq!(proto, i as u16);
    }
    assert!(ring.is_empty());

    // A one-frame ring keeps only the newest, whatever was pushed.
    let mut small = Frames::with_capacity(1);
    for payload in input.pushes.iter().take(3) {
        small.push((payload.clone(), [0; 6], 0));
    }
    assert_eq!(small.len(), usize::from(!input.pushes.is_empty()));
});
