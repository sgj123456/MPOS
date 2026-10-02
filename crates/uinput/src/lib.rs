#![no_std]

//! `/dev/uinput`: the ABI of an input device userland makes for itself.
//!
//! A program opens `/dev/uinput`, declares what its device can report
//! (`UI_SET_*BIT`), names it (`UI_DEV_SETUP`), and asks for it to exist
//! (`UI_DEV_CREATE`) - at which point a real input device appears under
//! `/dev/input/eventN`, and every whole `struct input_event` written to the
//! descriptor is what that device reports. Closing the descriptor, or
//! `UI_DEV_DESTROY`, takes the device away again. Linux's contract, and the
//! reason the descriptor *is* the device.
//!
//! It is here because the self-test machine has no hands: the conformance suite
//! cannot press a key, so a rule about who reads an event could only be checked
//! from the inside with a device that injects one. This module is the
//! arithmetic of the interface - which request means which bit, what a `write`
//! has to be shaped like, how `struct uinput_setup` is laid out - so a host
//! build can argue with it. The device itself (the node, the ioctls' plumbing,
//! the devfs side) is `drivers/uinput.rs`, which is bare metal.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use mpos_evdev::RECORD;

/// The ioctl type byte every uinput request carries: `UINPUT_IOCTL_BASE` in the
/// kernel's own header.
pub const IOCTL_BASE: u8 = b'U';

/// The device number `/dev/uinput` reports. `misc` devices all share major 10,
/// and uinput is minor 223 of them - the number udev's rule for naming the node
/// is written against.
pub const MAJOR: u32 = 10;
pub const MINOR: u32 = 223;

/// The interface version `UI_GET_VERSION` answers: the one whose `UI_DEV_SETUP`
/// and `UI_GET_VERSION` themselves arrived in.
pub const VERSION: u32 = 5;

/// `UI_DEV_CREATE` / `UI_DEV_DESTROY`: `_IO('U', 1)` and `_IO('U', 2)`.
pub const DEV_CREATE: u32 = 1;
pub const DEV_DESTROY: u32 = 2;
/// `UI_DEV_SETUP`: `_IOW('U', 3, struct uinput_setup)`.
pub const DEV_SETUP: u32 = 3;
/// `UI_GET_VERSION`: `_IOR('U', 45, unsigned int)`.
pub const GET_VERSION: u32 = 45;

/// `struct uinput_setup`: `struct input_id`, a name, then the force-feedback
/// effect count this kernel has no use for.
pub const SETUP_SIZE: usize = 92;
/// `UINPUT_MAX_NAME_SIZE`: the name field inside it.
pub const SETUP_NAME_MAX: usize = 80;

/// The event types the interface takes codes for. The numbers are the ABI's own
/// - `input-event-codes.h` - and they are the *request* numbers' other half.
pub const EV_KEY: u8 = 0x01;
pub const EV_REL: u8 = 0x02;
pub const EV_ABS: u8 = 0x03;
pub const EV_MSC: u8 = 0x04;
pub const EV_SW: u8 = 0x05;
pub const EV_LED: u8 = 0x11;
pub const EV_SND: u8 = 0x12;
pub const EV_FF: u8 = 0x15;

/// The highest event type the interface will declare: `EV_MAX`.
const EV_MAX: usize = 0x1f;
/// The highest property bit, `INPUT_PROP_MAX`.
const PROP_MAX: usize = 0x1f;

/// What one `UI_SET_*BIT` request declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitSet {
    /// `UI_SET_EVBIT`: this device reports event type `n`.
    EventType,
    /// `UI_SET_KEYBIT` and its siblings: this device reports code `n` of the
    /// event type the request names.
    Codes(u8),
    /// `UI_SET_PROPBIT`: the device carries `INPUT_PROP_*` bit `n`.
    Prop,
}

/// What the request number `nr` declares, or `None` for a request that is not
/// one of the bit-setters.
///
/// `UI_SET_PHYS` (108) is deliberately not one: it hands over a `char *` whose
/// physical path this kernel has nowhere to put, and a request answered
/// `ENOTTY` is more honest than one accepted and dropped.
pub fn bit_request(nr: u32) -> Option<BitSet> {
    match nr {
        100 => Some(BitSet::EventType),
        101 => Some(BitSet::Codes(EV_KEY)),
        102 => Some(BitSet::Codes(EV_REL)),
        103 => Some(BitSet::Codes(EV_ABS)),
        104 => Some(BitSet::Codes(EV_MSC)),
        105 => Some(BitSet::Codes(EV_LED)),
        106 => Some(BitSet::Codes(EV_SND)),
        107 => Some(BitSet::Codes(EV_FF)),
        109 => Some(BitSet::Codes(EV_SW)),
        110 => Some(BitSet::Prop),
        _ => None,
    }
}

