//! The DRM cursor's arithmetic, in the one place a host test can reach it.
//!
//! `src/drivers/drm/` has no `cfg` gates and is never compiled for the host, so
//! anything written there is pinned only by booting a machine. Most of the card's
//! work is bookkeeping - id tables, property values, two-call list shapes - and
//! booting is the right instrument for it. The cursor is different: it is the one
//! piece of *arithmetic* on the presentation path. A 64x64 image is blended over
//! the scanout at a position that moves with the pointer, clipped at whichever of
//! four edges the pointer is near, with every pixel premultiplied. That is
//! exactly the kind of code that is wrong at one edge and looks right at the
//! other three, and a compositor's pointer is where the user finds out.
//!
//! So it lives here, following `drm_blob.rs` (the EDID and `IN_FORMATS`
//! encoders), `evdev.rs` and `uinput.rs`: pure logic in an ungated leaf module
//! with `cargo test` beside it, called from the driver that cannot be tested that
//! way.

/// Blend an `ARGB8888` cursor image over a `BGRA` scanout.
///
/// **Both buffers are blue, green, red, alpha in memory**, and that coincidence
/// is why there is no channel shuffle here. `DRM_FORMAT_ARGB8888` is the fourcc
/// `"AR24"`, whose components are named most-significant first in the 32-bit
/// word, so its low byte - the first byte in memory on this machine - is blue;
/// and the scanout this card presents into is `B8G8R8A8`, which is the same
/// order. `crate::term::blit`'s `put` writes blue, green, red, 0 for the same
/// reason. The only difference between the two formats is what the fourth byte
/// means, and the scanout's own alpha is not read by anything.
///
/// **The image is premultiplied**, which is what DRM's cursor planes are:
/// `DRM_MODE_BLEND_PREMULTI` is the default and the `pixel blend mode` property
/// that would say otherwise is not one this card publishes. So a source pixel is
/// `colour + destination * (1 - alpha)` componentwise, with the source colour
/// already scaled by its own alpha. An un-premultiplied image composited this way
/// comes out too bright, which is the classic version of this bug.
///
/// **`hotspot` is where the image is pinned to the pointer.** The image's pixel
/// at `hotspot` lands on the scanout at `at`; the rest of the image follows. An
/// arrow cursor is 0,0 - the tip is the top-left pixel - and the `drm_mode_cursor2`
/// call carries the pair for the images that are not.
///
/// Anything outside the scanout is left alone rather than clipped with a guard per
/// pixel: the four edges are intersected once, up front, and the loop runs over
/// the intersection. A cursor half off the right edge and one half off the left
/// are the same code, which is the property the tests below are arranged around.
///
/// Returns whether any pixel was touched - `false` for an image that is empty or
/// entirely off the scanout, so the caller can skip a flush that would change
/// nothing.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    dst: &mut [u8],
    dst_pitch: usize,
    dst_w: usize,
    dst_h: usize,
    src: &[u8],
    src_pitch: usize,
    src_w: usize,
    src_h: usize,
    hotspot: (i32, i32),
    at: (i32, i32),
) -> bool {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return false;
    }
    // Where the image's top-left corner goes, and then where that leaves the part
    // of it there is room for. `i64` so that `at` being a wild number from a
    // caller cannot overflow the sum.
    let left = at.0 as i64 - hotspot.0 as i64;
    let top = at.1 as i64 - hotspot.1 as i64;
    let x0 = left.max(0);
    let y0 = top.max(0);
    let x1 = (left + src_w as i64).min(dst_w as i64);
    let y1 = (top + src_h as i64).min(dst_h as i64);
    if x0 >= x1 || y0 >= y1 {
        return false;
    }

    let mut touched = false;
    for y in y0..y1 {
        for x in x0..x1 {
            let so = (y - top) as usize * src_pitch + (x - left) as usize * 4;
            let dof = y as usize * dst_pitch + x as usize * 4;
            // A source or destination that does not really hold the pixel its
            // geometry claims is not something to read past the end of: the
            // caller is the kernel's own state, but this function is reachable
            // from a unit test with any arguments at all.
            if so + 4 > src.len() || dof + 4 > dst.len() {
                return touched;
            }
            let alpha = src[so + 3] as u32;
            if alpha == 0 {
                continue; // fully transparent, and the common case around a glyph
            }
            if alpha == 255 {
                dst[dof..dof + 3].copy_from_slice(&src[so..so + 3]);
            } else {
                let keep = 255 - alpha;
                for c in 0..3 {
                    let s = src[so + c] as u32;
                    let d = dst[dof + c] as u32;
                    // Rounded rather than truncated: the destination is re-copied
                    // from the client's framebuffer on every flush, so an error
                    // here does not accumulate - but it does make a half-covered
                    // pixel a shade darker than it should be.
                    dst[dof + c] = (s + (d * keep + 127) / 255) as u8;
                }
            }
            // The scanout's own alpha byte is deliberately not written: nothing
            // reads it, and inventing a value for it would be inventing state.
            touched = true;
        }
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::compose;
    use alloc::vec::Vec;

    /// A scanout of `w`x`h` filled with one colour, as `blue, green, red, 0`.
    fn scanout(w: usize, h: usize, colour: [u8; 3]) -> Vec<u8> {
        let mut v = alloc::vec![0u8; w * h * 4];
        for px in v.chunks_exact_mut(4) {
            px[0] = colour[0];
            px[1] = colour[1];
            px[2] = colour[2];
        }
        v
    }

    /// A solid `w`x`h` premultiplied `ARGB8888` image.
    fn image(w: usize, h: usize, colour: [u8; 3], alpha: u8) -> Vec<u8> {
        let mut v = alloc::vec![0u8; w * h * 4];
        for px in v.chunks_exact_mut(4) {
            px[0] = colour[0];
            px[1] = colour[1];
            px[2] = colour[2];
            px[3] = alpha;
        }
        v
    }

    fn pixel(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * w + x) * 4;
        [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]
    }

    #[test]
    fn an_opaque_pixel_replaces_what_was_there() {
        let mut dst = scanout(4, 4, [0, 0, 0]);
        let img = image(2, 2, [10, 20, 30], 255);
        assert!(compose(&mut dst, 16, 4, 4, &img, 8, 2, 2, (0, 0), (1, 1)));
        // The image's top-left lands on the scanout's (1,1).
        assert_eq!(pixel(&dst, 4, 1, 1), [10, 20, 30, 0]);
        assert_eq!(pixel(&dst, 4, 2, 2), [10, 20, 30, 0]);
        // And nothing outside it moved.
        assert_eq!(pixel(&dst, 4, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn a_transparent_pixel_is_not_touched_at_all() {
        let mut dst = scanout(2, 2, [7, 8, 9]);
        let img = image(2, 2, [255, 255, 255], 0);
        // Fully transparent is "no pixel was touched", which is what lets a
        // caller skip a flush.
        assert!(!compose(&mut dst, 8, 2, 2, &img, 8, 2, 2, (0, 0), (0, 0)));
        assert_eq!(pixel(&dst, 2, 0, 0), [7, 8, 9, 0]);
    }

    #[test]
    fn a_half_covered_pixel_is_the_sum_of_the_two() {
        let mut dst = scanout(1, 1, [0, 0, 100]);
        // Premultiplied: half alpha means the colour arrives already halved.
        let img = image(1, 1, [50, 25, 0], 128);
        assert!(compose(&mut dst, 4, 1, 1, &img, 4, 1, 1, (0, 0), (0, 0)));
        let out = pixel(&dst, 1, 0, 0);
        assert_eq!(out[0], 50);
        assert_eq!(out[1], 25);
        // 0 + round(100 * 127/255) = round(49.8) = 50. This case does not separate
        // rounding from truncation - the next test does.
        assert_eq!(out[2], 50);
    }

    #[test]
    fn the_destination_is_rounded_rather_than_truncated() {
        // 200 * 127 / 255 = 99.6, so 100 rounded and 99 truncated.
        let mut dst = scanout(1, 1, [0, 0, 200]);
        let img = image(1, 1, [0, 0, 0], 128);
        assert!(compose(&mut dst, 4, 1, 1, &img, 4, 1, 1, (0, 0), (0, 0)));
        assert_eq!(pixel(&dst, 1, 0, 0)[2], 100);
    }

    #[test]
    fn the_hotspot_pins_the_image_to_the_pointer() {
        let mut dst = scanout(4, 4, [0, 0, 0]);
        let img = image(2, 2, [1, 2, 3], 255);
        // A hotspot of (1,1) puts the image's (1,1) pixel on the pointer, so the
        // image's top-left goes to (0,0) and its (1,1) to (1,1).
        assert!(compose(&mut dst, 16, 4, 4, &img, 8, 2, 2, (1, 1), (1, 1)));
        assert_eq!(pixel(&dst, 4, 0, 0), [1, 2, 3, 0]);
        assert_eq!(pixel(&dst, 4, 1, 1), [1, 2, 3, 0]);
        assert_eq!(pixel(&dst, 4, 2, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn clipping_is_the_same_code_at_every_edge() {
        // Each case puts a 4x4 image so that exactly a 2x2 corner of it is on the
        // scanout, and every one of the four must leave the same 2x2 covered:
        // that is the property the up-front intersection buys, and the reason
        // there is no guard inside the loop.
        let cases = [
            // (pointer, where the covered corner should be)
            ((-2, -2), [(0, 0), (1, 0), (0, 1), (1, 1)]), // off the top-left
            ((2, -2), [(2, 0), (3, 0), (2, 1), (3, 1)]),  // off the top-right
            ((-2, 2), [(0, 2), (1, 2), (0, 3), (1, 3)]),  // off the bottom-left
            ((2, 2), [(2, 2), (3, 2), (2, 3), (3, 3)]),   // off the bottom-right
        ];
        for (at, covered) in cases {
            let mut dst = scanout(4, 4, [0, 0, 0]);
            let img = image(4, 4, [9, 9, 9], 255);
            assert!(
                compose(&mut dst, 16, 4, 4, &img, 16, 4, 4, (0, 0), at),
                "at {at:?} something must have been drawn"
            );
            for y in 0..4 {
                for x in 0..4 {
                    let want = if covered.contains(&(x, y)) {
                        [9, 9, 9, 0]
                    } else {
                        [0, 0, 0, 0]
                    };
                    assert_eq!(pixel(&dst, 4, x, y), want, "at {at:?}, pixel ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn an_image_entirely_off_the_scanout_draws_nothing() {
        let mut dst = scanout(4, 4, [5, 5, 5]);
        let img = image(2, 2, [9, 9, 9], 255);
        assert!(!compose(
            &mut dst,
            16,
            4,
            4,
            &img,
            8,
            2,
            2,
            (0, 0),
            (100, 100)
        ));
        assert!(!compose(&mut dst, 16, 4, 4, &img, 8, 2, 2, (0, 0), (-9, 0)));
        assert_eq!(pixel(&dst, 4, 0, 0), [5, 5, 5, 0]);
    }

    #[test]
    fn an_empty_image_draws_nothing() {
        let mut dst = scanout(2, 2, [1, 1, 1]);
        let img = image(0, 0, [9, 9, 9], 255);
        assert!(!compose(&mut dst, 8, 2, 2, &img, 0, 0, 0, (0, 0), (0, 0)));
    }

    #[test]
    fn a_negative_pointer_with_a_hotspot_still_lands_where_it_says() {
        // The pointer is off the left edge but the image is wide, so its
        // right-hand columns are still on the scanout. The hotspot has to survive
        // the clipping for those columns to land where the caller asked - which
        // is the bug this pins, since the obvious way to write the clip is to
        // clamp the *pointer* and lose the offset with it.
        let w = 4usize;
        let pitch = 16usize;
        let mut src = alloc::vec![0u8; pitch];
        for x in 0..w {
            // Column x carries the value x + 1, so a misplaced column is visible.
            src[x * 4] = (x + 1) as u8;
            src[x * 4 + 3] = 255;
        }
        let mut dst = scanout(4, 1, [0, 0, 0]);
        // Hotspot at column 2, pointer at x = 0: the image's top-left goes to
        // -2, so its columns 2 and 3 occupy scanout columns 0 and 1.
        assert!(compose(
            &mut dst,
            16,
            4,
            1,
            &src,
            pitch,
            w,
            1,
            (2, 0),
            (0, 0)
        ));
        assert_eq!(pixel(&dst, 4, 0, 0)[0], 3);
        assert_eq!(pixel(&dst, 4, 1, 0)[0], 4);
        assert_eq!(pixel(&dst, 4, 2, 0)[0], 0);
        assert_eq!(pixel(&dst, 4, 3, 0)[0], 0);
    }

    #[test]
    fn a_pitch_wider_than_the_image_is_respected() {
        // The cursor buffer's pitch is 64-byte aligned, so a 2-pixel-wide cursor
        // has a pitch of 64 rather than 8. Reading it as if it were tight is a
        // real failure mode: it would smear each row into the next.
        let w = 2usize;
        let pitch = 64usize;
        let mut src = alloc::vec![0u8; pitch * 2];
        for y in 0..2 {
            for x in 0..w {
                let at = y * pitch + x * 4;
                src[at] = (x + 1) as u8;
                src[at + 3] = 255;
            }
        }
        let mut dst = scanout(2, 2, [0, 0, 0]);
        assert!(compose(
            &mut dst,
            8,
            2,
            2,
            &src,
            pitch,
            w,
            2,
            (0, 0),
            (0, 0)
        ));
        assert_eq!(pixel(&dst, 2, 0, 0)[0], 1);
        assert_eq!(pixel(&dst, 2, 1, 0)[0], 2);
        assert_eq!(pixel(&dst, 2, 0, 1)[0], 1);
    }
}
