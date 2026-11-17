//! Parsec-style quality retention: FSR fidelity detection, adaptive sharpening
//! and the QP/AQ coupling that keeps text and edges readable at low bitrate.

use bytemuck::{Pod, Zeroable};
use std::time::{Duration, Instant};

/// AMD FidelityFX Super Resolution 1 quality presets and their render scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FsrMode {
    /// Native: no spatial reconstruction.
    Off,
    /// 1.3x linear (0.77 render scale): near-native detail.
    UltraQuality,
    /// 1.5x linear (0.67 render scale): the usual sweet spot.
    Quality,
    /// 1.7x linear (0.59 render scale).
    Balanced,
    /// 2.0x linear (0.5 render scale): 1080p render for a 4K display.
    Performance,
}

impl FsrMode {
    pub fn factor(self) -> f32 {
        match self {
            Self::Off => 1.0,
            Self::UltraQuality => 1.3,
            Self::Quality => 1.5,
            Self::Balanced => 1.7,
            Self::Performance => 2.0,
        }
    }

    pub fn render_scale(self) -> f32 {
        1.0 / self.factor()
    }

    pub fn smallest_fit(display_pixels: u64, needed_pixels: u64) -> Self {
        if needed_pixels >= display_pixels {
            return Self::Off;
        }
        for mode in [
            Self::UltraQuality,
            Self::Quality,
            Self::Balanced,
            Self::Performance,
        ] {
            let scale = f64::from(mode.render_scale());
            let pixels = (display_pixels as f64 * scale * scale) as u64;
            if pixels <= needed_pixels.max(1) {
                return mode;
            }
        }
        Self::Performance
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "native",
            Self::UltraQuality => "FSR Ultra Quality",
            Self::Quality => "FSR Quality",
            Self::Balanced => "FSR Balanced",
            Self::Performance => "FSR Performance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpscalingMode {
    Off,
    Auto,
    UltraQuality,
    Quality,
    Balanced,
    Performance,
}

impl UpscalingMode {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "ultra_quality" | "ultra-quality" | "ultraquality" => Self::UltraQuality,
            "quality" => Self::Quality,
            "balanced" => Self::Balanced,
            "performance" => Self::Performance,
            _ => Self::Off,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Auto => 1,
            Self::UltraQuality => 2,
            Self::Quality => 3,
            Self::Balanced => 4,
            Self::Performance => 5,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Auto,
            2 => Self::UltraQuality,
            3 => Self::Quality,
            4 => Self::Balanced,
            5 => Self::Performance,
            _ => Self::Off,
        }
    }

    pub fn explicit_fsr(self) -> Option<FsrMode> {
        match self {
            Self::Off => Some(FsrMode::Off),
            Self::UltraQuality => Some(FsrMode::UltraQuality),
            Self::Quality => Some(FsrMode::Quality),
            Self::Balanced => Some(FsrMode::Balanced),
            Self::Performance => Some(FsrMode::Performance),
            Self::Auto => None,
        }
    }
}

/// Kept for API compatibility: the host never scales the frame it encodes, so
/// the result is always the requested (even-aligned) size.
pub fn apply_upscaling(
    width: u32,
    height: u32,
    _mode: UpscalingMode,
    _bitrate_bps: u32,
    _fps: u16,
) -> (u32, u32) {
    (
        even(width.max(MIN_RENDER_DIM)),
        even(height.max(MIN_RENDER_DIM)),
    )
}

#[derive(Debug, Clone)]
pub struct UpscalingController {
    detector: FidelityDetector,
}

impl Default for UpscalingController {
    fn default() -> Self {
        Self::new()
    }
}

impl UpscalingController {
    pub fn new() -> Self {
        Self {
            detector: FidelityDetector::new(),
        }
    }

    /// The host always encodes at the resolution the host selected.
    ///
    /// Rendering smaller than the selected resolution and letting the client
    /// stretch the picture back is exactly what made the old "FSR" path look
    /// soft: the client received fewer pixels than it displayed, and no
    /// reconstruction filter can invent them. Reconstruction now happens on
    /// the client, after decode, at the display's own resolution.
    pub fn dims(
        &mut self,
        width: u32,
        height: u32,
        _mode: UpscalingMode,
        _bitrate_bps: u32,
        _fps: u16,
        now: Instant,
    ) -> (u32, u32) {
        let _ = (now, self.detector.current().render_w);
        (
            even(width.max(MIN_RENDER_DIM)),
            even(height.max(MIN_RENDER_DIM)),
        )
    }

