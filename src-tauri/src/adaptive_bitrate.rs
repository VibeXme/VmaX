//! Delay-gradient bufferbloat detection and the NVENC rate-control coupling.
//!
//! This is the compact, standalone counterpart to [`crate::congestion`]: a
//! fixed 24-byte telemetry ACK the client emits every 16 ms, and a controller
//! that reacts to a *rise in one-way delay* rather than to loss.  Bufferbloat
//! is visible as `recv_delta - send_delta > 0` long before the first packet is
//! dropped, which is why a Parsec-class host reacts in one RTT instead of one
//! second.
//!
//! Wire format (little-endian, `bytemuck` POD, exactly 24 bytes):
//!
//! ```text
//! offset size field
//! 0      4    last_complete_frame_id : u32
//! 4      2    loss_permille          : u16
//! 6      2    reserved               : u16   (0)
//! 8      8    host_send_ts_us        : u64   (echo of VideoChunkHeader::timestamp_us)
//! 16     8    client_recv_ts_us      : u64   (client monotonic clock)
//! ```
//!
//! Integration: the client sends [`ClientTelemetryAck::encode_datagram`] from
//! its telemetry tick (the same 16 ms task that feeds
//! [`crate::congestion::FeedbackBuilder`]); the host feeds every decoded ACK to
//! [`AdaptiveBitrateController::on_ack`] and applies
//! [`AdaptiveBitrateController::nvenc_plan`] through
//! `encoder.set_bitrate_bps(plan.average_bit_rate)`.

use bytemuck::{Pod, Zeroable};
use std::time::{Duration, Instant};

/// The client reports every 16 ms (one 60 Hz frame).  120 Hz clients may halve
/// this; the filter is sample-count based, not time based, so the semantics do
/// not change.
pub const TELEMETRY_INTERVAL: Duration = Duration::from_millis(16);

/// Queue growth per sample that counts as bufferbloat.
pub const DELAY_GRADIENT_THRESHOLD_US: i64 = 4_000;

/// Consecutive over-threshold samples required before backing off.
pub const OVERUSE_STREAK: u32 = 3;

/// Hard floor for the target bitrate. The stream never goes below this, and the
/// encoder keeps the host's resolution at all times: rate control, frame pacing
/// and QP absorb the squeeze instead of the frame size.
pub const MIN_TARGET_BPS: u32 = 2_000_000;

/// Multiplicative decrease on detected bufferbloat.
pub const DECREASE_FACTOR: f32 = 0.75;

/// Additive increase while the queue is stable.
pub const INCREASE_BPS_PER_SEC: u32 = 400_000;

/// Loss at or above this (0.5%) suspends the ramp-up.
pub const RAMP_LOSS_PERMILLE: u16 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct ClientTelemetryAck {
    pub last_complete_frame_id: u32,
    pub loss_permille: u16,
    pub reserved: u16,
    pub host_send_ts_us: u64,
    pub client_recv_ts_us: u64,
}

const _: () = assert!(core::mem::size_of::<ClientTelemetryAck>() == 24);

impl ClientTelemetryAck {
    pub fn new(
        last_complete_frame_id: u32,
        loss_permille: u16,
        host_send_ts_us: u64,
        client_recv_ts_us: u64,
    ) -> Self {
        Self {
            last_complete_frame_id,
            loss_permille: loss_permille.min(1000),
            reserved: 0,
            host_send_ts_us,
            client_recv_ts_us,
        }
    }

    pub fn to_bytes(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out.copy_from_slice(bytemuck::bytes_of(self));
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (bytes.len() == 24).then(|| bytemuck::pod_read_unaligned(bytes))
    }

    /// Datagram with the VmaX channel tag, ready for
    /// `quinn::Connection::send_datagram`.
    pub fn encode_datagram(&self) -> bytes::Bytes {
        let mut out = bytes::BytesMut::with_capacity(25);
        out.extend_from_slice(&[crate::protocol::ChannelId::Feedback as u8]);
        out.extend_from_slice(&self.to_bytes());
        out.freeze()
    }

    pub fn decode_datagram(buf: &[u8]) -> Option<Self> {
        if buf.len() != 25
            || buf[0] != crate::protocol::ChannelId::Feedback as u8
            // The 248-byte GCC report also uses this channel; the length
            // discriminates them unambiguously.
            || buf.len() != 25
        {
            return None;
        }
        Self::from_bytes(&buf[1..])
    }

    /// One-way delay *change* for a pair of consecutive ACKs.  Clock offsets in
    /// `client_recv_ts_us` cancel in the subtraction.
    pub fn delay_delta_us(&self, previous: &Self) -> i64 {
        let send_delta = self.host_send_ts_us as i64 - previous.host_send_ts_us as i64;
        let recv_delta = self.client_recv_ts_us as i64 - previous.client_recv_ts_us as i64;
        // A clock step or a counter reset would otherwise look like a huge
        // queue; clamp to something a real path cannot exceed.
        (recv_delta - send_delta).clamp(-250_000, 250_000)
    }
}

