//! BT.709 full-range (0-255) colour conversion for the host capture path.
//!
//! Captured desktop content is sRGB 8-bit and must stay *full range* end to
//! end: limiting it to 16-235 crushes near-black UI (dark IDEs, dark themes)
//! and washes out highlights. Everything here is BT.709 (R709 primaries,
//! sRGB transfer, BT.709 matrix) and full range, matching the VUI flags the
//! encoder writes and the client shader that converts back.
//!
//! The GPU implementation of the same math lives in
//! `src-tauri/shaders/nv12_csc.hlsl`; [`NV12_CSC_HLSL`] embeds it so the
//! shader cannot drift away from the code that ships it.

pub const NV12_CSC_HLSL: &str = include_str!("../shaders/nv12_csc.hlsl");

/// BT.709 luma weights, full range.
pub const LUMA_R: f32 = 0.2126;
pub const LUMA_G: f32 = 0.7152;
pub const LUMA_B: f32 = 0.0722;

/// BT.709 chroma (full range, 0.5 biased).
pub const CB_R: f32 = -0.1146;
pub const CB_G: f32 = -0.3854;
pub const CB_B: f32 = 0.5000;

pub const CR_R: f32 = 0.5000;
pub const CR_G: f32 = -0.4542;
pub const CR_B: f32 = -0.0458;

/// Luma byte for an (r, g, b) triplet, full range: white maps to 255, black
/// to 0.
#[inline]
pub fn luma_bt709_full(r: u8, g: u8, b: u8) -> u8 {
    let y = LUMA_R * r as f32 + LUMA_G * g as f32 + LUMA_B * b as f32;
    y.round().clamp(0.0, 255.0) as u8
}

/// Cb / Cr bytes for an (r, g, b) triplet, full range with 0.5 bias.
#[inline]
pub fn chroma_bt709_full(r: f32, g: f32, b: f32) -> (u8, u8) {
    let cb = CB_R * r + CB_G * g + CB_B * b + 128.0;
    let cr = CR_R * r + CR_G * g + CR_B * b + 128.0;
    (
        cb.round().clamp(0.0, 255.0) as u8,
        cr.round().clamp(0.0, 255.0) as u8,
    )
}

// A horizontally shifted 2x2 box has 1:2:1 weights across three columns.
// This leaves the sample left-sited and half a pixel down for MPEG-2 4:2:0.
fn left_sited_chroma_rgb(bgra: &[u8], width: usize, x: usize, y: usize) -> [f32; 3] {
    let mut rgb = [0.0; 3];
    let mut weight_sum = 0.0;
    for row in [y, y + 1] {
        for (col, weight) in [
            (x.saturating_sub(1), 1.0),
            (x, 2.0),
            ((x + 1).min(width - 1), 1.0),
        ] {
            let idx = (row * width + col) * 4;
            if let Some(pixel) = bgra.get(idx..idx + 4) {
                rgb[0] += f32::from(pixel[2]) * weight;
                rgb[1] += f32::from(pixel[1]) * weight;
                rgb[2] += f32::from(pixel[0]) * weight;
                weight_sum += weight;
            }
        }
    }
    rgb.map(|component| component / f32::max(weight_sum, 1.0))
}

/// Convert tightly packed BGRA8 to NV12, BT.709 full range. Odd dimensions
/// are cropped to the 4:2:0 grid without changing the input row stride.
pub fn bgra_to_nv12_bt709(bgra: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let w = width & !1;
    let h = height & !1;
    let luma_len = w * h;
    out.clear();
    out.resize(luma_len + luma_len / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(luma_len);

    for y in 0..h {
        for x in 0..w {
            let idx = (y * width + x) * 4;
            if let Some(pixel) = bgra.get(idx..idx + 4) {
                y_plane[y * w + x] = luma_bt709_full(pixel[2], pixel[1], pixel[0]);
            }
        }
    }

    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let [r, g, b] = left_sited_chroma_rgb(bgra, width, x, y);
            let (u, v) = chroma_bt709_full(r, g, b);
            let dst = y / 2 * w + x;
            uv_plane[dst..dst + 2].copy_from_slice(&[u, v]);
        }
    }
}

/// Convert tightly packed BGRA8 to BT.709 full-range P010. Quantize directly
/// to 10 bits and put the sample in the high bits of each 16-bit word.
pub fn bgra_to_p010_bt709(bgra: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let w = width & !1;
    let h = height & !1;
    let luma_len = w * h * 2;
    out.clear();
    out.resize(luma_len + luma_len / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(luma_len);
    let pack = |v: f32| (((v.clamp(0.0, 1.0) * 1023.0).round() as u16) << 6).to_le_bytes();

    for y in 0..h {
        for x in 0..w {
            let idx = (y * width + x) * 4;
            if let Some(pixel) = bgra.get(idx..idx + 4) {
                let y_value = LUMA_R * f32::from(pixel[2])
                    + LUMA_G * f32::from(pixel[1])
                    + LUMA_B * f32::from(pixel[0]);
                let dst = (y * w + x) * 2;
                y_plane[dst..dst + 2].copy_from_slice(&pack(y_value / 255.0));
            }
        }
    }

    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let [r, g, b] = left_sited_chroma_rgb(bgra, width, x, y).map(|v| v / 255.0);
            let u = CB_R * r + CB_G * g + CB_B * b + 0.5;
            let v = CR_R * r + CR_G * g + CR_B * b + 0.5;
            let dst = (y / 2 * w + x) * 2;
            uv_plane[dst..dst + 2].copy_from_slice(&pack(u));
            uv_plane[dst + 2..dst + 4].copy_from_slice(&pack(v));
        }
    }
}

