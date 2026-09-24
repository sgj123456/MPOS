//! Blit a terminal [`Grid`] into a BGRA framebuffer.
//!
//! Pixels, not abstractions: this walks each dirty text row, draws its cells'
//! glyphs as an 8x16 bitmap into the pixel buffer, and leaves everything that
//! was not dirty alone. It takes the buffer as a plain byte slice and depends
//! only on the grid, so it runs under a host test exactly as it runs on the DMA
//! framebuffer - which is why it lives beside [`super::Grid`] rather than down
//! in `drivers`, where the host build cannot reach it.
//!
//! The buffer is BGRA32, row-major - blue first in memory, red third, matching
//! what `gpu.rs` reports through `FBIOGET_VSCREENINFO` (red offset 16, green 8,
//! blue 0).

use super::font::{self, CELL_H, CELL_W};
use super::{Cell, Grid, Rgb};

/// Paint the grid's dirty rows into `fb` at pixel `origin`, over a buffer that
/// is `fb_w` x `fb_h` pixels (so the walk stops at the edge, and a grid bigger
/// than the screen is clipped rather than run off the end).
///
/// `dirty` is the row bitmask from [`Grid::take_dirty`]. The cursor is drawn
/// last, inverted over its cell, so it stays legible regardless of the glyph
/// under it - unless the screen has hidden it (`cursor_visible`).
pub fn paint(
    grid: &Grid,
    fb: &mut [u8],
    fb_w: usize,
    fb_h: usize,
    origin: (usize, usize),
    dirty: u32,
) {
    let (ox, oy) = origin;
    let stride = fb_w * 4;
    let (rows, cols) = (grid.rows(), grid.cols());
    for r in 0..rows {
        if dirty & (1 << r) == 0 {
            continue;
        }
        for c in 0..cols {
            let cell = grid.cell(r, c);
            draw_cell(
                fb,
                stride,
                fb_w,
                fb_h,
                ox + c * CELL_W,
                oy + r * CELL_H,
                &cell,
            );
        }
    }

    if grid.cursor_visible {
        let (crow, ccol) = grid.cursor();
        let cell = grid.cell(crow, ccol);
        draw_cursor(
            fb,
            stride,
            fb_w,
            fb_h,
            ox + ccol * CELL_W,
            oy + crow * CELL_H,
            &cell,
        );
    }
}

fn draw_cell(
    fb: &mut [u8],
    stride: usize,
    fb_w: usize,
    fb_h: usize,
    cx: usize,
    cy: usize,
    cell: &Cell,
) {
    let glyph = font::glyph(cell.ch);
    for (py, &bits) in glyph.iter().enumerate() {
        let y = cy + py;
        if y >= fb_h {
            break;
        }
        for px in 0..CELL_W {
            let x = cx + px;
            if x >= fb_w {
                break;
            }
            let on = bits & (0x80 >> px) != 0;
            let color = if on { cell.fg } else { cell.bg };
            put(fb, stride, x, y, color);
        }
    }
}

/// A block cursor: the cell's glyph inverted (paper-coloured ink on an
/// ink-coloured field), so it reads as "here" without hiding the character.
fn draw_cursor(
    fb: &mut [u8],
    stride: usize,
    fb_w: usize,
    fb_h: usize,
    cx: usize,
    cy: usize,
    cell: &Cell,
) {
    let glyph = font::glyph(cell.ch);
    for (py, &bits) in glyph.iter().enumerate() {
        let y = cy + py;
        if y >= fb_h {
            break;
        }
        for px in 0..CELL_W {
            let x = cx + px;
            if x >= fb_w {
                break;
            }
            let on = bits & (0x80 >> px) != 0;
            let color = if on { cell.bg } else { cell.fg };
            put(fb, stride, x, y, color);
        }
    }
}

/// One pixel, BGRA in memory; out-of-range writes are dropped, not clamped.
fn put(fb: &mut [u8], stride: usize, x: usize, y: usize, color: Rgb) {
    let at = y * stride + x * 4;
    if at + 4 <= fb.len() {
        fb[at] = color[2]; // blue
        fb[at + 1] = color[1]; // green
        fb[at + 2] = color[0]; // red
        fb[at + 3] = 0x00;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::term::Grid;
    use alloc::vec;

    fn px(fb: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
        let at = y * w * 4 + x * 4;
        [fb[at + 2], fb[at + 1], fb[at]]
    }

    #[test]
    fn painting_a_cell_writes_the_glyph_ink_and_paper() {
        let mut fb = vec![0u8; 40 * 40 * 4];
        let mut grid = Grid::new(2, 2);
        grid.feed(b"A");
        let (fg, bg) = (grid.cell(0, 0).fg, grid.cell(0, 0).bg);
        paint(&grid, &mut fb, 40, 40, (0, 0), 0b11);
        // Text row 2 -> pixel row 2; the 'A' apex is the single ink pixel there.
        assert_eq!(px(&fb, 40, 3, 2), fg, "apex pixel is ink");
        assert_eq!(px(&fb, 40, 0, 2), bg, "row's left is paper");
    }

    #[test]
    fn a_clean_row_is_left_untouched() {
        let mut fb = vec![7u8; 20 * 40 * 4];
        let grid = Grid::new(2, 2);
        paint(&grid, &mut fb, 20, 40, (0, 0), 0b00);
        let at = 16 * 20 * 4;
        assert_eq!(&fb[at..at + 4], &[7, 7, 7, 7], "no dirty row, no repaint");
    }
}