/// WebRTC-style overuse detector: N consecutive samples with a positive delay
/// gradient, plus a short hold so a single reordered ACK cannot flip state.
#[derive(Debug, Clone)]
pub struct DelayGradientFilter {
    threshold_us: i64,
    required_streak: u32,
    streak: u32,
    last: Option<ClientTelemetryAck>,
    last_gradient_us: i64,
    overusing: bool,
    /// Longer than one ACK interval: prevents hysteresis chatter.
    hold: Duration,
    overuse_since: Option<Instant>,
}

impl Default for DelayGradientFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayGradientFilter {
    pub fn new() -> Self {
        Self {
            threshold_us: DELAY_GRADIENT_THRESHOLD_US,
            required_streak: OVERUSE_STREAK,
            streak: 0,
            last: None,
            last_gradient_us: 0,
            overusing: false,
            hold: Duration::from_millis(32),
            overuse_since: None,
        }
    }

    pub fn with_threshold(mut self, threshold_us: i64, streak: u32) -> Self {
        self.threshold_us = threshold_us.max(1);
        self.required_streak = streak.max(1);
        self
    }

    pub fn reset(&mut self) {
        self.streak = 0;
        self.last = None;
        self.last_gradient_us = 0;
        self.overusing = false;
        self.overuse_since = None;
    }

    /// Returns `true` while the path is considered bufferbloated.
    pub fn observe(&mut self, ack: &ClientTelemetryAck, now: Instant) -> bool {
        let Some(previous) = self.last.replace(*ack) else {
            return self.overusing;
        };
        let gradient = ack.delay_delta_us(&previous);
        self.last_gradient_us = gradient;

        if gradient > self.threshold_us {
            self.streak = self.streak.saturating_add(1);
        } else if gradient <= 0 {
            // A draining or flat queue clears the evidence immediately: there
            // is no reason to wait once the delay is gone.
            self.streak = 0;
            self.overusing = false;
            self.overuse_since = None;
            return false;
        } else {
            // Between 0 and the threshold: ambiguous, decay slowly.
            self.streak = self.streak.saturating_sub(1);
        }

        if self.streak >= self.required_streak {
            if !self.overusing {
                self.overuse_since = Some(now);
            }
            self.overusing = true;
        } else if self.overusing {
            // Keep the overuse verdict for the hold interval so the encoder is
            // not reconfigured twice in one RTT.
            if self
                .overuse_since
                .is_none_or(|since| now.saturating_duration_since(since) >= self.hold)
            {
                self.overusing = false;
                self.overuse_since = None;
            }
        }
        self.overusing
    }

    pub fn gradient_us(&self) -> i64 {
        self.last_gradient_us
    }

    pub fn streak(&self) -> u32 {
        self.streak
    }

    pub fn overusing(&self) -> bool {
        self.overusing
    }
}

/// What the host should do with the encoder after one ACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateAction {
    /// No encoder call: the target changed by less than the reconfigure
    /// threshold.
    Hold,
    /// Apply the new target.  Never an IDR.
    Reconfigure,
}

/// NVENC values to push through `NvEncReconfigureEncoder`.  `force_idr` is
/// always `false`: a rate change must not cost a keyframe spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencReconfigurePlan {
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    /// Exactly one frame of bits at the current fps; prevents the encoder from
    /// building a burst the pacer then has to absorb.
    pub vbv_buffer_size: u32,
    pub force_idr: bool,
}

impl NvencReconfigurePlan {
    pub fn for_target(target_bps: u32, fps: u16) -> Self {
        let fps = u32::from(fps.max(1));
        let frame_bits = (target_bps / fps).max(1);
        Self {
            average_bit_rate: target_bps,
            max_bit_rate: target_bps,
            vbv_buffer_size: frame_bits,
            force_idr: false,
        }
    }

    /// Bridge to the existing encoder plumbing
    /// (`encoder.set_bitrate_bps` / `NvEncReconfigureEncoder`).
    pub fn to_nvenc_params(
        self,
        ir: crate::encoder_recovery::IntraRefreshConfig,
    ) -> crate::encoder_recovery::NvencRateParams {
        let ir = ir.sanitized();
        crate::encoder_recovery::NvencRateParams {
            average_bit_rate: self.average_bit_rate,
            max_bit_rate: self.max_bit_rate,
            vbv_buffer_size: self.vbv_buffer_size,
            vbv_initial_delay: self.vbv_buffer_size,
            enable_intra_refresh: ir.enabled,
            intra_refresh_period: ir.period_frames,
            intra_refresh_cnt: ir.refresh_frames,
        }
    }
}