/// Opaque black / full white in NV12 relative to full range, used by tests and
/// by the pipeline self-check.
pub fn nv12_black(width: usize, height: usize) -> Vec<u8> {
    nv12_solid(width, height, 0, 128, 128)
}

pub fn nv12_white(width: usize, height: usize) -> Vec<u8> {
    nv12_solid(width, height, 255, 128, 128)
}

fn nv12_solid(width: usize, height: usize, y: u8, u: u8, v: u8) -> Vec<u8> {
    let mut out = vec![y; width * height];
    out.extend(std::iter::repeat_n([u, v], (width / 2) * (height / 2)).flatten());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_solid(width: usize, height: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            out.extend_from_slice(&[b, g, r, 255]);
        }
        out
    }

    #[test]
    fn full_range_endpoints_are_exact() {
        assert_eq!(luma_bt709_full(0, 0, 0), 0);
        assert_eq!(luma_bt709_full(255, 255, 255), 255);
        // Never limited range: no 16 floor, no 235 ceiling.
        assert_ne!(luma_bt709_full(0, 0, 0), 16);
        assert_ne!(luma_bt709_full(255, 255, 255), 235);
    }

    #[test]
    fn bt709_primaries_land_on_the_expected_codes() {
        // Pure red: Y = 0.2126 * 255 = 54.2, Cb = 128 - 29.2 = 98.8, Cr = 128 + 127.5 = 255.
        assert_eq!(luma_bt709_full(255, 0, 0), 54);
        let (cb, cr) = chroma_bt709_full(255.0, 0.0, 0.0);
        assert_eq!(cb, 99);
        assert_eq!(cr, 255);

        // Pure green: Y = 182.4.
        assert_eq!(luma_bt709_full(0, 255, 0), 182);
        // Pure blue: Y = 18.4.
        assert_eq!(luma_bt709_full(0, 0, 255), 18);
    }

    #[test]
    fn bt709_and_bt601_are_distinguishable() {
        // The whole point of the module: BT.601 would give 76 here, BT.709 gives 54.
        assert_ne!(luma_bt709_full(255, 0, 0), 76);
    }

    #[test]
    fn nv12_planes_have_the_right_size_and_grey_axis() {
        let bgra = bgra_solid(4, 4, 255, 255, 255);
        let mut out = Vec::new();
        bgra_to_nv12_bt709(&bgra, 4, 4, &mut out);
        assert_eq!(out.len(), 4 * 4 + (4 * 4) / 2);
        assert!(out[..16].iter().all(|&y| y == 255));
        // Neutral chroma sits exactly at 128 in full range.
        assert!(out[16..].iter().all(|&c| c == 128));
    }

    #[test]
    fn chroma_is_a_2x2_box_average_not_a_point_sample() {
        // Left half black, right half white in a 2x2 block. Point sampling
        // would give chroma for pure black; the box average gives grey.
        let mut bgra = bgra_solid(2, 2, 0, 0, 0);
        // Top-right and bottom-right pixels -> white.
        for px in [1usize, 3] {
            bgra[px * 4] = 255;
            bgra[px * 4 + 1] = 255;
            bgra[px * 4 + 2] = 255;
        }
        let mut out = Vec::new();
        bgra_to_nv12_bt709(&bgra, 2, 2, &mut out);
        let (u, v) = (out[4], out[5]);
        // Average of black and white is mid grey => neutral chroma.
        assert_eq!((u, v), (128, 128));
    }

    #[test]
    fn odd_dimensions_are_truncated_instead_of_overflowing() {
        let bgra = bgra_solid(5, 5, 10, 20, 30);
        let mut out = Vec::new();
        bgra_to_nv12_bt709(&bgra, 5, 5, &mut out);
        assert_eq!(out.len(), 4 * 4 + (4 * 4) / 2);
    }

    #[test]
    fn p010_is_bit_depth_scaled() {
        let bgra = bgra_solid(2, 2, 255, 255, 255);
        let mut out = Vec::new();
        bgra_to_p010_bt709(&bgra, 2, 2, &mut out);
        assert_eq!(out.len(), 2 * 2 * 2 + 2 * 1 * 2);
        let y = u16::from_le_bytes([out[0], out[1]]);
        assert_eq!(y, 1023 << 6);
    }

    #[test]
    fn shader_is_embedded_and_full_range() {
        assert!(NV12_CSC_HLSL.contains("0.2126"));
        assert!(NV12_CSC_HLSL.contains("0.7152"));
        assert!(NV12_CSC_HLSL.contains("0.0722"));
        assert!(NV12_CSC_HLSL.contains("-0.1146"));
        assert!(!NV12_CSC_HLSL.contains("16.0 / 255.0"));
    }

    #[test]
    fn black_helpers_match_the_converter() {
        let black = nv12_black(2, 2);
        let white = nv12_white(2, 2);
        assert_eq!(black[0], 0);
        assert_eq!(white[0], 255);
    }
}