/// The most code event type `evtype` can carry, as the kernel's own `*_MAX`
/// constants spell it. `None` is a type this interface takes no codes for.
fn code_limit(evtype: u8) -> Option<usize> {
    match evtype {
        EV_KEY => Some(0x2ff),
        EV_REL => Some(0x0f),
        EV_ABS => Some(0x3f),
        EV_MSC => Some(0x07),
        EV_SW => Some(0x10),
        EV_LED => Some(0x0f),
        EV_SND => Some(0x07),
        EV_FF => Some(0x7f),
        _ => None,
    }
}

/// The device one open file description is building - or has already created.
///
/// Linux keeps this in the `uinput_device` a descriptor owns, and it is why two
/// opens of `/dev/uinput` are two devices while a `dup` is one.
pub struct Device {
    /// The name `UI_DEV_SETUP` gave it, which is what `EVIOCGNAME` reports.
    pub name: String,
    /// `UI_DEV_SETUP`'s `struct input_id`: bus, vendor, product, version.
    pub id: [u16; 4],
    /// Bit *t* set when the device reports event type *t*.
    type_bits: [u8; 4],
    /// The code bitmap per event type, indexed by type.
    code_bits: [Vec<u8>; 0x20],
    /// The `INPUT_PROP_*` bits.
    prop_bits: Vec<u8>,
}

impl Default for Device {
    /// A device that has not been told anything yet: no name, no capabilities.
    fn default() -> Self {
        Device {
            name: String::new(),
            id: [0; 4],
            type_bits: [0; 4],
            code_bits: core::array::from_fn(|_| Vec::new()),
            prop_bits: Vec::new(),
        }
    }
}

impl Device {
    /// Declare one bit. `false` is `EINVAL`, the answer Linux gives a bit the
    /// request's own kind cannot carry - a key beyond `KEY_MAX`, an event type
    /// beyond `EV_MAX`.
    ///
    /// A code bit also sets its own event type's bit, which Linux's handlers do
    /// not do. The reason is the one §7.2 records about `capabilities/ev`: a
    /// device that answers `EVIOCGBIT(EV_KEY)` with bits while `EVIOCGBIT(0)`
    /// says `EV_KEY` is absent is a device no classifier can see, and a caller
    /// that declared `KEY_A` has declared `EV_KEY` whatever it forgot to send.
    pub fn set_bit(&mut self, set: BitSet, code: usize) -> bool {
        match set {
            BitSet::EventType => {
                if code > EV_MAX {
                    return false;
                }
                self.type_bits[code / 8] |= 1 << (code % 8);
                true
            }
            BitSet::Prop => {
                if code > PROP_MAX {
                    return false;
                }
                if self.prop_bits.len() <= code / 8 {
                    self.prop_bits.resize(code / 8 + 1, 0);
                }
                self.prop_bits[code / 8] |= 1 << (code % 8);
                true
            }
            BitSet::Codes(evtype) => {
                let Some(limit) = code_limit(evtype) else {
                    return false;
                };
                if code > limit {
                    return false;
                }
                self.type_bits[evtype as usize / 8] |= 1 << (evtype % 8);
                let bits = &mut self.code_bits[evtype as usize];
                if bits.len() <= code / 8 {
                    bits.resize(code / 8 + 1, 0);
                }
                bits[code / 8] |= 1 << (code % 8);
                true
            }
        }
    }

    /// The capability picture, in the shape the evdev ioctls answer from.
    pub fn type_bits(&self) -> [u8; 4] {
        self.type_bits
    }

