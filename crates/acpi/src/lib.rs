#![no_std]

//! ACPI tables: the machine, describing itself, as bytes.
//!
//! x86_64 firmware hands the kernel almost nothing directly - no device tree,
//! and a memory map that says where RAM is but not what is in it. What it does
//! publish is ACPI: a root pointer, a tree of tables behind it, and in those
//! tables the machine's account of its own devices, buses and windows. `NOTES.md`
//! §7.4's PCI identity chain and this crate's first consumer both read them.
//!
//! The first consumer is **MCFG**, the memory-mapped configuration table. A
//! PCIe machine may decode configuration space as *memory* rather than through
//! the 0xcf8/0xcfc port pair: one 4 KiB page per function, one 1 MiB window per
//! bus, and MCFG names where those windows are. Linux calls the mechanism ECAM
//! (or MMCONFIG) and prefers it, because mechanism 1's address register has
//! eight bits of offset - 256 bytes per function, no PCIe extended
//! capabilities - while a memory window reaches all 4 KiB. Reading MCFG is
//! therefore what "enumerate over MMIO, ports as the fallback" starts with.
//!
//! Why this is its own crate: nothing in it can fail at boot for a reason a
//! host test cannot reproduce. Every question it answers - is this a root
//! pointer, does this table's checksum hold, how long is it, where are its
//! memory windows - is arithmetic on a `&[u8]`, and arithmetic is worth a test.
//! What stays outside is the machine's half: *reading* a physical table (the
//! caller maps it, because only the machine knows how) and deciding what to do
//! with a window (`src/drivers/pci.rs`).
//!
//! Two rules the reader keeps, both of them the shape `crates/fdt` records for
//! the same reason - a firmware that lies must not make the kernel read past the
//! buffer it was handed. First, a table's declared length is checked against the
//! buffer that *arrived* rather than trusted; second, a checksum is a condition
//! on using the table rather than a note about it, so a table that fails is
//! refused rather than parsed loosely. Neither rule can be relaxed by a caller.

extern crate alloc;

use alloc::vec::Vec;

/// The root pointer's signature: eight bytes, `RSD PTR ` - the space is part of
/// it, and a reader that trims it matches nothing.
pub const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";

/// The ACPI 1.0 root pointer: signature, checksum, OEM id, revision, RSDT.
pub const RSDP_V1_LEN: usize = 20;
/// The ACPI 2.0 root pointer, which appends a length, the XSDT and a second
/// checksum covering the whole of it.
pub const RSDP_V2_LEN: usize = 36;

/// Every ACPI table begins with the same 36-byte header: signature, length,
/// revision, checksum, six bytes of OEM id, eight of OEM table id, four of OEM
/// revision, four of creator id and four of creator revision.
pub const SDT_HEADER_LEN: usize = 36;

/// The memory-mapped configuration table's signature.
pub const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// One bus's worth of configuration space in an ECAM window: 32 devices × 8
/// functions × 4 KiB.
pub const ECAM_BUS_BYTES: u64 = 1 << 20;

/// The sum of every byte of a table is zero when its checksum holds. One
/// function for both the root pointer and a table, because the rule is one
/// rule.
pub fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

/// A root pointer that has been validated: where the root table is, and which
/// width its entries are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    /// The ACPI revision the machine claims. Two or above means the extended
    /// half is present and authoritative.
    pub revision: u8,
    /// The 32-bit root table's address, which every revision carries.
    pub rsdt: u32,
    /// The 64-bit root table's address; zero on a 1.0 machine.
    pub xsdt: u64,
}

impl Rsdp {
    /// Whether to walk the XSDT rather than the RSDT - the extended root is the
    /// one a revision-2 machine means, and reading the 32-bit one on such a
    /// machine would miss every table above 4 GiB.
    pub fn has_xsdt(&self) -> bool {
        self.revision >= 2 && self.xsdt != 0
    }
}

