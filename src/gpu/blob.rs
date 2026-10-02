//! The two DRM blobs a KMS driver *publishes as bytes*: an EDID, and the
//! `IN_FORMATS` list of which (format, modifier) pairs a plane will scan out.
//!
//! They are here, ungated, rather than inside `drivers/drm.rs`, for the reason
//! `AGENTS.md` §4 gives: they are pure encoders, and a host unit test can check
//! the very bytes a real reader parses. `drivers/drm.rs` holds the card's state
//! and talks to the GPU, so nothing in it can be exercised off the machine - and
//! an EDID one field out of place is a display no compositor can name, which is
//! the kind of defect that only shows up as "Unknown" in somebody's settings
//! panel.
//!
//! Both formats are Linux's, and both were read off a real card before they were
//! written: `modetest -c -p` and a direct-ioctl probe against the AMD device on
//! the development host (Linux 7.2.7). What that measurement settled, rather
//! than a specification:
//!
//!   * a connector reports `mm_width`/`mm_height` as the EDID's *centimetre*
//!     fields times ten - 30 and 19 gave 300x190 - and `0x0` when the connector
//!     has no EDID at all;
//!   * `subpixel` was **0** on that connector, so 0 is the unknown value and
//!     nothing needs inventing for it;
//!   * the chroma words are ten-bit `coord * 1024`, which the panel's own bytes
//!     check out to the least significant bit;
//!   * `IN_FORMATS` is a 24-byte header, a `u32` format array at offset 24, a
//!     modifier array at `align(24 + 4 * count_formats, 8)` - 104 for the 20
//!     formats that card lists - 24 bytes per modifier, and each modifier's
//!     `formats` word is a bitmask over the format array from its `offset`.
//!
//! The one thing here that is a *decision* rather than a measurement is
//! [`size_mm`]: a window on a host has no physical size, so the millimetres are
//! what a display of those pixels would be at a nominal 96 dpi. That is the same
//! number a compositor computes for itself when a display reports no size at
//! all, which is why it is better than the zero it replaces - and it says so in
//! `PLAN.md` §7.4 rather than passing itself off as a measurement.

use alloc::vec::Vec;

/// One EDID block, base or extension.
pub const EDID_BLOCK: usize = 128;