    /// The code bitmap for `evtype`, empty when the type has none.
    pub fn code_bits(&self, evtype: u8) -> &[u8] {
        self.code_bits
            .get(evtype as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn prop_bits(&self) -> &[u8] {
        &self.prop_bits
    }
}

/// Read a `struct uinput_setup` out of the bytes the caller passed: its name
/// (up to the first NUL, and no longer than the name field) and its id. The
/// effect count that follows is read and dropped - this kernel has no
/// force-feedback - but it has to be *there*, because a caller that passed a
/// shorter structure is a caller this interface cannot speak its own version
/// of.
pub fn parse_setup(bytes: &[u8]) -> Option<(String, [u16; 4], u32)> {
    let short = bytes.len() < SETUP_SIZE;
    if short {
        return None;
    }
    let mut id = [0u16; 4];
    for (i, slot) in id.iter_mut().enumerate() {
        *slot = u16::from_ne_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
    }
    let name = bytes[8..8 + SETUP_NAME_MAX]
        .split(|&b| b == 0)
        .next()
        .unwrap_or(&[]);
    // A name that is not UTF-8 is not one this kernel will report back; the
    // replacement characters are the honest spelling of "these bytes were not
    // a string", and the caller can read them back and see that.
    let name = String::from_utf8_lossy(name).into_owned();
    let effects = u32::from_ne_bytes([bytes[88], bytes[89], bytes[90], bytes[91]]);
    Some((name, id, effects))
}

/// How many records a `write` of `len` bytes carries, or `None` when the length
/// is not a whole number of them.
///
/// A partial record is `EINVAL` rather than a short write, because a caller that
/// wrote one has a bug in its own buffering and a byte count that does not
/// describe what was reported is a way to never find out.
pub fn record_count(len: usize) -> Option<usize> {
    len.is_multiple_of(RECORD).then_some(len / RECORD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bit_requests_name_the_type_they_set() {
        assert_eq!(bit_request(100), Some(BitSet::EventType));
        assert_eq!(bit_request(101), Some(BitSet::Codes(EV_KEY)));
        assert_eq!(bit_request(102), Some(BitSet::Codes(EV_REL)));
        assert_eq!(bit_request(103), Some(BitSet::Codes(EV_ABS)));
        assert_eq!(bit_request(110), Some(BitSet::Prop));
        // `UI_SET_PHYS` hands over a string, and `UI_GET_VERSION` is not a bit
        // request at all.
        assert_eq!(bit_request(108), None);
        assert_eq!(bit_request(GET_VERSION), None);
        assert_eq!(bit_request(0), None);
    }

    #[test]
    fn a_code_bit_declares_its_own_event_type_too() {
        let mut device = Device::default();
        assert!(device.set_bit(BitSet::Codes(EV_KEY), 30)); // KEY_A
        assert_eq!(device.type_bits()[0] & (1 << EV_KEY), 1 << EV_KEY);
        assert_eq!(device.code_bits(EV_KEY), &[0, 0, 0, 1 << 6]);
        // And a type with no codes declared has nothing to report.
        assert_eq!(device.code_bits(EV_REL), &[] as &[u8]);
        assert!(device.code_bits(0x1f).is_empty());
    }

    #[test]
    fn a_bit_the_request_cannot_carry_is_refused() {
        let mut device = Device::default();
        // KEY_MAX is 0x2ff, so 0x300 is past the end of the key bitmap.
        assert!(!device.set_bit(BitSet::Codes(EV_KEY), 0x300));
        assert!(device.set_bit(BitSet::Codes(EV_KEY), 0x2ff));
        // REL_MAX is 0x0f.
        assert!(!device.set_bit(BitSet::Codes(EV_REL), 0x10));
        // EV_MAX is 0x1f.
        assert!(!device.set_bit(BitSet::EventType, 0x20));
        assert!(device.set_bit(BitSet::EventType, 0x1f));
        // INPUT_PROP_MAX is 0x1f.
        assert!(!device.set_bit(BitSet::Prop, 0x20));
        assert!(device.set_bit(BitSet::Prop, 0x1f));
        assert_eq!(device.prop_bits(), &[0, 0, 0, 0x80]);
    }

    #[test]
    fn a_bitmap_grows_to_hold_the_bit_it_was_given() {
        let mut device = Device::default();
        assert!(device.set_bit(BitSet::Codes(EV_KEY), 0x2ff));
        let bits = device.code_bits(EV_KEY);
        assert_eq!(bits.len(), 0x300 / 8);
        assert_eq!(bits[0x2ff / 8], 0x80);
    }

    #[test]
    fn a_write_that_is_not_whole_records_is_refused() {
        assert_eq!(record_count(0), Some(0));
        assert_eq!(record_count(RECORD), Some(1));
        assert_eq!(record_count(RECORD * 3), Some(3));
        assert_eq!(record_count(RECORD * 3 - 1), None);
        assert_eq!(record_count(1), None);
    }

    #[test]
    fn a_setup_is_read_out_of_its_own_bytes() {
        let mut bytes = [0u8; SETUP_SIZE];
        // `struct input_id { bustype, vendor, product, version }`.
        for (i, word) in [3u16, 0x1234, 0x5678, 0x0001].into_iter().enumerate() {
            bytes[i * 2..i * 2 + 2].copy_from_slice(&word.to_ne_bytes());
        }
        bytes[8..14].copy_from_slice(b"uitest");
        bytes[88..92].copy_from_slice(&0u32.to_ne_bytes());

        let (name, id, effects) = parse_setup(&bytes).expect("a whole setup");
        assert_eq!(name, "uitest");
        assert_eq!(id, [3, 0x1234, 0x5678, 0x0001]);
        assert_eq!(effects, 0);

        // A name field with no NUL is a name of the whole field, which is what
        // a caller that filled it exactly means.
        let mut bytes = [b'x'; SETUP_SIZE];
        bytes[88..92].copy_from_slice(&7u32.to_ne_bytes());
        let (name, _, effects) = parse_setup(&bytes).unwrap();
        assert_eq!(name.len(), SETUP_NAME_MAX);
        assert_eq!(effects, 7);

        // And a short structure is no structure.
        assert!(parse_setup(&[0u8; SETUP_SIZE - 1]).is_none());
    }
}