/// Validate a root pointer and read what it names.
///
/// `bytes` is the 36-byte window the firmware published an RSDP in - the caller
/// has already found it (UEFI names it in its configuration table, or it sits
/// in the first kilobyte of EBDA) and this only decides whether what is there is
/// a root pointer. A revision-1 pointer is accepted with `xsdt` zero; a
/// revision-2 one is checked against its *extended* checksum, which covers the
/// bytes the 1.0 checksum does not.
pub fn parse_rsdp(bytes: &[u8]) -> Option<Rsdp> {
    if bytes.len() < RSDP_V1_LEN || &bytes[0..8] != RSDP_SIGNATURE {
        return None;
    }
    if !checksum_ok(&bytes[..RSDP_V1_LEN]) {
        return None;
    }
    let revision = bytes[15];
    let rsdt = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let mut xsdt = 0u64;
    if revision >= 2 {
        if bytes.len() < RSDP_V2_LEN || !checksum_ok(&bytes[..RSDP_V2_LEN]) {
            return None;
        }
        xsdt = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
    }
    Some(Rsdp {
        revision,
        rsdt,
        xsdt,
    })
}

/// A table's header, read far enough to trust the rest of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
}

/// Validate one table's header against the bytes that arrived.
///
/// A table whose declared length runs past its buffer, or is shorter than the
/// header itself, is refused rather than clamped: the length is what every
/// reader after this one will stride by, so a wrong one is not a table this
/// crate can describe.
pub fn parse_header(bytes: &[u8]) -> Option<SdtHeader> {
    if bytes.len() < SDT_HEADER_LEN {
        return None;
    }
    let length = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let length = length as usize;
    if length < SDT_HEADER_LEN || length > bytes.len() {
        return None;
    }
    if !checksum_ok(&bytes[..length]) {
        return None;
    }
    Some(SdtHeader {
        signature: bytes[0..4].try_into().ok()?,
        length: length as u32,
        revision: bytes[8],
    })
}

/// The table addresses a root table names.
///
/// `wide` is the RSDT/XSDT difference and nothing else: an XSDT holds 64-bit
/// addresses, an RSDT 32. A trailing partial entry - a length that is not a
/// whole number of entries - is ignored rather than read past, which is the same
/// rule as everywhere else here.
pub fn table_entries(body: &[u8], wide: bool) -> Vec<u64> {
    let stride = if wide { 8 } else { 4 };
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + stride <= body.len() {
        let address = if wide {
            u64::from_le_bytes(body[at..at + 8].try_into().expect("8 bytes"))
        } else {
            u32::from_le_bytes(body[at..at + 4].try_into().expect("4 bytes")) as u64
        };
        out.push(address);
        at += stride;
    }
    out
}

/// One memory-mapped configuration window, as MCFG describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcamRange {
    /// Where the window is in physical address space.
    pub base: u64,
    /// The PCI segment group it decodes. Almost every machine has one, segment
    /// zero; the field is why the machine's PCI addresses carry a domain.
    pub segment: u16,
    /// The first and last bus numbers the window covers.
    pub start_bus: u8,
    pub end_bus: u8,
}

impl EcamRange {
    /// How much address space the window spans.
    pub fn len(&self) -> u64 {
        (self.end_bus as u64 + 1 - self.start_bus as u64) * ECAM_BUS_BYTES
    }

    /// Whether the window is empty - a table may name a range no bus is in, and
    /// a caller that mapped it anyway would map a page nothing uses.
    pub fn is_empty(&self) -> bool {
        self.end_bus < self.start_bus
    }

    /// Where one function's 4 KiB of configuration space starts, inside the
    /// window - or `None` for a bus the window does not cover.
    ///
    /// The arithmetic is the ECAM layout and nothing else: 20 bits of bus, then
    /// three of device, then three of function, then the 12-bit register offset
    /// a caller adds.
    pub fn offset(&self, bus: u8, device: u8, function: u8) -> Option<u64> {
        if self.is_empty() || bus < self.start_bus || bus > self.end_bus || device > 31 || function > 7
        {
            return None;
        }
        Some(
            ((bus - self.start_bus) as u64) << 20
                | (device as u64) << 15
                | (function as u64) << 12,
        )
    }
}

