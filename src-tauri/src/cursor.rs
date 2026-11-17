//! Host-side mouse pointer compositing.
//!
//! Desktop Duplication deliberately leaves the mouse pointer **out** of the
//! acquired desktop image: it reports the pointer separately in
//! `DXGI_OUTDUPL_FRAME_INFO` and expects the capturer to draw it. VmaX never
//! did, and the client hides its own cursor whenever "Hardware Cursor" is on,
//! so the streamed desktop had no visible pointer at all — while every mouse
//! move still forced a full encode of an unchanged picture.
//!
//! This module owns the drawing part. It is deliberately platform-independent
//! and unit-tested: the capture path itself cannot run on a test box, but the
//! pixel rules can.
//!
//! Three shape formats have to be supported, because Windows still hands out
//! all of them:
//!
//! * [`SHAPE_COLOR`] — a plain 32-bpp BGRA bitmap with an alpha channel.
//! * [`SHAPE_MASKED_COLOR`] — 32-bpp where alpha 0 means "invert the desktop"
//!   and alpha 255 means "replace it" (used by some legacy and themed cursors).
//! * [`SHAPE_MONOCHROME`] — the classic AND/XOR bit masks, two stacked bitmaps
//!   of `height / 2` rows each.

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME`.
pub const SHAPE_MONOCHROME: u32 = 1;
/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR`.
pub const SHAPE_COLOR: u32 = 2;
/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR`.
pub const SHAPE_MASKED_COLOR: u32 = 4;

/// One pointer bitmap as reported by `IDXGIOutputDuplication::GetFramePointerShape`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerShape {
    /// One of the `SHAPE_*` constants.
    pub kind: u32,
    pub width: u32,
    /// For monochrome shapes this is the combined height of both masks; the
    /// visible cursor is half of it.
    pub height: u32,
    /// Bytes per row of the shape bitmap, which is not always `width * 4`.
    pub pitch: u32,
    /// The pixel inside the bitmap that sits on the reported pointer position.
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub pixels: Vec<u8>,
}

impl PointerShape {
    /// Rows of the visible cursor. Monochrome shapes stack the AND mask above
    /// the XOR mask, so the bitmap is twice as tall as the cursor.
    pub fn visible_height(&self) -> u32 {
        if self.kind == SHAPE_MONOCHROME {
            self.height / 2
        } else {
            self.height
        }
    }

    /// Cheap sanity check before the per-frame draw.
    pub fn is_drawable(&self) -> bool {
        self.width > 0
            && self.visible_height() > 0
            && self.pitch > 0
            && !self.pixels.is_empty()
            && matches!(
                self.kind,
                SHAPE_MONOCHROME | SHAPE_COLOR | SHAPE_MASKED_COLOR
            )
    }

    /// The BGRA colour and alpha of one cursor pixel, or `None` when the pixel
    /// is transparent.
    fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.visible_height() {
            return None;
        }
        let pitch = self.pitch as usize;
        match self.kind {
            SHAPE_MONOCHROME => {
                let row = y as usize;
                let col = x as usize;
                let byte = col / 8;
                let bit = 7 - (col % 8);
                if byte >= pitch {
                    return None;
                }
                let and_offset = row * pitch + byte;
                let xor_offset = (row + self.visible_height() as usize) * pitch + byte;
                let and_mask = *self.pixels.get(and_offset)?;
                let xor_mask = *self.pixels.get(xor_offset)?;
                let and_bit = (and_mask >> bit) & 1;
                let xor_bit = (xor_mask >> bit) & 1;
                if and_bit == 1 {
                    // AND set: the desktop shows through.
                    return None;
                }
                Some(if xor_bit == 1 {
                    [0xFF, 0xFF, 0xFF, 0xFF]
                } else {
                    [0x00, 0x00, 0x00, 0xFF]
                })
            }
            SHAPE_COLOR => {
                let offset = y as usize * pitch + x as usize * 4;
                let px = self.pixels.get(offset..offset + 4)?;
                Some([px[0], px[1], px[2], px[3]])
            }
            SHAPE_MASKED_COLOR => {
                let offset = y as usize * pitch + x as usize * 4;
                let px = self.pixels.get(offset..offset + 4)?;
                // Alpha 0 is the documented "invert the desktop" value; pack it
                // as a fully transparent pixel and let `composite` invert.
                Some([px[0], px[1], px[2], px[3]])
            }
            _ => None,
        }
    }
}

/// Blend `shape` into a tightly packed BGRA frame.
///
/// `dst` is `dst_w * dst_h * 4` bytes at *encode* resolution, while
/// `pointer_x`/`pointer_y` and the shape itself are in *desktop* (native)
/// pixels — the pointer is scaled by the same factor the frame was, so it lands
/// where the user's cursor actually is on a downscaled stream.
///
/// Everything is clipped: a pointer parked on a second monitor reports
/// coordinates outside this output, and that must be a no-op rather than an
/// out-of-bounds write.
pub fn composite(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    pointer_x: i32,
    pointer_y: i32,
    source_w: u32,
    source_h: u32,
    shape: &PointerShape,
) {
    if dst_w == 0 || dst_h == 0 || source_w == 0 || source_h == 0 {
        return;
    }
    if !shape.is_drawable() {
        return;
    }
    if dst.len() != dst_w as usize * dst_h as usize * 4 {
        return;
    }

    let scale_x = dst_w as f32 / source_w as f32;
    let scale_y = dst_h as f32 / source_h as f32;
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return;
    }

    let visible_h = shape.visible_height();
    let left = pointer_x - shape.hotspot_x;
    let top = pointer_y - shape.hotspot_y;

    // Destination rectangle, rounded outwards and clipped to the frame.
    let x0 = ((left as f32) * scale_x).floor().max(0.0) as i64;
    let y0 = ((top as f32) * scale_y).floor().max(0.0) as i64;
    let x1 = (((left + shape.width as i32) as f32) * scale_x)
        .ceil()
        .min(dst_w as f32) as i64;
    let y1 = (((top + visible_h as i32) as f32) * scale_y)
        .ceil()
        .min(dst_h as f32) as i64;
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    let row_bytes = dst_w as usize * 4;
    for dy in y0..y1 {
        // Map the destination row back into shape space, nearest neighbour.
        let src_y = (((dy as f32 + 0.5) / scale_y) - top as f32).floor();
        if src_y < 0.0 || src_y >= visible_h as f32 {
            continue;
        }
        let src_y = src_y as u32;
        let row = dy as usize * row_bytes;
        for dx in x0..x1 {
            let src_x = (((dx as f32 + 0.5) / scale_x) - left as f32).floor();
            if src_x < 0.0 || src_x >= shape.width as f32 {
                continue;
            }
            let Some(px) = shape.pixel(src_x as u32, src_y) else {
                continue;
            };
            let offset = row + dx as usize * 4;
            let Some(pixel) = dst.get_mut(offset..offset + 4) else {
                continue;
            };
            blend(pixel, px, shape.kind);
        }
    }
}

/// One cursor pixel over one frame pixel. `dst` and `src` are BGRA.
fn blend(dst: &mut [u8], src: [u8; 4], kind: u32) {
    let alpha = src[3];
    if kind == SHAPE_MASKED_COLOR && alpha == 0 {
        // Documented "invert the desktop" mask pixel.
        for channel in dst.iter_mut().take(3) {
            *channel = 255 - *channel;
        }
        return;
    }
    if alpha == 0 {
        return;
    }
    if alpha == 0xFF {
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        return;
    }
    let a = u32::from(alpha);
    let inv = 255 - a;
    for i in 0..3 {
        let blended = (u32::from(src[i]) * a + u32::from(dst[i]) * inv + 127) / 255;
        dst[i] = blended as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_shape(pixels: &[[u8; 4]], hotspot: (i32, i32)) -> PointerShape {
        let mut bytes = Vec::new();
        for p in pixels {
            bytes.extend_from_slice(p);
        }
        PointerShape {
            kind: SHAPE_COLOR,
            width: pixels.len() as u32,
            height: 1,
            pitch: pixels.len() as u32 * 4,
            hotspot_x: hotspot.0,
            hotspot_y: hotspot.1,
            pixels: bytes,
        }
    }

    fn frame(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for chunk in v.chunks_exact_mut(4) {
            chunk.copy_from_slice(&bgra);
        }
        v
    }

    #[test]
    fn opaque_pointer_pixels_replace_the_desktop() {
        let shape = color_shape(&[[0, 0, 255, 255]], (0, 0));
        let mut dst = frame(4, 2, [10, 20, 30, 255]);
        composite(&mut dst, 4, 2, 2, 1, 4, 2, &shape);
        // Red at the pointer position only.
        assert_eq!(&dst[(1 * 4 + 2) * 4..][..4], &[0, 0, 255, 255]);
        assert_eq!(&dst[(1 * 4 + 1) * 4..][..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn hotspot_offsets_the_pointer_so_the_tip_lands_on_the_position() {
        // 3x1 pointer whose hotspot is the middle pixel.
        let shape = color_shape(&[[1, 2, 3, 255], [4, 5, 6, 255], [7, 8, 9, 255]], (1, 0));
        let mut dst = frame(6, 1, [0, 0, 0, 255]);
        composite(&mut dst, 6, 1, 3, 0, 6, 1, &shape);
        assert_eq!(&dst[(3 - 1) * 4..][..4], &[1, 2, 3, 255]);
        assert_eq!(&dst[3 * 4..][..4], &[4, 5, 6, 255]);
        assert_eq!(&dst[(3 + 1) * 4..][..4], &[7, 8, 9, 255]);
    }

    #[test]
    fn translucent_pointer_pixels_blend() {
        let shape = color_shape(&[[255, 255, 255, 128]], (0, 0));
        let mut dst = frame(1, 1, [0, 0, 0, 255]);
        composite(&mut dst, 1, 1, 0, 0, 1, 1, &shape);
        assert_eq!(dst[0], 128);
        assert_eq!(dst[3], 255, "alpha channel must stay opaque");
    }

    #[test]
    fn pointer_is_scaled_with_the_frame() {
        // 4x4 desktop captured at 2x2: a 2x2 pointer at (2,2) covers (1,1).
        let shape = color_shape(&[[9, 9, 9, 255], [9, 9, 9, 255]], (0, 0));
        let shape = PointerShape {
            height: 2,
            pitch: 8,
            pixels: vec![9, 9, 9, 255, 9, 9, 9, 255, 9, 9, 9, 255, 9, 9, 9, 255],
            ..shape
        };
        let mut dst = frame(2, 2, [0, 0, 0, 255]);
        composite(&mut dst, 2, 2, 2, 2, 4, 4, &shape);
        assert_eq!(&dst[(1 * 2 + 1) * 4..][..4], &[9, 9, 9, 255]);
        assert_eq!(&dst[(0 * 2 + 0) * 4..][..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn pointer_outside_the_captured_output_is_a_no_op() {
        let shape = color_shape(&[[1, 1, 1, 255]], (0, 0));
        let mut dst = frame(4, 4, [7, 7, 7, 255]);
        let untouched = dst.clone();
        composite(&mut dst, 4, 4, 9_000, 9_000, 4, 4, &shape);
        composite(&mut dst, 4, 4, -9_000, -9_000, 4, 4, &shape);
        assert_eq!(dst, untouched);
    }

    #[test]
    fn partially_offscreen_pointer_is_clipped_not_wrapped() {
        let shape = color_shape(&[[1, 1, 1, 255], [2, 2, 2, 255]], (0, 0));
        let mut dst = frame(2, 1, [0, 0, 0, 255]);
        composite(&mut dst, 2, 1, -1, 0, 2, 1, &shape);
        // The second pixel lands on column 0; the first is off-screen.
        assert_eq!(&dst[0..4], &[2, 2, 2, 255]);
        assert_eq!(&dst[4..8], &[0, 0, 0, 255]);
    }

    #[test]
    fn monochrome_and_mask_leaves_the_desktop_alone() {
        // One cursor pixel: AND=1 (transparent), XOR=0.
        let shape = PointerShape {
            kind: SHAPE_MONOCHROME,
            width: 1,
            height: 2,
            pitch: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![0b1000_0000, 0b0000_0000],
        };
        assert_eq!(shape.visible_height(), 1);
        let mut dst = frame(1, 1, [50, 60, 70, 255]);
        composite(&mut dst, 1, 1, 0, 0, 1, 1, &shape);
        assert_eq!(&dst[0..4], &[50, 60, 70, 255]);
    }

    #[test]
    fn monochrome_xor_mask_paints_black_or_white() {
        let black = PointerShape {
            kind: SHAPE_MONOCHROME,
            width: 1,
            height: 2,
            pitch: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![0b0000_0000, 0b0000_0000],
        };
        let mut dst = frame(1, 1, [50, 60, 70, 255]);
        composite(&mut dst, 1, 1, 0, 0, 1, 1, &black);
        assert_eq!(&dst[0..4], &[0, 0, 0, 255]);

        let white = PointerShape {
            pixels: vec![0b0000_0000, 0b1000_0000],
            ..black
        };
        let mut dst = frame(1, 1, [50, 60, 70, 255]);
        composite(&mut dst, 1, 1, 0, 0, 1, 1, &white);
        assert_eq!(&dst[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn masked_color_with_zero_alpha_inverts_the_desktop() {
        let shape = PointerShape {
            kind: SHAPE_MASKED_COLOR,
            width: 1,
            height: 1,
            pitch: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![0, 0, 0, 0],
        };
        let mut dst = frame(1, 1, [10, 20, 30, 255]);
        composite(&mut dst, 1, 1, 0, 0, 1, 1, &shape);
        assert_eq!(&dst[0..3], &[245, 235, 225]);
    }

    #[test]
    fn a_shape_with_no_pixels_never_touches_the_frame() {
        let shape = PointerShape {
            kind: SHAPE_COLOR,
            width: 32,
            height: 32,
            pitch: 128,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: Vec::new(),
        };
        assert!(!shape.is_drawable());
        let mut dst = frame(8, 8, [1, 2, 3, 255]);
        let untouched = dst.clone();
        composite(&mut dst, 8, 8, 2, 2, 8, 8, &shape);
        assert_eq!(dst, untouched);
    }
}