/// `DRM_FORMAT_MOD_LINEAR`: the only modifier a dumb buffer here is ever in.
pub const FORMAT_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`, which is `fourcc_mod_code(NONE, DRM_FORMAT_RESERVED)`
/// - the reserved value of the one vendor namespace, not a small number.
pub const FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
/// `FORMAT_BLOB_CURRENT`: the only `version` the `IN_FORMATS` format has.
pub const FORMAT_BLOB_CURRENT: u32 = 1;
/// `DRM_MODE_SUBPIXEL_UNKNOWN`, which is 0 - measured, not read.
pub const SUBPIXEL_UNKNOWN: u32 = 0;

/// `sizeof(struct drm_format_modifier_blob)`.
const BLOB_HEADER: usize = 24;
/// `sizeof(struct drm_format_modifier)`.
const MODIFIER_SIZE: usize = 24;

/// Where the four descriptors start, and how long each is.
const DESCRIPTOR_AT: usize = 54;
const DESCRIPTOR_SIZE: usize = 18;

/// The nominal density the physical size is derived from. A host window has no
/// size in millimetres, and this is the number a compositor falls back to.
const NOMINAL_DPI: u32 = 96;

/// The manufacturer code this card reports, in EDID's five-bits-per-letter form.
const MFG_MPO: u16 = 0x360f;
/// The product code, and the serial number, both fixed - this is one synthesized
/// display and not a fleet.
const PRODUCT: u16 = 0x0001;
const SERIAL: u32 = 1;

/// The vertical refresh every mode here reports, in Hz. One number for one
/// synthesized timing, which is what `ADDFB2` and the mode list already use.
const REFRESH: u32 = 60;

/// The blanking the measured panel's detailed timing uses, applied to whatever
/// size the host window is. Nothing reads these back: the EDID exists so a
/// compositor can name the display and size it, and the *mode* it scans out
/// comes from `GETCONNECTOR`, whose entry is a rectangle of the same active
/// size.
const HBLANK: u32 = 160;
const VBLANK: u32 = 60;
const HSYNC_OFFSET: u32 = 48;
const HSYNC_WIDTH: u32 = 32;
const VSYNC_OFFSET: u32 = 3;
const VSYNC_WIDTH: u32 = 6;

/// The sRGB primaries in EDID's ten-bit words, read off the measured panel's own
/// bytes: red 655/338, green 307/614, blue 154/61, white 321/337 - which is
/// sRGB (0.640/0.330, 0.300/0.600, 0.150/0.060, 0.3127/0.3290) to within one
/// step of a ten-bit field. In order, `Rx Ry Gx Gy Bx By Wx Wy`.
const CHROMATICITY: [u16; 8] = [655, 338, 307, 614, 154, 61, 321, 337];

/// The video-input word: digital, eight bits per primary, DisplayPort. This
/// card has no physical connector, and the field has no "none" value - bit 7
/// alone decides whether a reader treats the display as digital, so it has to
/// say something, and DisplayPort is what a virtual display over a GPU is.
const VIDEO_INPUT: u8 = 0xa5;

/// The feature word: the preferred timing is the native one (bit 1) and the
/// chromaticity above is sRGB (bit 2). Not bit 0, "continuous frequency": there
/// is exactly one timing here.
const FEATURES: u8 = 0x06;

/// The millimetres a display of `width` x `height` pixels would have at
/// [`NOMINAL_DPI`]. Derived from [`size_cm`] so that a connector's `mm_width` is
/// always exactly ten times the EDID's own field - which is the relation the
/// measurement found between them.
pub fn size_mm(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = size_cm(width, height);
    (w as u32 * 10, h as u32 * 10)
}

/// The centimetres the EDID's own size fields carry, rounded to the nearest
/// whole centimetre as the format requires.
fn size_cm(width: u32, height: u32) -> (u8, u8) {
    // Pixels to centimetres as 2.54 cm to the inch: `pixels * 254 / (100 * dpi)`,
    // rounded to the nearest by adding half a divisor.
    let dpi = u64::from(NOMINAL_DPI);
    let cm = |pixels: u32| -> u8 {
        let value = (u64::from(pixels) * 254 + 50 * dpi) / (100 * dpi);
        value.min(u8::MAX as u64) as u8
    };
    (cm(width), cm(height))
}

/// Build a 128-byte EDID 1.4 base block for a display of `width` x `height`
/// pixels, named `name`.
///
/// The block is a complete answer to "what monitor is this": the vendor and
/// product codes, the physical size, the sRGB primaries, one detailed timing
/// for the mode being advertised, a name string, the rate range that timing
/// falls in, and a valid checksum. No extension blocks: this display has no
/// CTA-861 data (no audio, no additional formats) to put in one, and an EDID
/// that claims an extension it does not have is worse than one that stops.
pub fn edid_base_block(width: u32, height: u32, name: &str) -> [u8; EDID_BLOCK] {
    let mut block = [0u8; EDID_BLOCK];
    // The header every reader checks before it reads anything else.
    block[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    block[8..10].copy_from_slice(&MFG_MPO.to_be_bytes());
    block[10..12].copy_from_slice(&PRODUCT.to_le_bytes());
    block[12..16].copy_from_slice(&SERIAL.to_le_bytes());
    // Week and year of manufacture. The year is stored as an offset from 1990,
    // and a reader that finds 0 there reports "1990" - so the epoch itself is
    // what this reports, there being no better date for a window.
    block[16] = 1;
    block[17] = 0;
    block[18] = 1; // EDID version 1
    block[19] = 4; // revision 4
    block[20] = VIDEO_INPUT;
    let (cm_w, cm_h) = size_cm(width, height);
    block[21] = cm_w;
    block[22] = cm_h;
    // Gamma 2.2, in the format's (gamma * 100) - 100 encoding.
    block[23] = 0x78;
    block[24] = FEATURES;
    // The chromaticity words, packed low bits together and high bits apart -
    // the layout the panel's own bytes check out against.
    block[25] = ((CHROMATICITY[0] & 3) << 6) as u8
        | ((CHROMATICITY[1] & 3) << 4) as u8
        | ((CHROMATICITY[2] & 3) << 2) as u8
        | (CHROMATICITY[3] & 3) as u8;
    block[26] = ((CHROMATICITY[4] & 3) << 6) as u8
        | ((CHROMATICITY[5] & 3) << 4) as u8
        | ((CHROMATICITY[6] & 3) << 2) as u8
        | (CHROMATICITY[7] & 3) as u8;
    for (index, word) in CHROMATICITY.iter().enumerate() {
        block[27 + index] = (word >> 2) as u8;
    }
    // No established timings, and every standard-timing slot left unused - the
    // encoding for "unused" is 0x01 0x01, which is not zero and not a timing.
    for slot in 0..8 {
        block[38 + slot * 2] = 0x01;
        block[39 + slot * 2] = 0x01;
    }
    // Four descriptors: the detailed timing, the rate range that timing falls
    // in, the monitor's name, and the dummy a display with nothing more to say
    // carries. That is the measured panel's own order, and the order matters
    // only in that a reader walks all four - but matching a real panel's is one
    // fewer thing to be wrong about.
    let timing = detailed_timing(width, height);
    block[DESCRIPTOR_AT..DESCRIPTOR_AT + DESCRIPTOR_SIZE].copy_from_slice(&timing);
    let limits = rate_range_descriptor(width, height);
    block[DESCRIPTOR_AT + DESCRIPTOR_SIZE..DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE]
        .copy_from_slice(&limits);
    let name_descriptor = descriptor(0xfc, name);
    block[DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE..DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE]
        .copy_from_slice(&name_descriptor);
    // The dummy descriptor: the tag 0x10 at byte three of the block, and zeros
    // after it.
    block[DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE + 3] = 0x10;
    block[126] = 0; // no extension blocks
    block[127] = checksum(&block);
    block
}

/// The sum of the block's first 127 bytes, negated - the byte that makes the
/// whole block add to zero modulo 256, which is the only self-check an EDID has.
fn checksum(block: &[u8; EDID_BLOCK]) -> u8 {
    let sum: u8 = block[..EDID_BLOCK - 1]
        .iter()
        .fold(0u8, |acc, byte| acc.wrapping_add(*byte));
    0u8.wrapping_sub(sum)
}

/// A string descriptor: the tag, up to thirteen printable ASCII bytes, a line
/// feed, and spaces to the end. A name longer than the field holds is cut - the
/// field is the whole of what a reader can be told.
fn descriptor(tag: u8, text: &str) -> [u8; DESCRIPTOR_SIZE] {
    let mut out = [0x20u8; DESCRIPTOR_SIZE];
    out[0] = 0x00;
    out[1] = 0x00;
    out[2] = 0x00;
    out[3] = tag;
    out[4] = 0x00;
    let mut written = 0;
    for (slot, byte) in out[5..DESCRIPTOR_SIZE].iter_mut().zip(text.bytes()) {
        *slot = if (0x20..0x7f).contains(&byte) {
            byte
        } else {
            b'?'
        };
        written += 1;
    }
    // The terminator, in the slot after the text - and only when there is room
    // for one: a name that fills all thirteen bytes is its own terminator, which
    // is what the format allows.
    if written < DESCRIPTOR_SIZE - 5 {
        out[5 + written] = 0x0a;
    }
    out
}

/// The detailed timing descriptor for `width` x `height` at [`REFRESH`], with
/// the blanking the measured panel uses and a pixel clock that follows from the
/// totals rather than being carried in from anywhere.
fn detailed_timing(width: u32, height: u32) -> [u8; DESCRIPTOR_SIZE] {
    let htotal = width + HBLANK;
    let vtotal = height + VBLANK;
    // In units of 10 kHz, which is the only unit the field has.
    let clock = ((u64::from(htotal) * u64::from(vtotal) * u64::from(REFRESH) + 5_000) / 10_000)
        .min(u64::from(u16::MAX)) as u16;
    let (mm_w, mm_h) = size_mm(width, height);
    let mut out = [0u8; DESCRIPTOR_SIZE];
    out[0..2].copy_from_slice(&clock.to_le_bytes());
    out[2] = width as u8;
    out[3] = HBLANK as u8;
    out[4] = (((width >> 8) as u8) << 4) | ((HBLANK >> 8) as u8 & 0x0f);
    out[5] = height as u8;
    out[6] = VBLANK as u8;
    out[7] = (((height >> 8) as u8) << 4) | ((VBLANK >> 8) as u8 & 0x0f);
    out[8] = HSYNC_OFFSET as u8;
    out[9] = HSYNC_WIDTH as u8;
    out[10] = ((VSYNC_OFFSET as u8 & 0x0f) << 4) | (VSYNC_WIDTH as u8 & 0x0f);
    out[11] = (((HSYNC_OFFSET >> 8) as u8 & 3) << 6)
        | (((HSYNC_WIDTH >> 8) as u8 & 3) << 4)
        | (((VSYNC_OFFSET >> 4) as u8 & 3) << 2)
        | ((VSYNC_WIDTH >> 4) as u8 & 3);
    // The image size in millimetres, the same number the base block reports in
    // centimetres - so the two agree, as the measured pair does.
    out[12] = mm_w as u8;
    out[13] = mm_h as u8;
    out[14] = (((mm_w >> 8) as u8 & 0x0f) << 4) | ((mm_h >> 8) as u8 & 0x0f);
    // Borders: none. And the sync word the measured panel carries: digital
    // separate sync, positive horizontal and negative vertical - 0x1a, the
    // polarity `modetest` prints for that panel's own mode as
    // "phsync, nvsync". The card's mode list has no flags at all (`write_mode`
    // writes zero there), so nothing contradicts it.
    out[15] = 0;
    out[16] = 0;
    out[17] = 0x1a;
    out
}

/// The rate-range descriptor: the vertical rates, horizontal rates and the
/// ceiling on the pixel clock that this display's one timing falls inside.
///
/// The floor and the ceiling are the timing's own rates rather than a wider
/// range, because this display has exactly one timing and a range that claimed
/// more would be a mode list it does not have.
fn rate_range_descriptor(width: u32, height: u32) -> [u8; DESCRIPTOR_SIZE] {
    let htotal = width + HBLANK;
    let vtotal = height + VBLANK;
    let clock_10khz = (u64::from(htotal) * u64::from(vtotal) * u64::from(REFRESH) + 5_000) / 10_000;
    let hrate_khz = (u64::from(htotal) * u64::from(REFRESH) + 500) / 1000;
    // The field is "rounded up to the next 10 MHz", so a clock of 157.25 MHz
    // reports 16 (160 MHz) and never a ceiling below the timing it describes.
    let clock_10mhz = clock_10khz.div_ceil(1000);
    let mut out = [0x20u8; DESCRIPTOR_SIZE];
    // Bytes nought and one are what tells a reader whether this is a detailed
    // timing or a display descriptor: a timing starts with a pixel clock, and a
    // descriptor starts with two zeroes. Leaving them at the padding byte makes
    // `edid-decode` read the rate range as a second 0x1340 timing, which it
    // printed as "0x1340 110.7 Hz ... analog composite" before this was fixed.
    out[0] = 0x00;
    out[1] = 0x00;
    out[2] = 0x00;
    out[3] = 0xfd;
    out[4] = 0x00;
    out[5] = REFRESH.min(255) as u8;
    out[6] = REFRESH.min(255) as u8;
    out[7] = hrate_khz.min(255) as u8;
    out[8] = hrate_khz.min(255) as u8;
    out[9] = clock_10mhz.min(255) as u8;
    // "No timing information" (0x01), not "default GTF" (0x00): a reader that
    // reads GTF here would derive a mode list from the rate range, and this
    // display's modes come from the detailed timing above and nowhere else.
    out[10] = 0x01;
    out[11] = 0x0a;
    out
}

/// The `IN_FORMATS` blob for a plane that scans out `formats` in `modifier`.
///
/// One modifier entry covering every format, because this card has one: a dumb
/// buffer is always linear, so a caller asking whether a format supports some
/// other modifier is told no by the format's absence from that entry's bitmask.
/// `formats` is the plane's own fourcc list, in the order `GETPLANE` reports it -
/// the bitmask is an index into *that* array, so the two have to be built from
/// one list to stay in step.
pub fn in_formats(formats: &[u32], modifier: u64) -> Vec<u8> {
    let count = formats.len().min(64);
    let formats_offset = BLOB_HEADER;
    let modifiers_offset = align8(BLOB_HEADER + 4 * count);
    let mut blob = alloc::vec![0u8; modifiers_offset + MODIFIER_SIZE];
    put_u32(&mut blob, 0, FORMAT_BLOB_CURRENT);
    put_u32(&mut blob, 4, 0);
    put_u32(&mut blob, 8, count as u32);
    put_u32(&mut blob, 12, formats_offset as u32);
    put_u32(&mut blob, 16, 1);
    put_u32(&mut blob, 20, modifiers_offset as u32);
    for (index, format) in formats.iter().take(count).enumerate() {
        put_u32(&mut blob, formats_offset + 4 * index, *format);
    }
    // Which of the format array's entries this modifier applies to: all of them,
    // as bits from `offset` 0.
    let bitmask = if count == 64 {
        u64::MAX
    } else {
        (1u64 << count) - 1
    };
    put_u64(&mut blob, modifiers_offset, bitmask);
    put_u32(&mut blob, modifiers_offset + 8, 0); // offset into the format array
    put_u32(&mut blob, modifiers_offset + 12, 0); // pad
    put_u64(&mut blob, modifiers_offset + 16, modifier);
    blob
}

/// The next multiple of eight, which is where a reader that follows
/// `modifiers_offset` may assume the array starts.
fn align8(value: usize) -> usize {
    value.next_multiple_of(8)
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(all(test, not(target_os = "none")))]
mod tests {
    use super::*;

    /// Read the fields back out of a block the way a reader does - a decoder
    /// written from the same specification as the encoder is what makes the
    /// test a test rather than a restatement, so where the two could share a
    /// helper they deliberately do not.
    struct Decoded {
        manufacturer: alloc::string::String,
        product: u16,
        serial: u32,
        cm: (u8, u8),
        gamma: u8,
        features: u8,
        chromaticity: [u16; 8],
        name: alloc::string::String,
        timing: Option<(u16, u16, u16, u16, u16, u32)>,
        extensions: u8,
    }

    fn decode(block: &[u8; EDID_BLOCK]) -> Decoded {
        let word = u16::from_be_bytes([block[8], block[9]]);
        let letter = |shift: u16| ((word >> shift) & 0x1f) as u8 + b'A' - 1;
        let manufacturer =
            alloc::string::String::from_utf8(alloc::vec![letter(10), letter(5), letter(0)])
                .unwrap();

        let mut chromaticity = [0u16; 8];
        chromaticity[0] = (u16::from(block[27]) << 2) | u16::from(block[25] >> 6);
        chromaticity[1] = (u16::from(block[28]) << 2) | u16::from((block[25] >> 4) & 3);
        chromaticity[2] = (u16::from(block[29]) << 2) | u16::from((block[25] >> 2) & 3);
        chromaticity[3] = (u16::from(block[30]) << 2) | u16::from(block[25] & 3);
        chromaticity[4] = (u16::from(block[31]) << 2) | u16::from(block[26] >> 6);
        chromaticity[5] = (u16::from(block[32]) << 2) | u16::from((block[26] >> 4) & 3);
        chromaticity[6] = (u16::from(block[33]) << 2) | u16::from((block[26] >> 2) & 3);
        chromaticity[7] = (u16::from(block[34]) << 2) | u16::from(block[26] & 3);

        let mut name = alloc::string::String::new();
        for byte in
            &block[DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE + 5..DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE]
        {
            if *byte == 0x0a {
                break;
            }
            name.push(*byte as char);
        }

        let dtd = &block[DESCRIPTOR_AT..DESCRIPTOR_AT + DESCRIPTOR_SIZE];
        let timing = if dtd[0] == 0 && dtd[1] == 0 {
            None
        } else {
            let clock = u16::from_le_bytes([dtd[0], dtd[1]]);
            let hactive = u16::from(dtd[2]) | (u16::from(dtd[4] >> 4) << 8);
            let hblank = u16::from(dtd[3]) | (u16::from(dtd[4] & 0x0f) << 8);
            let vactive = u16::from(dtd[5]) | (u16::from(dtd[7] >> 4) << 8);
            let vblank = u16::from(dtd[6]) | (u16::from(dtd[7] & 0x0f) << 8);
            // The refresh the clock and the totals imply.
            let refresh = (u32::from(clock) * 10_000)
                / (u32::from(hactive + hblank) * u32::from(vactive + vblank));
            Some((clock, hactive, hblank, vactive, vblank, refresh))
        };

        Decoded {
            manufacturer,
            product: u16::from_le_bytes([block[10], block[11]]),
            serial: u32::from_le_bytes([block[12], block[13], block[14], block[15]]),
            cm: (block[21], block[22]),
            gamma: block[23],
            features: block[24],
            chromaticity,
            name,
            timing,
            extensions: block[126],
        }
    }

    #[test]
    fn the_block_is_one_an_edid_reader_would_accept() {
        let block = edid_base_block(1280, 800, "MPOS Virtual");
        assert_eq!(block.len(), EDID_BLOCK);
        assert_eq!(
            &block[..8],
            &[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00],
            "the header is the first thing a reader checks"
        );
        assert_eq!(
            block.iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0,
            "and the checksum is the second: the block adds to zero"
        );
        assert_eq!((block[18], block[19]), (1, 4), "EDID 1.4");
        assert_eq!(block[126], 0, "no extension block is claimed");
        assert_ne!(block[20] & 0x80, 0, "a digital input, not an analog one");
    }

    #[test]
    fn changing_one_byte_breaks_the_checksum() {
        let mut block = edid_base_block(1024, 768, "x");
        block[54] ^= 0x01;
        assert_ne!(
            block.iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0,
            "the checksum is a real check and not a copied constant"
        );
    }

    #[test]
    fn a_reader_gets_the_name_and_the_size_back() {
        let block = edid_base_block(1280, 800, "MPOS Virtual");
        let decoded = decode(&block);
        assert_eq!(decoded.manufacturer, "MPO");
        assert_eq!(decoded.product, PRODUCT);
        assert_eq!(decoded.serial, SERIAL);
        assert_eq!(decoded.name, "MPOS Virtual");
        assert_eq!(decoded.gamma, 0x78, "gamma 2.2 in its own encoding");
        assert_eq!(
            decoded.features & 0x02,
            0x02,
            "the preferred timing is the native one"
        );
        assert_eq!(decoded.cm, size_cm(1280, 800));
        // And the connector's millimetres are exactly ten times the EDID's
        // centimetres, which is the relation the measurement found.
        assert_eq!(size_mm(1280, 800), (34 * 10, 21 * 10));
    }

    #[test]
    fn the_chromaticity_words_are_the_primaries_a_panel_reports() {
        let decoded = decode(&edid_base_block(1920, 1080, "n"));
        assert_eq!(decoded.chromaticity, CHROMATICITY);
        // sRGB, to within one unit of the ten-bit field.
        let as_float = |value: u16| f64::from(value) / 1024.0;
        assert!((as_float(decoded.chromaticity[0]) - 0.640).abs() < 0.002);
        assert!((as_float(decoded.chromaticity[1]) - 0.330).abs() < 0.002);
        assert!((as_float(decoded.chromaticity[6]) - 0.3127).abs() < 0.002);
    }

    #[test]
    fn the_preferred_timing_is_the_mode_and_says_sixty_hertz() {
        let block = edid_base_block(1920, 1200, "MPOS");
        let (clock, hactive, hblank, vactive, vblank, refresh) =
            decode(&block).timing.expect("a detailed timing");
        assert_eq!(
            (hactive, vactive),
            (1920, 1200),
            "the active size is the mode"
        );
        assert_eq!((hblank, vblank), (HBLANK as u16, VBLANK as u16));
        assert_eq!(refresh, REFRESH);
        // The measured panel's own 1920x1200 timing is 2080x1260 at 15725 -
        // the same totals and the same clock, which is the whole point of
        // taking the blanking from it.
        assert_eq!(clock, 15725, "2080 x 1260 x 60, rounded to 10 kHz");
        assert_eq!(block[DESCRIPTOR_AT + 17], 0x1a, "digital separate, +h / -v");
    }

    #[test]
    fn the_rate_range_has_a_ceiling_no_lower_than_the_timing() {
        let block = edid_base_block(1280, 800, "MPOS");
        let limits = &block[DESCRIPTOR_AT + DESCRIPTOR_SIZE..DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE];
        assert_eq!(limits[3], 0xfd, "the range-limits descriptor tag");
        assert_eq!(limits[5], REFRESH as u8, "the floor is the mode's own rate");
        assert_eq!(limits[6], REFRESH as u8, "and so is the ceiling");
        let (clock, _, _, _, _, _) = decode(&block).timing.unwrap();
        let ceiling_10mhz = u32::from(limits[9]) * 1000;
        assert!(
            ceiling_10mhz >= u32::from(clock),
            "the pixel-clock ceiling covers the timing it describes: {ceiling_10mhz} vs {clock}"
        );
    }

    #[test]
    fn a_name_too_long_for_the_field_is_cut_and_still_a_string() {
        let long = "a-very-long-monitor-name-indeed";
        let block = edid_base_block(640, 480, long);
        let decoded = decode(&block);
        assert_eq!(decoded.name.len(), 13, "thirteen printable bytes fit");
        assert!(long.starts_with(&decoded.name));
        // Every byte of the field is printable ASCII or the terminator, so a
        // reader that prints it cannot walk into anything else.
        let field =
            &block[DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE + 5..DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE];
        assert!(field.iter().all(|b| (0x20..0x7f).contains(b) || *b == 0x0a));
    }

    #[test]
    fn a_short_name_is_terminated_and_padded() {
        let block = edid_base_block(640, 480, "MPOS");
        let field =
            &block[DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE..DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE];
        assert_eq!(&field[..5], &[0x00, 0x00, 0x00, 0xfc, 0x00], "the tag");
        assert_eq!(&field[5..9], b"MPOS");
        assert_eq!(field[9], 0x0a, "the terminator follows the text");
        assert!(
            field[10..].iter().all(|b| *b == 0x20),
            "and the rest is the space padding the format asks for"
        );
    }

    /// The layout the measured card's own blob has: a 24-byte header, the format
    /// array at 24, the modifier array at `align(24 + 4n, 8)`, and one 24-byte
    /// entry per modifier.
    #[test]
    fn the_format_blob_lays_its_arrays_out_where_a_reader_looks() {
        let formats = [0x3432_5258u32, 0x3432_5241, 0x3432_4152, 0x3432_4241];
        let blob = in_formats(&formats, FORMAT_MOD_LINEAR);
        let u32_at = |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(blob[at..at + 8].try_into().unwrap());

        assert_eq!(u32_at(0), FORMAT_BLOB_CURRENT, "version 1");
        assert_eq!(u32_at(4), 0, "flags");
        assert_eq!(u32_at(8), formats.len() as u32);
        assert_eq!(
            u32_at(12),
            24,
            "the format array follows the 24-byte header"
        );
        assert_eq!(u32_at(16), 1, "one modifier: linear, and that is all");
        assert_eq!(
            u32_at(20),
            40,
            "24 + 4 * 4, which is already a multiple of eight"
        );
        assert_eq!(blob.len(), 24 + 4 * formats.len() + 24);

        for (index, format) in formats.iter().enumerate() {
            assert_eq!(u32_at(24 + 4 * index), *format);
        }
        let modifiers = u32_at(20) as usize;
        assert_eq!(
            u64_at(modifiers),
            0b1111,
            "the bitmask covers every format in the array, from offset 0"
        );
        assert_eq!(u32_at(modifiers + 8), 0, "offset into the format array");
        assert_eq!(u32_at(modifiers + 12), 0, "pad");
        assert_eq!(u64_at(modifiers + 16), FORMAT_MOD_LINEAR);
    }

    #[test]
    fn a_format_list_that_is_not_a_multiple_of_eight_aligns_its_modifiers() {
        let blob = in_formats(
            &[0x3432_5258u32, 0x3432_5241, 0x3432_4152],
            FORMAT_MOD_LINEAR,
        );
        let u32_at = |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
        // 24 + 12 = 36, which is not a multiple of eight: the modifier array
        // starts at 40, and the four bytes before it are padding.
        assert_eq!(u32_at(20), 40);
        assert_eq!(blob.len(), 40 + MODIFIER_SIZE);
        assert_eq!(&blob[36..40], &[0, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(blob[40..48].try_into().unwrap()),
            0b111,
            "and the bitmask counts the formats that are really there"
        );
    }

    #[test]
    fn the_invalid_modifier_is_the_reserved_vendor_value() {
        // A reader uses this to mean "no modifier", so it must not collide with
        // the zero that means linear.
        assert_eq!(FORMAT_MOD_INVALID, 0x00ff_ffff_ffff_ffff);
        assert_ne!(FORMAT_MOD_INVALID, FORMAT_MOD_LINEAR);
    }

    /// What distinguishes a detailed timing from a display descriptor is its
    /// first two bytes: a timing starts with a pixel clock, a descriptor with two
    /// zeroes. Leaving them at the space padding is a real bug, and one a reader
    /// found rather than a test: `edid-decode` parsed this block's rate range as
    /// a second timing - "0x1340 110.7 Hz ... analog composite" - until they were
    /// zeroed.
    #[test]
    fn only_the_first_descriptor_is_a_timing() {
        let block = edid_base_block(1280, 800, "MPOS Virtual");
        for index in 1..4 {
            let at = DESCRIPTOR_AT + index * DESCRIPTOR_SIZE;
            assert_eq!(
                (block[at], block[at + 1]),
                (0, 0),
                "descriptor {index} must open like a descriptor, not like a clock"
            );
        }
        assert_ne!(
            (block[DESCRIPTOR_AT], block[DESCRIPTOR_AT + 1]),
            (0, 0),
            "and the first must open like a timing"
        );
        assert_eq!(
            block[DESCRIPTOR_AT + 2 * DESCRIPTOR_SIZE + 3],
            0xfc,
            "the name tag"
        );
        assert_eq!(
            block[DESCRIPTOR_AT + 3 * DESCRIPTOR_SIZE + 3],
            0x10,
            "the dummy tag"
        );
    }

    #[test]
    fn the_timing_tables_are_empty_rather_than_absent() {
        let block = edid_base_block(1024, 768, "MPOS");
        assert_eq!(&block[35..38], &[0, 0, 0], "no established timings");
        // Eight standard-timing slots, each the two-byte "unused" marker.
        for slot in 0..8 {
            assert_eq!(
                (block[38 + slot * 2], block[39 + slot * 2]),
                (0x01, 0x01),
                "standard-timing slot {slot} is unused, not zeroed"
            );
        }
    }

    /// The decoder above, run over the bytes a *real* panel reports - the base
    /// block `modetest` printed from the development host's own display. This is
    /// what keeps the decoder honest: an encoder and a decoder written from one
    /// reading of a format agree with each other whether or not either is right,
    /// and these bytes are the format's own answer for the fields the
    /// measurement quoted (30 x 19 cm, "TMA", gamma 2.2, one extension block).
    #[test]
    fn the_decoder_reads_a_real_panels_block() {
        let mut block = [0u8; EDID_BLOCK];
        // Transcribed line for line from what `modetest` printed for that
        // panel, sixteen bytes to a line.
        let hex = b"\
            00ffffffffffff0051a1130800000000\
            2e220104a51e137807ee95a3544c9926\
            0f505400000001010101010101010101\
            0101010101016d3d80a070b03c403020\
            36002dbc1000001a000000fd00283c4c\
            4c10010a202020202020000000fc0054\
            4d3134305644585030320a2000000010\
            00000000000000000000000000000123";
        assert_eq!(hex.len(), EDID_BLOCK * 2, "eight lines of sixteen bytes");
        for (index, slot) in block.iter_mut().enumerate() {
            let pair = &hex[index * 2..index * 2 + 2];
            let digit = |b: u8| (b as char).to_digit(16).unwrap() as u8;
            *slot = (digit(pair[0]) << 4) | digit(pair[1]);
        }
        assert_eq!(
            block.iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0,
            "the panel's own block is checksum-clean, so the fixture is a block"
        );
        let decoded = decode(&block);
        assert_eq!(decoded.manufacturer, "TMA");
        assert_eq!(
            decoded.cm,
            (30, 19),
            "the size the connector reported as 300x190"
        );
        assert_eq!(decoded.gamma, 0x78);
        assert_eq!(decoded.extensions, 1, "this panel has a CTA extension");
        assert_eq!(decoded.name, "TM140VDXP02");
        assert_eq!(decoded.chromaticity, CHROMATICITY, "sRGB, to the last bit");
        // And its detailed timing, which is where this card's blanking and sync
        // word come from.
        let (clock, hactive, hblank, vactive, vblank, refresh) = decoded.timing.unwrap();
        assert_eq!(
            (clock, hactive, hblank, vactive, vblank, refresh),
            (15725, 1920, 160, 1200, 60, 60)
        );
        assert_eq!(block[DESCRIPTOR_AT + 17], 0x1a, "its sync word too");
    }
}