/// Host-side controller.  One instance per session; feed it every decoded ACK.
#[derive(Debug, Clone)]
pub struct AdaptiveBitrateController {
    min_bps: u32,
    max_bps: u32,
    target_bps: u32,
    last_applied_bps: u32,
    filter: DelayGradientFilter,
    fps: u16,
    /// Accumulates time so the ramp is exactly `INCREASE_BPS_PER_SEC`.
    ramp_credit_us: u64,
    last_ack: Option<Instant>,
    decreases: u32,
    increases: u32,
}

impl AdaptiveBitrateController {
    pub fn new(initial_bps: u32, max_bps: u32, fps: u16) -> Self {
        let max_bps = max_bps.max(MIN_TARGET_BPS);
        let target = initial_bps.clamp(MIN_TARGET_BPS, max_bps);
        Self {
            min_bps: MIN_TARGET_BPS,
            max_bps,
            target_bps: target,
            last_applied_bps: target,
            filter: DelayGradientFilter::new(),
            fps: fps.max(1),
            ramp_credit_us: 0,
            last_ack: None,
            decreases: 0,
            increases: 0,
        }
    }

    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    pub fn set_max_bps(&mut self, max_bps: u32) {
        self.max_bps = max_bps.max(self.min_bps);
        self.target_bps = self.target_bps.min(self.max_bps);
    }

    pub fn gradient_us(&self) -> i64 {
        self.filter.gradient_us()
    }

    pub fn is_overusing(&self) -> bool {
        self.filter.overusing()
    }

    /// Returns `(target, action)`.  `action` is `Reconfigure` only when the
    /// change is worth an NVENC round trip (>= 3% or a floor/ceiling hit).
    pub fn on_ack(&mut self, ack: &ClientTelemetryAck, now: Instant) -> (u32, RateAction) {
        let elapsed = self
            .last_ack
            .replace(now)
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::ZERO);
        let overusing = self.filter.observe(ack, now);

        if overusing {
            let reduced = ((self.target_bps as f64) * f64::from(DECREASE_FACTOR)) as u32;
            self.target_bps = reduced.max(self.min_bps);
            self.decreases = self.decreases.saturating_add(1);
            // Drop any accrued ramp credit: the next increase must be earned
            // from a clean gradient, not from time spent before the backoff.
            self.ramp_credit_us = 0;
            return (self.target_bps, self.maybe_reconfigure());
        }

        if self.filter.gradient_us() <= 0
            && ack.loss_permille < RAMP_LOSS_PERMILLE
            && self.target_bps < self.max_bps
        {
            self.ramp_credit_us = self
                .ramp_credit_us
                .saturating_add(elapsed.as_micros().min(1_000_000) as u64);
            let credit_bps = (u64::from(INCREASE_BPS_PER_SEC) * self.ramp_credit_us) / 1_000_000;
            if credit_bps > 0 {
                let step = credit_bps.min(u64::from(self.max_bps - self.target_bps)) as u32;
                self.target_bps += step;
                self.ramp_credit_us = 0;
                self.increases = self.increases.saturating_add(1);
            }
        } else {
            self.ramp_credit_us = 0;
        }