/// Every window an MCFG table names, in the table's order.
///
/// The body is the whole table, header included. A table with no room for even
/// one entry is an empty answer rather than a refusal - the header is a
/// well-formed table that describes no window, which is a fact about a machine
/// rather than a corrupt table.
pub fn parse_mcfg(bytes: &[u8]) -> Vec<EcamRange> {
    let Some(header) = parse_header(bytes) else {
        return Vec::new();
    };
    if header.signature != *MCFG_SIGNATURE {
        return Vec::new();
    }
    let body_start = SDT_HEADER_LEN;
    let end = header.length as usize;
    let mut out = Vec::new();
    let mut at = body_start;
    // Each entry is 16 bytes: a 64-bit base, a 16-bit segment, the first and
    // last bus, and four reserved bytes.
    while at + 16 <= end {
        let base = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let segment = u16::from_le_bytes(bytes[at + 8..at + 10].try_into().expect("2 bytes"));
        let start_bus = bytes[at + 10];
        let end_bus = bytes[at + 11];
        out.push(EcamRange {
            base,
            segment,
            start_bus,
            end_bus,
        });
        at += 16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Build a table whose checksum holds, so a test can vary one thing at a
    /// time from a table the crate accepts.
    fn table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let len = SDT_HEADER_LEN + body.len();
        let mut out = vec![0u8; len];
        out[0..4].copy_from_slice(signature);
        out[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        out[8] = 1;
        out[10..16].copy_from_slice(b"MPOSTS");
        out[36..].copy_from_slice(body);
        let sum = out.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        out[9] = 0u8.wrapping_sub(sum);
        out
    }

    fn mcfg_entry(base: u64, segment: u16, start: u8, end: u8) -> [u8; 16] {
        let mut entry = [0u8; 16];
        entry[0..8].copy_from_slice(&base.to_le_bytes());
        entry[8..10].copy_from_slice(&segment.to_le_bytes());
        entry[10] = start;
        entry[11] = end;
        entry
    }

    fn rsdp_v2(rsdt: u32, xsdt: u64) -> Vec<u8> {
        let mut out = vec![0u8; RSDP_V2_LEN];
        out[0..8].copy_from_slice(RSDP_SIGNATURE);
        out[15] = 2;
        out[16..20].copy_from_slice(&rsdt.to_le_bytes());
        out[20..24].copy_from_slice(&(RSDP_V2_LEN as u32).to_le_bytes());
        out[24..32].copy_from_slice(&xsdt.to_le_bytes());
        let sum = out[..RSDP_V1_LEN].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        out[8] = 0u8.wrapping_sub(sum);
        let sum = out.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        out[32] = 0u8.wrapping_sub(sum);
        out
    }

    #[test]
    fn a_root_pointer_is_read_where_acpi_puts_it() {
        let bytes = rsdp_v2(0x1234_5678, 0x1_0000_0000_2000);
        let rsdp = parse_rsdp(&bytes).expect("a well-formed root pointer");
        assert_eq!(rsdp.revision, 2);
        assert_eq!(rsdp.rsdt, 0x1234_5678);
        assert_eq!(rsdp.xsdt, 0x1_0000_0000_2000);
        assert!(rsdp.has_xsdt(), "a revision-2 pointer names an XSDT");
    }

    #[test]
    fn a_root_pointer_whose_checksum_does_not_hold_is_refused() {
        let mut bytes = rsdp_v2(0x1000, 0x2000);
        bytes[20] ^= 0x01;
        assert_eq!(parse_rsdp(&bytes), None);
    }

    #[test]
    fn a_revision_one_pointer_has_no_xsdt_to_walk() {
        let mut bytes = vec![0u8; RSDP_V1_LEN];
        bytes[0..8].copy_from_slice(RSDP_SIGNATURE);
        bytes[15] = 0;
        bytes[16..20].copy_from_slice(&0x4000u32.to_le_bytes());
        let sum = bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        bytes[8] = 0u8.wrapping_sub(sum);
        let rsdp = parse_rsdp(&bytes).expect("a 1.0 root pointer");
        assert_eq!(rsdp.rsdt, 0x4000);
        assert!(!rsdp.has_xsdt());
    }

    #[test]
    fn a_table_shorter_than_its_header_is_not_a_table() {
        assert!(parse_header(&[0u8; 12]).is_none());
    }

    #[test]
    fn a_table_whose_length_overruns_its_buffer_is_refused() {
        // A header that claims a kilobyte while only forty bytes arrived: the
        // length is what every later reader strides by, so it cannot be
        // clamped into meaning.
        let mut bytes = table(b"MCFG", &[0u8; 4]);
        bytes[4..8].copy_from_slice(&1024u32.to_le_bytes());
        assert!(parse_header(&bytes).is_none());
        assert!(parse_mcfg(&bytes).is_empty());
    }

    #[test]
    fn the_windows_an_mcfg_names_are_read_in_order() {
        let mut body = vec![0u8; 32];
        body[0..16].copy_from_slice(&mcfg_entry(0xb000_0000, 0, 0, 255));
        body[16..32].copy_from_slice(&mcfg_entry(0xc000_0000, 1, 0, 15));
        let bytes = table(MCFG_SIGNATURE, &body);
        let ranges = parse_mcfg(&bytes);
        assert_eq!(ranges.len(), 2);
        assert_eq!(
            ranges[0],
            EcamRange {
                base: 0xb000_0000,
                segment: 0,
                start_bus: 0,
                end_bus: 255
            }
        );
        assert_eq!(ranges[1].segment, 1);
        assert_eq!(ranges[1].end_bus, 15);
    }

    #[test]
    fn an_mcfg_with_no_room_for_an_entry_names_no_window() {
        let bytes = table(MCFG_SIGNATURE, &[0u8; 8]);
        assert!(parse_mcfg(&bytes).is_empty());
    }

    #[test]
    fn another_tables_signature_is_not_read_as_mcfg() {
        let bytes = table(b"APIC", &mcfg_entry(0xb000_0000, 0, 0, 0xff));
        assert!(parse_mcfg(&bytes).is_empty());
    }

    #[test]
    fn a_window_spans_one_mebibyte_a_bus_and_one_page_a_function() {
        let range = EcamRange {
            base: 0xb000_0000,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
        };
        assert_eq!(range.len(), 256 * ECAM_BUS_BYTES);
        assert_eq!(range.offset(0, 0, 0), Some(0));
        assert_eq!(range.offset(0, 1, 0), Some(0x8000));
        assert_eq!(range.offset(1, 0, 0), Some(ECAM_BUS_BYTES));
        assert_eq!(range.offset(2, 3, 4), Some(2 * ECAM_BUS_BYTES + 3 * 0x8000 + 4 * 0x1000));
        // A bus the window does not cover has no offset inside it, and neither
        // does a device or function number the layout cannot spell.
        assert_eq!(range.offset(255, 31, 7).map(|_| ()), Some(()));
        assert_eq!(range.offset(0, 32, 0), None);
        assert_eq!(range.offset(0, 0, 8), None);
        let narrow = EcamRange {
            base: 0,
            segment: 0,
            start_bus: 4,
            end_bus: 7,
        };
        assert_eq!(narrow.offset(3, 0, 0), None, "below the window");
        assert_eq!(narrow.offset(8, 0, 0), None, "above the window");
        assert_eq!(narrow.offset(4, 0, 0), Some(0));
    }

    #[test]
    fn an_empty_window_is_not_one_to_map() {
        let range = EcamRange {
            base: 0,
            segment: 0,
            start_bus: 8,
            end_bus: 3,
        };
        assert!(range.is_empty());
        assert_eq!(range.offset(4, 0, 0), None);
    }

    #[test]
    fn a_root_table_names_eight_byte_addresses_when_it_is_an_xsdt() {
        let mut body = vec![0u8; 24];
        body[0..8].copy_from_slice(&0x1000u64.to_le_bytes());
        body[8..16].copy_from_slice(&0x1_0000_0000_0000u64.to_le_bytes());
        body[16..24].copy_from_slice(&0x3000u64.to_le_bytes());
        assert_eq!(table_entries(&body, true), vec![0x1000, 0x1_0000_0000_0000, 0x3000]);
        // The 32-bit root is the same body read four bytes at a time, which is
        // why the width is a parameter and not a second function - and why the
        // width *matters*: the address above 4 GiB is not representable there,
        // and a reader that walked an XSDT as an RSDT would split one address
        // into two entries and see a table nobody named (`0x1_0000` is the top
        // half of the wide address, not a table).
        assert_eq!(
            table_entries(&body, false),
            vec![0x1000, 0, 0, 0x1_0000, 0x3000, 0]
        );
    }

    #[test]
    fn a_trailing_partial_entry_is_not_read_past_the_end() {
        let body = vec![0u8; 10];
        assert_eq!(table_entries(&body, true).len(), 1);
        assert_eq!(table_entries(&body, false).len(), 2);
    }
}
