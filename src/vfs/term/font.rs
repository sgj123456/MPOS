//! The console's bitmap font: a public-domain 8x16 VGA cell.
//!
//! The bytes are the glyphs themselves, one byte per row, most-significant bit
//! the left pixel, sixteen rows per code point - converted once from the Linux
//! `default8x16` console font and committed as a blob so the table is a data
//! fact, not something retyped (and mistranscribed) in source. A known-glyph
//! test below pins a few patterns so a corrupt blob cannot ship silently.

/// The cell the console draws in, in pixels.
pub const CELL_W: usize = 8;
pub const CELL_H: usize = 16;

/// One byte per row x 16 rows x 256 code points.
static GLYPHS: &[u8; 256 * CELL_H] = include_bytes!("vga8x16.bin");

/// The 16 rows of one code point, MSB = the left pixel of the row.
pub fn glyph(ch: u8) -> [u8; CELL_H] {
    let mut rows = [0u8; CELL_H];
    rows.copy_from_slice(&GLYPHS[ch as usize * CELL_H..(ch as usize + 1) * CELL_H]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real font has a real `A` and a truly empty space; a blob that landed
    /// as zeroes or as a shifted copy would fail one of these and not the other.
    #[test]
    fn space_is_empty_and_is_not_the_only_empty_glyph() {
        assert_eq!(glyph(b' '), [0u8; CELL_H]);
        assert_ne!(glyph(b'A'), [0u8; CELL_H]);
    }

    #[test]
    fn capital_a_is_the_vga_shape() {
        // The rows we are sure of, MSB = left pixel (the blob renders `A`
        // left-aligned in the cell): empty top, a single centred apex, the
        // full crossbar, and an empty descender row.
        let a = glyph(b'A');
        assert_eq!(a[0], 0);
        assert_eq!(a[2], 0b0001_0000); // apex
        assert_eq!(a[7], 0b1111_1110); // crossbar
        assert_eq!(a[15], 0); // descender row empty
    }
}
