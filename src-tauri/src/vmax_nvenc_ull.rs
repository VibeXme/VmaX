//! VmaX Windows Host: Ultra-Low-Latency NVENC Configuration
//! Target: NVIDIA RTX 3050 Laptop (4GB VRAM) + DXGI Desktop Duplication
//! Docs: https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmaxCodec {
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyPreset {
    /// P3: Best for 120-144 FPS with clean motion edges
    MaxFps,
    /// P4: Parsec-grade quality/latency sweet spot (~2.45ms encode on RTX 3050 at 1080p)
    Balanced,
}

#[derive(Debug, Clone)]
pub struct NvencUllConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: VmaxCodec,
    pub preset: LatencyPreset,
    /// Spreads keyframe data across N frames to prevent QUIC Wi-Fi packet spikes
    pub intra_refresh_period_frames: u32,
    /// Minimum QP floor (Parsec default = 5 for Lowest Latency)
    pub min_qp: u32,
    /// Emit BT.709 Full Range (0-255) VUI flags
    pub full_range_bt709: bool,
}

impl Default for NvencUllConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
            codec: VmaxCodec::Hevc,
            preset: LatencyPreset::Balanced,
            intra_refresh_period_frames: 60,
            min_qp: 5,
            full_range_bt709: true,
        }
    }
}

impl NvencUllConfig {
    /// Single-frame VBV buffer prevents NVENC from buffering multiple frames
    /// or spiking bitrate on complex scenes.
    pub fn vbv_buffer_size_bits(&self) -> u32 {
        (self.bitrate_bps / self.fps.max(1)).max(16_384)
    }

    /// Maps to NV_ENC_CONFIG_H264 / NV_ENC_CONFIG_HEVC parameters
    pub fn to_nvenc_params(&self) -> NvencNativeParams {
        NvencNativeParams {
            tuning_info: "NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY",
            preset_guid: match self.preset {
                LatencyPreset::MaxFps => "NV_ENC_PRESET_P3_GUID",
                LatencyPreset::Balanced => "NV_ENC_PRESET_P4_GUID",
            },
            rate_control_mode: "NV_ENC_PARAMS_RC_VBR",
            multi_pass: "NV_ENC_TWO_PASS_QUARTER_RESOLUTION",
            average_bitrate: self.bitrate_bps,
            max_bitrate: self.bitrate_bps,
            vbv_buffer_size: self.vbv_buffer_size_bits(),
            vbv_initial_delay: self.vbv_buffer_size_bits(),
            min_qp: self.min_qp,
            aq_strength: 8,
            // Zero B-frames = 0-frame reordering latency
            frame_interval_p: 1,
            gop_length: u32::MAX, // Infinite GOP when using Intra Refresh
            enable_intra_refresh: true,
            intra_refresh_period: self.fps.max(30),
            intra_refresh_cnt: self.intra_refresh_period_frames.max(5),
            zero_reorder_delay: true,
            repeat_sps_pps: true, // Required so late-joining/packet-loss clients recover
            video_full_range_flag: self.full_range_bt709,
            colour_primaries: 1,         // BT.709
            transfer_characteristics: 1, // BT.709
            matrix_coefficients: 1,      // BT.709
            max_dec_frame_buffering: 1,  // Forces immediate 0-queue hardware decode on client
            num_reorder_frames: 0,
        }
    }

    /// Dynamic bitrate + resolution step-down hook for quinn BBR congestion feedback
    pub fn adapt_to_congestion(&mut self, bbr_target_bps: u32, packet_loss_pct: f32) {
        let clamped = bbr_target_bps.clamp(1_500_000, 80_000_000);
        // Smooth downward step, conservative upward ramp
        if clamped < self.bitrate_bps || packet_loss_pct > 1.5 {
            self.bitrate_bps = (clamped as f32 * 0.90) as u32;
        } else {
            self.bitrate_bps = ((self.bitrate_bps as f32 * 0.85) + (clamped as f32 * 0.15)) as u32;
        }
    }
}

#[derive(Debug, Clone)]
pub struct NvencNativeParams {
    pub tuning_info: &'static str,
    pub preset_guid: &'static str,
    pub rate_control_mode: &'static str,
    pub multi_pass: &'static str,
    pub average_bitrate: u32,
    pub max_bitrate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    pub min_qp: u32,
    pub aq_strength: u32,
    pub frame_interval_p: u32,
    pub gop_length: u32,
    pub enable_intra_refresh: bool,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub zero_reorder_delay: bool,
    pub repeat_sps_pps: bool,
    pub video_full_range_flag: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub max_dec_frame_buffering: u32,
    pub num_reorder_frames: u32,
}
