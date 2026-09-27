#![no_main]

//! What `/dev/uinput` agrees a device is, read out of the bytes a program
//! passed.
//!
//! `src/uinput.rs` is the arithmetic of `UI_DEV_SETUP` and the `UI_SET_*BIT`
//! family: the struct layout a caller's `write`/`ioctl` presents, and the bit
//! a request declares. All of it is userland-supplied, so the questions worth
//! fuzzing are the ones a wrong answer turns into a device that lies about
//! itself: does a short struct stay refused, does a name never escape its
//! field or its NUL, do the id words stay exactly the bytes given, and does a
//! declared bit land in the bitmap `EVIOCGBIT` will later read back.

use arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use mpos::evdev::RECORD;
use mpos::uinput::{self, BitSet, Device, SETUP_NAME_MAX, SETUP_SIZE};

#[derive(Arbitrary, Debug)]
enum Bit {
    Type { code: usize },
    Code { evtype: u8, code: usize },
    Prop { code: usize },
}

#[derive(Arbitrary, Debug)]
struct Input {
    bytes: Vec<u8>,
    requests: Vec<u32>,
    bits: Vec<Bit>,
}

/// Whether bit `code` is set in a bitmap, the way the kernel's accessors
/// spell it (bit `code` is byte `code / 8`, bit `code % 8`).
fn has_bit(map: &[u8], code: usize) -> bool {
    map.get(code / 8)
        .is_some_and(|b| b & (1 << (code % 8)) != 0)
}

fuzz_target!(|input: Input| {
    // --- `struct uinput_setup`, by length -------------------------------
    //
    // A struct shorter than the one this interface speaks is refused
    // outright, and that is the whole of the length rule: a caller that
    // passed fewer bytes has a version of the ABI this kernel cannot read.
    let parsed = uinput::parse_setup(&input.bytes);
    assert_eq!(
        parsed.is_some(),
        input.bytes.len() >= SETUP_SIZE,
        "a {}-byte struct answered {}",
        input.bytes.len(),
        if parsed.is_some() { "yes" } else { "no" },
    );

    if let Some((name, id, effects)) = parsed {
        // The name is what stands before the first NUL inside its own field,
        // decoded lossily (a name that is not UTF-8 reads back with
        // replacement characters rather than being dropped).
        let field = &input.bytes[8..8 + SETUP_NAME_MAX];
        let raw = field.split(|&b| b == 0).next().unwrap_or(&[]);
        assert_eq!(name, String::from_utf8_lossy(raw));
        assert!(
            name.len() <= SETUP_NAME_MAX,
            "a {}-character name escaped its {}-byte field",
            name.len(),
            SETUP_NAME_MAX,
        );
        assert!(
            !name.as_bytes().contains(&0),
            "a NUL survived into the name"
        );

        // The four `struct input_id` words are the first eight bytes, native
        // endian; the effect count is the `u32` at 88.
        for (i, want) in id.iter().enumerate() {
            let got = u16::from_ne_bytes([input.bytes[i * 2], input.bytes[i * 2 + 1]]);
            assert_eq!(*want, got, "id word {i} was not the bytes given");
        }
        assert_eq!(
            effects,
            u32::from_ne_bytes([
                input.bytes[88],
                input.bytes[89],
                input.bytes[90],
                input.bytes[91]
            ]),
        );
    }

    // --- `record_count`: a `write` is a whole number of records ---------
    let count = uinput::record_count(input.bytes.len());
    assert_eq!(
        count.is_some(),
        input.bytes.len().is_multiple_of(RECORD),
        "a {}-byte write answered {}",
        input.bytes.len(),
        if count.is_some() { "a count" } else { "no" },
    );
    if let Some(n) = count {
        assert_eq!(n * RECORD, input.bytes.len());
    }

    // --- `bit_request`: only the requests that name a bit set one -------
    for &nr in input.requests.iter().take(64) {
        let known = matches!(nr, 100..=107 | 109 | 110);
        assert_eq!(
            uinput::bit_request(nr).is_some(),
            known,
            "request {nr} answered {}",
            if known { "unknown" } else { "a bit set" },
        );
    }

    // --- `set_bit`: a declared bit lands where the reader will look -----
    let mut device = Device::default();
    for bit in input.bits.iter().take(512) {
        let (set, accepted, present) = match *bit {
            Bit::Type { code } => {
                let accepted = device.set_bit(BitSet::EventType, code);
                (
                    BitSet::EventType,
                    accepted,
                    has_bit(&device.type_bits(), code),
                )
            }
            Bit::Code { evtype, code } => {
                let accepted = device.set_bit(BitSet::Codes(evtype), code);
                let present = has_bit(device.code_bits(evtype), code);
                if accepted {
                    // Declaring a code declares its event type too, or the
                    // bitmap `EVIOCGBIT` hands out would name codes under a
                    // type the device says it does not report.
                    assert!(
                        has_bit(&device.type_bits(), evtype as usize),
                        "code {code} of type {evtype} was declared without the type"
                    );
                }
                (BitSet::Codes(evtype), accepted, present)
            }
            Bit::Prop { code } => {
                let accepted = device.set_bit(BitSet::Prop, code);
                (BitSet::Prop, accepted, has_bit(device.prop_bits(), code))
            }
        };
        assert_eq!(
            accepted, present,
            "{set:?} answered {accepted} and the bitmap says {present}",
        );
    }
});