    pub fn current_fsr(&self) -> FsrMode {
        self.detector.current().fsr
    }
}

/// No host-side render scaling means no scaling boost: sharpening a native
/// encode harder only adds halos and coding noise. The client sharpens the
/// image it actually displays.
pub fn effective_sharpness(user_sharpness: u8, _mode: UpscalingMode) -> u8 {
    user_sharpness.min(100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentMode {
    Motion,
    Balanced,
    Detail,
}

impl ContentMode {
    pub fn min_bits_per_pixel_milli(self) -> u64 {
        match self {
            Self::Motion => 46,
            Self::Balanced => 38,
            Self::Detail => 32,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FidelityInput {
    pub display_w: u32,
    pub display_h: u32,
    pub fps: u16,
    pub target_bps: u32,
    pub loss_permille: u16,
    pub rtt_ms: u16,
    pub jitter_us: u32,
    pub content: ContentMode,
    pub prefer_sharpness: bool,
}

impl Default for FidelityInput {
    fn default() -> Self {
        Self {
            display_w: 1920,
            display_h: 1080,
            fps: 60,
            target_bps: 20_000_000,
            loss_permille: 0,
            rtt_ms: 20,
            jitter_us: 0,
            content: ContentMode::Balanced,
            prefer_sharpness: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityPlan {
    pub render_w: u32,
    pub render_h: u32,
    pub fsr: FsrMode,
    pub sharpness: f32,
    pub min_qp: u8,
    pub aq_strength: u8,
    pub bits_per_pixel: f32,
    pub reason: &'static str,
}

impl QualityPlan {
    pub fn render_pixels(&self) -> u64 {
        u64::from(self.render_w) * u64::from(self.render_h)
    }

    pub fn tuning(&self) -> QualityTuning {
        QualityTuning {
            aq_strength: self.aq_strength,
            min_qp: self.min_qp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityTuning {
    pub aq_strength: u8,
    pub min_qp: u8,
}

impl Default for QualityTuning {
    fn default() -> Self {
        Self {
            aq_strength: 8,
            min_qp: 5,
        }
    }
}

pub const MIN_RENDER_DIM: u32 = 640;
pub const PLAN_HOLD: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct FidelityDetector {
    current: QualityPlan,
    pending: Option<(QualityPlan, Instant)>,
}

impl Default for FidelityDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl FidelityDetector {
    pub fn new() -> Self {
        Self {
            current: QualityPlan {
                render_w: 1920,
                render_h: 1080,
                fsr: FsrMode::Off,
                sharpness: 0.05,
                min_qp: 5,
                aq_strength: 8,
                bits_per_pixel: 0.16,
                reason: "startup",
            },
            pending: None,
        }
    }

    pub fn current(&self) -> QualityPlan {
        self.current
    }

    pub fn invalidate(&mut self) {
        self.pending = None;
    }

    pub fn update(&mut self, input: &FidelityInput, now: Instant) -> QualityPlan {
        let target = plan_for(input);
        if target == self.current {
            self.pending = None;
            return self.current;
        }
        let is_downgrade =
            target.render_pixels() < self.current.render_pixels() || target.fsr > self.current.fsr;
        if is_downgrade {
            self.pending = None;
            self.current = target;
            return self.current;
        }
        let (pending, since) = match self.pending {
            Some((pending, since)) if pending == target => (pending, since),
            _ => {
                self.pending = Some((target, now));
                return self.current;
            }
        };
        if now.saturating_duration_since(since) >= PLAN_HOLD {
            self.pending = None;
            self.current = pending;
        }
        self.current
    }
}

pub fn tuning_for(bitrate_bps: u32, fps: u16, width: u32, height: u32) -> QualityTuning {
    let fps = u32::from(fps.max(1));
    let pixels = u64::from(width.max(1)) * u64::from(height.max(1));
    let bits_per_frame = u64::from(bitrate_bps.max(200_000)) / u64::from(fps);
    let bpp = bits_per_frame as f64 / pixels.max(1) as f64;
    let scarcity = (1.0 - (bpp / 0.10)).clamp(0.0, 1.0) as f32;
    QualityTuning {
        aq_strength: (8.0 + scarcity * 7.0).round() as u8,
        min_qp: (5.0 + scarcity * 15.0) as u8,
    }
}

pub fn plan_for(input: &FidelityInput) -> QualityPlan {
    let fps = u32::from(input.fps.max(1));
    let display_w = even(input.display_w.max(MIN_RENDER_DIM));
    let display_h = even(input.display_h.max(MIN_RENDER_DIM));
    let display_pixels = u64::from(display_w) * u64::from(display_h);

    let bits_per_frame = u64::from(input.target_bps.max(200_000)) / u64::from(fps);
    let bpp_milli = input.content.min_bits_per_pixel_milli();
    let affordable_pixels = (bits_per_frame * 1000 / bpp_milli.max(1)).max(1);

    let content_scale = if input.prefer_sharpness { 0.85 } else { 1.0 };
    let needed = (affordable_pixels as f64 * content_scale) as u64;

    let mode = if input.loss_permille >= 30 && needed < display_pixels {
        match FsrMode::smallest_fit(display_pixels, needed) {
            FsrMode::Performance => FsrMode::Balanced,
            other => other,
        }
    } else {
        FsrMode::smallest_fit(display_pixels, needed)
    };

    let (render_w, render_h) = scaled(even(display_w), even(display_h), mode.render_scale());
    let render_pixels = u64::from(render_w) * u64::from(render_h);
    let bits_per_pixel = (bits_per_frame as f64 / render_pixels.max(1) as f64) as f32;

    let upscale_need = 1.0 - (render_pixels as f64 / display_pixels.max(1) as f64) as f32;
    let mut sharpness = 0.05 + upscale_need * 1.25;
    if input.content == ContentMode::Detail || input.prefer_sharpness {
        sharpness += 0.12;
    }
    let loss_penalty = (f32::from(input.loss_permille) / 1000.0) * 0.9;
    let jitter_penalty = (input.jitter_us as f32 / 40_000.0).min(0.2);
    sharpness = (sharpness - loss_penalty - jitter_penalty).clamp(0.0, 0.85);

    let scarcity = (1.0 - (bits_per_pixel / 0.10)).clamp(0.0, 1.0);
    let min_qp = (5.0 + scarcity * 15.0 + f32::from(input.loss_permille) * 0.15) as u8;
    let min_qp = min_qp.clamp(5, 24);

    let aq_strength = (6.0 + scarcity * 9.0) as u8;
    let aq_strength = aq_strength.clamp(1, 15);

    let reason = if mode == FsrMode::Off && sharpness <= 0.1 {
        "native resolution with headroom"
    } else if input.loss_permille >= 30 {
        "lossy path: conservative scaling"
    } else if input.prefer_sharpness || input.content == ContentMode::Detail {
        "detail content: sharpened reconstruction"
    } else if mode == FsrMode::Performance {
        "bitrate-starved: 2x spatial reconstruction"
    } else {
        "resolution scaled ahead of encoder starvation"
    };

    QualityPlan {
        render_w,
        render_h,
        fsr: mode,
        sharpness,
        min_qp,
        aq_strength,
        bits_per_pixel,
        reason,
    }
}

#[inline]
fn even(value: u32) -> u32 {
    value & !1
}

fn scaled(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let out_w = even(((w as f32 * scale).round() as u32).max(MIN_RENDER_DIM));
    let out_h = even(((h as f32 * scale).round() as u32).max(MIN_RENDER_DIM / 2));
    (out_w, out_h)
}

#[derive(Debug, Clone, Copy)]
pub struct RcasSharpener {
    pub sharpness: f32,
}

impl RcasSharpener {
    pub fn new(sharpness: f32) -> Self {
        Self {
            sharpness: sharpness.clamp(0.0, 1.0),
        }
    }

    pub fn from_plan(plan: &QualityPlan) -> Self {
        Self::new(plan.sharpness)
    }

    pub fn apply_nv12(&self, luma: &mut [u8], width: usize, height: usize, stride: usize) {
        if self.sharpness <= 0.001 || width < 3 || height < 3 || stride < width {
            return;
        }
        let len = luma.len();
        if stride * height > len {
            return;
        }
        let amount = self.sharpness * 5.0;
        let mut row = vec![0u8; width];

        for y in 1..height - 1 {
            let base = y * stride;
            let up = base - stride;
            let down = base + stride;
            for x in 1..width - 1 {
                let center = i16::from(luma[base + x]);
                let left = i16::from(luma[base + x - 1]);
                let right = i16::from(luma[base + x + 1]);
                let top = i16::from(luma[up + x]);
                let bottom = i16::from(luma[down + x]);

                let edge = 4 * center - (left + right + top + bottom);
                let local = (left - right).abs() + (top - bottom).abs();
                let limit = local.max(8);
                let contribution =
                    (f32::from(edge) * amount / 5.0 * (8.0 / f32::from(limit))).clamp(-63.0, 63.0);
                let value = center + contribution.round() as i16;
                row[x] = value.clamp(0, 255) as u8;
            }
            luma[base + 1..base + width - 1].copy_from_slice(&row[1..width - 1]);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct FsrShaderConstants {
    pub render_w: f32,
    pub render_h: f32,
    pub display_w: f32,
    pub display_h: f32,
    pub sharpness: f32,
    pub edge_softness: f32,
    pub exposure: f32,
    pub reserved: f32,
}

const _: () = assert!(core::mem::size_of::<FsrShaderConstants>() == 32);

impl FsrShaderConstants {
    pub fn for_plan(plan: &QualityPlan) -> Self {
        Self {
            render_w: plan.render_w as f32,
            render_h: plan.render_h as f32,
            display_w: (plan.render_w as f32 * plan.fsr.factor()).round(),
            display_h: (plan.render_h as f32 * plan.fsr.factor()).round(),
            sharpness: plan.sharpness,
            edge_softness: (plan.fsr.factor() - 1.0) * 0.35,
            exposure: 1.0,
            reserved: 0.0,
        }
    }

    pub fn to_bytes(self) -> [u8; 32] {
        *bytemuck::bytes_of(&self)
            .as_array()
            .expect("POD size is asserted above")
    }
}

#[derive(Debug, Clone)]
pub struct FrameGate {
    last_sent: Option<Instant>,
    last_hash: Option<u64>,
    pub keepalive: Duration,
}

impl Default for FrameGate {
    fn default() -> Self {
        Self {
            last_sent: None,
            last_hash: None,
            keepalive: Duration::from_secs(1),
        }
    }
}

impl FrameGate {
    pub fn should_send(
        &mut self,
        hash: u64,
        changed_pixels_permille: u16,
        bicubic_equivalent_bpp: f32,
        now: Instant,
    ) -> bool {
        let unchanged = self.last_hash == Some(hash) || changed_pixels_permille == 0;
        let keepalive_due = self
            .last_sent
            .is_none_or(|last| now.saturating_duration_since(last) >= self.keepalive);
        let abundant = bicubic_equivalent_bpp >= 0.12;
        if unchanged && !keepalive_due && !abundant {
            return false;
        }
        self.last_hash = Some(hash);
        self.last_sent = Some(now);
        true
    }

    pub fn reset(&mut self) {
        self.last_sent = None;
        self.last_hash = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(bps: u32, fps: u16, loss: u16) -> FidelityInput {
        FidelityInput {
            display_w: 1920,
            display_h: 1080,
            fps,
            target_bps: bps,
            loss_permille: loss,
            ..FidelityInput::default()
        }
    }

    #[test]
    fn headroom_keeps_native_resolution_and_minimal_sharpening() {
        let plan = plan_for(&input(40_000_000, 60, 0));
        assert_eq!(plan.fsr, FsrMode::Off);
        assert_eq!((plan.render_w, plan.render_h), (1920, 1080));
        assert!(plan.sharpness <= 0.12);
        assert_eq!(plan.min_qp, 5);
    }

    #[test]
    fn low_bitrate_scales_resolution_instead_of_frame_rate() {
        let plan = plan_for(&input(4_000_000, 60, 0));
        assert!(plan.fsr >= FsrMode::UltraQuality);
        assert!(plan.render_pixels() < 1920 * 1080);
        assert_eq!(plan.render_w % 2, 0);
        assert_eq!(plan.render_h % 2, 0);
    }
}