        (self.target_bps, self.maybe_reconfigure())
    }

    /// Explicit loss backoff for the case where the ACK loss field is the only
    /// signal (e.g. a burst that completed before the delay filter reacted).
    pub fn on_loss(&mut self, loss_permille: u16) -> u32 {
        if loss_permille >= 30 {
            let reduced = ((self.target_bps as f64) * 0.80) as u32;
            self.target_bps = reduced.max(self.min_bps);
            self.ramp_credit_us = 0;
            self.decreases = self.decreases.saturating_add(1);
        }
        self.target_bps
    }

    fn maybe_reconfigure(&mut self) -> RateAction {
        let threshold = (self.last_applied_bps / 32).max(100_000);
        let changed = self.target_bps.abs_diff(self.last_applied_bps) >= threshold
            || (self.target_bps == self.min_bps && self.last_applied_bps != self.min_bps)
            || (self.target_bps == self.max_bps && self.last_applied_bps != self.max_bps);
        if changed {
            self.last_applied_bps = self.target_bps;
            RateAction::Reconfigure
        } else {
            RateAction::Hold
        }
    }

    pub fn nvenc_plan(&self) -> NvencReconfigurePlan {
        NvencReconfigurePlan::for_target(self.target_bps, self.fps)
    }

    pub fn stats(&self) -> (u32, u32, i64) {
        (self.decreases, self.increases, self.filter.gradient_us())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(send_us: u64, recv_us: u64, loss: u16) -> ClientTelemetryAck {
        ClientTelemetryAck::new(1, loss, send_us, recv_us)
    }

    #[test]
    fn ack_is_exactly_24_bytes_and_round_trips() {
        let ack = ClientTelemetryAck::new(77, 12, 1_234_567, 7_654_321);
        let bytes = ack.to_bytes();
        assert_eq!(bytes.len(), 24);
        assert_eq!(ClientTelemetryAck::from_bytes(&bytes), Some(ack));
        assert_eq!(ClientTelemetryAck::from_bytes(&bytes[..23]), None);

        let dgram = ack.encode_datagram();
        assert_eq!(dgram.len(), 25);
        assert_eq!(ClientTelemetryAck::decode_datagram(&dgram), Some(ack));
    }

    #[test]
    fn delay_delta_cancels_clock_offset() {
        let a = ack(1_000, 500_000, 0);
        let b = ack(17_000, 511_000, 0); // 16 ms send, 11 ms recv
        assert_eq!(b.delay_delta_us(&a), -5_000);
    }

    #[test]
    fn three_growing_delay_samples_trigger_overuse() {
        let now = Instant::now();
        let mut filter = DelayGradientFilter::new();
        // send +16 ms, recv +21 ms -> +5 ms per sample, three times.
        for i in 0..4u64 {
            let a = ack(i * 16_000, i * 21_000, 0);
            let over = filter.observe(&a, now + Duration::from_millis(i * 16));
            assert_eq!(over, i == 3, "sample {i}");
        }
        assert!(filter.overusing());
    }

    #[test]
    fn two_samples_are_not_enough() {
        let now = Instant::now();
        let mut filter = DelayGradientFilter::new();
        // Two gradient samples (three ACKs) are one short of the streak.
        for i in 0..3u64 {
            let a = ack(i * 16_000, i * 25_000, 0);
            assert!(!filter.observe(&a, now + Duration::from_millis(i * 16)));
        }
    }

    #[test]
    fn flat_delay_clears_overuse_immediately() {
        let now = Instant::now();
        let mut filter = DelayGradientFilter::new();
        for i in 0..4u64 {
            filter.observe(&ack(i * 10_000, i * 20_000, 0), now);
        }
        assert!(filter.overusing());
        assert!(!filter.observe(&ack(100_000, 110_000, 0), now));
    }

    #[test]
    fn bufferbloat_cuts_target_by_25_percent_without_idr() {
        let now = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(20_000_000, 80_000_000, 60);
        let mut target = ctrl.target_bps();
        for i in 0..4u64 {
            let (next, action) = ctrl.on_ack(&ack(i * 16_000, i * 21_000, 0), now + Duration::from_millis(i * 16));
            target = next;
            if i == 3 {
                assert_eq!(action, RateAction::Reconfigure);
            }
        }
        assert_eq!(target, 15_000_000);
        assert!(!ctrl.nvenc_plan().force_idr);
    }

    #[test]
    fn target_never_drops_below_two_mbps() {
        let now = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(3_000_000, 80_000_000, 60);
        for round in 0..20u64 {
            for i in 0..3u64 {
                let base = (round * 3 + i) * 16_000;
                // Arrival slips further behind every sample: sustained queue
                // growth with zero loss.
                let recv = base + 40_000 + base / 2;
                ctrl.on_ack(&ack(base, recv, 0), now + Duration::from_millis(base / 1000));
            }
        }
        assert_eq!(ctrl.target_bps(), MIN_TARGET_BPS);
    }

    #[test]
    fn clean_path_ramps_at_400_kbps_per_second() {
        let now = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(10_000_000, 80_000_000, 60);
        // One second of 16 ms ACKs with a perfectly flat delay.
        for i in 0..63u64 {
            ctrl.on_ack(
                &ack(i * 16_000, i * 16_000, 0),
                now + Duration::from_millis(i * 16),
            );
        }
        let target = ctrl.target_bps();
        assert!(
            (10_300_000..=10_500_000).contains(&target),
            "expected ~10.4 Mbps after one second, got {target}"
        );
    }

    #[test]
    fn loss_suspends_the_ramp() {
        let now = Instant::now();
        let mut ctrl = AdaptiveBitrateController::new(10_000_000, 80_000_000, 60);
        let start = ctrl.target_bps();
        for i in 0..63u64 {
            ctrl.on_ack(
                &ack(i * 16_000, i * 16_000, 9),
                now + Duration::from_millis(i * 16),
            );
        }
        assert_eq!(ctrl.target_bps(), start);
    }

    #[test]
    fn vbv_is_one_frame_of_bits() {
        let plan = NvencReconfigurePlan::for_target(12_000_000, 120);
        assert_eq!(plan.vbv_buffer_size, 100_000);
        assert_eq!(plan.max_bit_rate, plan.average_bit_rate);
        assert!(!plan.force_idr);
    }
}
