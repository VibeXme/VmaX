//! Rate-control policy for the non-NVENC encoder paths.
//!
//! The NVENC plan in [`crate::nvenc`] is explicit: VBR with a two-frame VBV, a
//! peak above the average so a busy frame can borrow bits, and no lookahead.
//! The Media Foundation path — which is what every AMD, Intel and
//! software-encode host actually runs — was not: it negotiated plain CBR with a
//! vendor default GOP. That combination is the "liquid" look: every detailed
//! frame is clipped to `bitrate / fps`, the rate controller answers with a QP
//! spike on exactly the frames that need detail, and a short default GOP keeps
//! spending bits on I-frames that add nothing to a progressive stream.
//!
//! This module is the single place that decides those numbers, so the vendor
//! MFT setup only has to push them through `ICodecAPI`. It is deliberately
//! platform-independent and unit-tested: the Windows encoder cannot be tested
//! on a CI box, the policy can.

/// `eAVEncCommonRateControlMode` values.
pub const RC_CBR: u32 = 0;
/// Peak-constrained VBR: the average is held, individual frames may spend up to
/// `max_bitrate`. This is the closest Media Foundation gets to the NVENC plan.
pub const RC_PEAK_CONSTRAINED_VBR: u32 = 1;

/// Two seconds of pictures between IDRs, matching the NVENC intra-refresh
/// period. The client can always request a keyframe, so a long GOP costs
/// nothing in recovery time but saves the periodic I-frame bit spike.
const GOP_SECONDS: u32 = 2;
/// Sanity bounds for the GOP on unusual frame rates.
const MIN_GOP: u32 = 30;
const MAX_GOP: u32 = 300;

/// The `ICodecAPI` values a vendor MFT needs for streaming-quality output.
///
/// Everything here is derived from the *media* parameters (bitrate, frame
/// rate, content intent) rather than from device capability, so an AMD, Intel
/// or software encoder all get the same shape of rate control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MftRateControl {
    /// `CODECAPI_AVEncCommonRateControlMode`.
    pub mode: u32,
    /// `CODECAPI_AVEncCommonMeanBitRate`, bits per second.
    pub mean_bitrate_bps: u32,
    /// `CODECAPI_AVEncCommonMaxBitRate`, bits per second.
    pub max_bitrate_bps: u32,
    /// `CODECAPI_AVEncCommonBufferSize`, in bits.
    pub buffer_size_bits: u32,
    /// `CODECAPI_AVEncCommonQualityVsSpeed`, 0 (speed) to 100 (quality).
    pub quality_vs_speed: u32,
    /// `CODECAPI_AVEncMPVGOPSize`, in frames.
    pub gop_length: u32,
}

impl MftRateControl {
    /// Rate control for a stream at `bitrate_bps` and `fps`.
    ///
    /// `detail` selects the quality-leaning encoder effort. It is the same
    /// signal that picks NVENC P3 over P4: text, IDEs and UI benefit from the
    /// slower search, and the frame budget is only generous enough for it when
    /// the client asked for detail pacing.
    pub fn for_stream(bitrate_bps: u32, fps: u16, detail: bool) -> Self {
        let fps = u32::from(fps.max(1));
        let mean = bitrate_bps.max(1);
        Self {
            mode: RC_PEAK_CONSTRAINED_VBR,
            mean_bitrate_bps: mean,
            max_bitrate_bps: peak_for(mean),
            buffer_size_bits: buffer_for(mean, fps),
            quality_vs_speed: if detail { 85 } else { 70 },
            gop_length: gop_for(fps),
        }
    }

    /// Same policy after a congestion-driven bitrate change.
    ///
    /// Note what does *not* change: the mode, the quality/speed bias and the
    /// GOP. A bitrate drop must not silently make the encoder thriftier on the
    /// frames that remain — that is how a temporary dip turns into a permanent
    /// one.
    pub fn with_bitrate(self, bitrate_bps: u32, fps: u16) -> Self {
        let fps = u32::from(fps.max(1));
        let mean = bitrate_bps.max(1);
        Self {
            mean_bitrate_bps: mean,
            max_bitrate_bps: peak_for(mean),
            buffer_size_bits: buffer_for(mean, fps),
            ..self
        }
    }

