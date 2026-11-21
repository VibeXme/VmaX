//! The single source of truth for the host NVENC configuration.
//!
//! `encoder.rs` fills the raw `NV_ENC_*` structs from the values produced
//! here, so the tuning knobs live in one testable, platform-independent
//! place instead of being scattered through the encoder's unsafe setup.
//!
//! The plan encodes the two hard requirements for streaming:
//! * **No reordering.** `frameIntervalP = 1`, `numReorderFrames = 0`,
//!   `maxDecFrameBuffering = 1`. A decoder may not hold a decoded picture
//!   waiting for a B-frame, because there are none.
//! * **Bounded bitrate.** CBR with a one-frame VBV, no lookahead, and a
//!   rolling intra-refresh instead of periodic IDR spikes.

/// Encoder tier: P4 is the balanced quality/latency preset the SDK documents
/// for 60-120 FPS streaming; P3 gives more quality when the frame budget is
/// comfortable (detail pacing at 60 FPS or below).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    P3,
    P4,
}

/// Persisted NVENC settings. The decoder pipeline always uses an IP-only GOP.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NvencSettings {
    pub preset: u8,
    pub vbr: bool,
    pub vbv_frames: u8,
    pub aq_strength: u8,
    pub min_qp: u8,
    pub quarter_pass: bool,
    pub intra_refresh: bool,
    pub refresh_period: u32,
    pub refresh_frames: u32,
}

impl Default for NvencSettings {
    fn default() -> Self {
        Self {
            preset: 4,
            vbr: false,
            vbv_frames: 1,
            aq_strength: 8,
            min_qp: 5,
            quarter_pass: true,
            intra_refresh: true,
            // One refreshing row per frame: the intra band is thin enough to
            // be invisible, and because it never stops, the bit cost is spread
            // evenly instead of landing as a quality pulse every refresh
            // period. A wide band (the old 60/5 default) spent the same total
            // bits in a burst, and the rest of the picture visibly softened
            // while it swept past.
            refresh_period: 120,
            refresh_frames: 1,
        }
    }
}

impl NvencSettings {
    pub fn vbv_bits(self, bitrate_bps: u32, fps: u32) -> u32 {
        (bitrate_bps / fps.max(1)).max(1)
    }
}

/// Rate-control knobs that make or break latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateControl {
    /// The streaming plan uses constant bitrate with one frame of VBV.
    pub vbr: bool,
    /// Per-frame VBV cap in bits (`averageBitRate / fps`).
    pub vbv_bits: u32,
    /// Floor QP so refinement passes can reach near-lossless text.
    pub min_qp_inter_p: u8,
    pub min_qp_intra: u8,
    /// Spatial adaptive quantization strength (0-15 in the SDK header).
    pub aq_strength: u8,
    /// Must stay 0: lookahead buffers frames before encoding.
    pub lookahead_depth: u16,
    /// `NV_ENC_TWO_PASS_QUARTER_RESOLUTION`. A quarter-resolution first pass
    /// costs no extra latency and materially improves quality per bit.
    pub multi_pass_quarter_res: bool,
}

/// BT.709 full-range VUI, written into the bitstream so hardware decoders and
/// the client shader agree on 0-255 BT.709 without out-of-band negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VuiBt709FullRange {
    pub video_signal_type_present: bool,
    pub video_full_range: bool,
    pub colour_description_present: bool,
    /// H.273 code points, all 1 = BT.709.
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// MPEG-2 chroma siting: left, top-left.
    pub chroma_sample_location_flag: bool,
    pub chroma_sample_location_top: u8,
    pub chroma_sample_location_bot: u8,
    pub bitstream_restriction: bool,
    /// Forces every hardware decoder out of its DPB prefetch.
    pub max_dec_frame_buffering: u8,
    pub num_reorder_frames: u8,
}

impl VuiBt709FullRange {
    pub const fn bt709_full_range() -> Self {
        Self {
            video_signal_type_present: true,
            video_full_range: true,
            colour_description_present: true,
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            chroma_sample_location_flag: true,
            chroma_sample_location_top: 0,
            chroma_sample_location_bot: 0,
            bitstream_restriction: true,
            max_dec_frame_buffering: 1,
            num_reorder_frames: 0,
        }
    }
}

/// Intra-refresh instead of periodic IDRs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntraRefresh {
    pub enabled: bool,
    pub period: u32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvencPlan {
    pub preset: Preset,
    /// `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`.
    pub tuning_ultra_low_latency: bool,
    pub frame_interval_p: u32,
    pub gop_length: u32,
    pub intra_refresh: IntraRefresh,
    pub rate_control: RateControl,
    pub vui: VuiBt709FullRange,
    pub repeat_sps_pps: bool,
    pub num_reorder_frames: u8,
    pub max_dec_frame_buffering: u8,
}

impl NvencPlan {
    /// Build the streaming plan for a codec at `fps` and `bitrate_bps`.
    /// `detail` selects P3 (quality) instead of P4 when the frame budget is
    /// generous.
    pub fn streaming(fps: u32, bitrate_bps: u32, detail: bool) -> Self {
        let fps = fps.max(1);
        let frame_bits = (bitrate_bps / fps).max(1);
        Self {
            preset: if detail { Preset::P3 } else { Preset::P4 },
            tuning_ultra_low_latency: true,
            frame_interval_p: 1,
            gop_length: u32::MAX,
            intra_refresh: IntraRefresh {
                enabled: true,
                // One full refresh every two seconds at the configured rate.
                period: fps * 2,
                count: 5,
            },
            rate_control: RateControl {
                vbr: false,
                vbv_bits: frame_bits,
                min_qp_inter_p: 5,
                min_qp_intra: 5,
                aq_strength: 8,
                lookahead_depth: 0,
                multi_pass_quarter_res: true,
            },
            vui: VuiBt709FullRange::bt709_full_range(),
            repeat_sps_pps: true,
            num_reorder_frames: 0,
            max_dec_frame_buffering: 1,
        }
    }