    /// True when this plan asks for something other than plain CBR.
    pub fn is_vbr(self) -> bool {
        self.mode != RC_CBR
    }
}

/// Twice the average, the same head-start NVENC gets from
/// [`crate::nvenc::NvencSettings::max_bitrate`]. A busy frame borrows, the next
/// quiet frame repays. `mean` is already clamped to at least 1 bit/s.
fn peak_for(mean: u32) -> u32 {
    mean.saturating_mul(2).max(mean.saturating_add(1))
}

/// Two frame-times of rate budget: enough for a peak frame without adding a
/// reorder queue (there are no B-frames on this path either).
fn buffer_for(bitrate_bps: u32, fps: u32) -> u32 {
    (bitrate_bps / fps.max(1)).max(1).saturating_mul(2)
}

fn gop_for(fps: u32) -> u32 {
    (fps.max(1) * GOP_SECONDS).clamp(MIN_GOP, MAX_GOP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_plan_is_peak_constrained_vbr_not_cbr() {
        let plan = MftRateControl::for_stream(20_000_000, 60, false);
        assert_eq!(plan.mode, RC_PEAK_CONSTRAINED_VBR);
        assert!(plan.is_vbr());
        assert_ne!(plan.mode, RC_CBR);
    }

    #[test]
    fn busy_frames_can_spend_twice_the_average() {
        let plan = MftRateControl::for_stream(20_000_000, 60, false);
        assert_eq!(plan.mean_bitrate_bps, 20_000_000);
        assert_eq!(plan.max_bitrate_bps, 40_000_000);
        // Degenerate input must not produce a zero or overflowing ceiling.
        let tiny = MftRateControl::for_stream(0, 60, false);
        assert!(tiny.max_bitrate_bps > tiny.mean_bitrate_bps);
        let huge = MftRateControl::for_stream(u32::MAX, 60, false);
        assert!(huge.max_bitrate_bps >= u32::MAX);
    }

    #[test]
    fn buffer_spans_two_frame_times() {
        assert_eq!(buffer_for(24_000_000, 60), 800_000);
        // A zero frame rate must not divide by zero.
        assert!(buffer_for(24_000_000, 0) > 0);
    }

    #[test]
    fn gop_is_two_seconds_and_bounded() {
        assert_eq!(gop_for(60), 120);
        assert_eq!(gop_for(144), 288);
        assert_eq!(gop_for(1), MIN_GOP);
        assert_eq!(gop_for(0), MIN_GOP);
    }

    #[test]
    fn detail_pacing_asks_for_the_slower_search() {
        let balanced = MftRateControl::for_stream(20_000_000, 60, false);
        let detail = MftRateControl::for_stream(20_000_000, 60, true);
        assert!(detail.quality_vs_speed > balanced.quality_vs_speed);
        // Effort is not allowed to exceed the documented 0-100 range.
        assert!(detail.quality_vs_speed <= 100);
    }

    #[test]
    fn a_bitrate_change_keeps_the_policy_and_rescales_the_peaks() {
        let plan = MftRateControl::for_stream(20_000_000, 60, true);
        let reduced = plan.with_bitrate(8_000_000, 60);
        assert_eq!(reduced.mode, plan.mode);
        assert_eq!(reduced.quality_vs_speed, plan.quality_vs_speed);
        assert_eq!(reduced.gop_length, plan.gop_length);
        assert_eq!(reduced.mean_bitrate_bps, 8_000_000);
        assert_eq!(reduced.max_bitrate_bps, 16_000_000);
        // The buffer follows the frame rate, not the old bitrate.
        assert_eq!(reduced.buffer_size_bits, buffer_for(8_000_000, 60));
        let faster = reduced.with_bitrate(8_000_000, 120);
        assert!(faster.buffer_size_bits < reduced.buffer_size_bits);
    }
}