    /// Recompute the one-frame VBV after a bitrate or frame-rate change.
    pub fn set_bitrate(&mut self, bitrate_bps: u32, fps: u32) {
        self.rate_control.vbv_bits = (bitrate_bps / fps.max(1)).max(1);
    }
}

/// Codec id carried in the frame telemetry header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CodecId {
    H264 = 0,
    Hevc = 1,
    Hevc444 = 2,
    HevcP010 = 3,
    Av1 = 4,
}

impl CodecId {
    pub fn for_codec(codec: crate::protocol::VideoCodec) -> Self {
        match codec {
            crate::protocol::VideoCodec::H264 => Self::H264,
            crate::protocol::VideoCodec::Hevc => Self::Hevc,
            crate::protocol::VideoCodec::HevcMain10 => Self::HevcP010,
            crate::protocol::VideoCodec::Hevc444 => Self::Hevc444,
            crate::protocol::VideoCodec::Av1 => Self::Av1,
        }
    }

    /// The byte written into [`crate::telemetry::FrameTelemetry::codec_id`].
    pub fn wire_id(self) -> u8 {
        self as u8
    }

    /// Map the wire `codec_id` byte back to a label for the telemetry bar.
    pub fn label_for_wire(id: u8) -> &'static str {
        match id {
            0 => "H.264",
            1 => "H.265",
            2 => "H.265 4:4:4",
            3 => "H.265 10-bit",
            4 => "AV1",
            _ => "H.265",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "H.265",
            Self::Hevc444 => "H.265 4:4:4",
            Self::HevcP010 => "H.265 10-bit",
            Self::Av1 => "AV1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_plan_has_zero_reordering() {
        let plan = NvencPlan::streaming(60, 20_000_000, false);
        assert_eq!(plan.frame_interval_p, 1);
        assert_eq!(plan.num_reorder_frames, 0);
        assert_eq!(plan.max_dec_frame_buffering, 1);
        assert_eq!(plan.vui.num_reorder_frames, 0);
        assert_eq!(plan.vui.max_dec_frame_buffering, 1);
        assert!(plan.tuning_ultra_low_latency);
        assert_eq!(plan.preset, Preset::P4);
    }

    #[test]
    fn legacy_vbv_settings_use_one_frame() {
        let settings = NvencSettings {
            vbv_frames: 4,
            ..Default::default()
        };
        assert_eq!(settings.vbv_bits(24_000_000, 60), 400_000);
        assert_eq!(settings.vbv_bits(12_000_000, 120), 100_000);
    }

    #[test]
    fn vbv_is_exactly_one_frame_of_bits() {
        let plan = NvencPlan::streaming(60, 18_000_000, false);
        assert_eq!(plan.rate_control.vbv_bits, 300_000);
        let mut plan = plan;
        plan.set_bitrate(12_000_000, 120);
        assert_eq!(plan.rate_control.vbv_bits, 100_000);
    }

    #[test]
    fn vui_is_bt709_full_range() {
        let vui = VuiBt709FullRange::bt709_full_range();
        assert!(vui.video_signal_type_present);
        assert!(vui.video_full_range);
        assert!(vui.colour_description_present);
        assert_eq!(vui.colour_primaries, 1);
        assert_eq!(vui.transfer_characteristics, 1);
        assert_eq!(vui.matrix_coefficients, 1);
        assert!(vui.bitstream_restriction);
    }

    #[test]
    fn rate_control_never_enables_lookahead() {
        let plan = NvencPlan::streaming(120, 40_000_000, true);
        assert_eq!(plan.rate_control.lookahead_depth, 0);
        assert!(!plan.rate_control.vbr);
        assert!(plan.rate_control.multi_pass_quarter_res);
        assert_eq!(plan.rate_control.aq_strength, 8);
        assert_eq!(plan.preset, Preset::P3);
    }

    #[test]
    fn intra_refresh_covers_two_seconds_without_idr_spikes() {
        let plan = NvencPlan::streaming(60, 10_000_000, false);
        assert!(plan.intra_refresh.enabled);
        assert_eq!(plan.intra_refresh.period, 120);
        assert_eq!(plan.intra_refresh.count, 5);
        assert_eq!(plan.gop_length, u32::MAX);
    }

    #[test]
    fn bitrate_changes_preserve_the_minimum_qp() {
        let mut plan = NvencPlan::streaming(60, 24_000_000, false);
        plan.set_bitrate(12_000_000, 60);
        assert_eq!(plan.rate_control.min_qp_inter_p, 5);
        assert_eq!(plan.rate_control.min_qp_intra, 5);
    }

    #[test]
    fn codec_ids_match_the_wire_values() {
        use crate::protocol::VideoCodec;
        assert_eq!(CodecId::for_codec(VideoCodec::H264) as u8, 0);
        assert_eq!(CodecId::for_codec(VideoCodec::Hevc) as u8, 1);
        assert_eq!(CodecId::for_codec(VideoCodec::HevcMain10) as u8, 3);
        assert_eq!(CodecId::HevcP010.label(), "H.265 10-bit");
    }
}
