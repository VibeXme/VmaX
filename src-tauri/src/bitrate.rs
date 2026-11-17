use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const MIN_VIDEO_BITRATE_BPS: u32 = 1_000_000;
pub const MAX_VIDEO_BITRATE_BPS: u32 = 200_000_000;
pub const DEFAULT_INITIAL_BITRATE_BPS: u32 = 20_000_000; // 20 Mbps

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FramePacing {
    #[default]
    Balanced,
    Motion,
    Detail,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
pub struct CaptureCadence {
    tier: usize,
    pending: Option<(usize, Instant)>,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Default)]
pub struct CaptureResolution {
    limited: bool,
    pending: Option<Instant>,
}

#[cfg(any(target_os = "windows", test))]
impl CaptureResolution {
    /// The capture resolution is fixed by the host's configured stream size.
    ///
    /// Bitrate pressure is absorbed by rate control, frame pacing and QP — it
    /// never shrinks the encoded frame. Downscaling on a thin uplink used to
    /// cost a full capture/encoder rebuild (plus an IDR) and left the client on
    /// a permanently softer image; keeping the host resolution means the client
    /// gets the sharpest picture the link can carry at any moment.
    pub fn ceiling(
        &mut self,
        _now: Instant,
        _bitrate: u32,
        _fps: u16,
        width: u32,
        height: u32,
        _mode: FramePacing,
    ) -> (u32, u32) {
        self.limited = false;
        self.pending = None;
        (width, height)
    }
}

#[cfg(test)]
#[test]
fn capture_resolution_keeps_the_host_size_under_any_bitrate() {
    let mut adaptive = CaptureResolution::default();
    let now = Instant::now();
    let mut size = |secs, bps| {
        adaptive.ceiling(
            now + Duration::from_secs(secs),
            bps,
            60,
            1920,
            1080,
            FramePacing::Balanced,
        )
    };
    // Even a two-second squeeze at 3 Mbps leaves the host resolution alone.
    assert_eq!(size(0, 3_000_000), (1920, 1080));
    assert_eq!(size(2, 3_000_000), (1920, 1080));
    assert_eq!(size(11, 8_000_000), (1920, 1080));
    // 4K stays 4K regardless of pacing mode.
    let (w, h) = adaptive.ceiling(now, 1_000_000, 60, 3840, 2160, FramePacing::Motion);
    assert_eq!((w, h), (3840, 2160));
}

#[cfg(any(target_os = "windows", test))]
impl CaptureCadence {
    pub fn fps(&mut self, now: Instant, bitrate: u32, target: u16, mode: FramePacing) -> u16 {
        let thresholds = match mode {
            FramePacing::Motion => [1_200_000, 1_800_000, 2_800_000],
            FramePacing::Balanced => [1_250_000, 1_750_000, 3_000_000],
            FramePacing::Detail => [2_000_000, 4_000_000, 7_000_000],
        };
        let wanted = thresholds
            .iter()
            .rev()
            .enumerate()
            .take_while(|&(index, &threshold)| {
                let threshold = if self.tier > index {
                    threshold * 115 / 100
                } else {
                    threshold
                };
                bitrate <= threshold
            })
            .count();

        // When target > 60 (e.g. 120Hz Moto G64 / 144Hz monitor), keep uncapped target
        // on clean paths and floor at 60 FPS so it never dips to 45/30 FPS.
        let caps = if matches!(mode, FramePacing::Motion) {
            [target, target, target, 30]
        } else if target > 60 {
            [target, (target * 3 / 4).max(60), 60, 60]
        } else {
            [target, 45, 30, 20]
        };

        if wanted == self.tier {
            self.pending = None;
        } else {
            let (previous, since) = *self.pending.get_or_insert((wanted, now));
            if wanted != previous {
                self.pending = Some((wanted, now));
            } else {
                let hold = if wanted > self.tier { 600 } else { 2_500 };
                if now.saturating_duration_since(since) >= Duration::from_millis(hold) {
                    self.tier = wanted;
                    self.pending = None;
                }
            }
        }
        target.min(caps[self.tier])
    }
}

#[cfg(test)]
#[test]
fn capture_cadence_waits_for_sustained_changes_and_recovery_headroom() {
    let mut cadence = CaptureCadence::default();
    let start = Instant::now();
    let mut fps = |ms, bitrate| {
        cadence.fps(
            start + Duration::from_millis(ms),
            bitrate,
            60,
            FramePacing::Balanced,
        )
    };
    assert_eq!(fps(0, 2_500_000), 60);
    assert_eq!(fps(400, 4_000_000), 60);
    assert_eq!(fps(1_500, 4_000_000), 60);
    assert_eq!(fps(2_000, 2_500_000), 60);
    assert_eq!(fps(2_599, 2_500_000), 60);
    assert_eq!(fps(2_600, 2_500_000), 45);
    assert_eq!(fps(3_000, 3_100_000), 45);
    assert_eq!(fps(4_000, 4_000_000), 45);
    assert_eq!(fps(6_499, 4_000_000), 45);
    assert_eq!(fps(6_500, 4_000_000), 60);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CongestionState {
    Stable,
    RampingUp,
    MildCongestion,
    SevereCongestion,
    Cooldown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitrateDecision {
    pub target_bitrate_bps: u32,
    pub state: CongestionState,
    pub rtt_ms: f64,
    pub min_rtt_ms: f64,
    pub loss_pct: f64,
    pub reason: String,
    /// The controller's own verdict that this target is worth an encoder round
    /// trip. `RateLatch` narrows it further before the value reaches NVENC, so
    /// a caller that has a latch only needs this as a hint.
    pub encoder_reconfigure_recommended: bool,
}

/// Minimum spacing between two encoder rate changes.
///
/// `nvEncReconfigureEncoder` is accepted on any frame, but every call rebuilds
/// the rate-control state and re-seeds the VBV, so a controller that nudges the
/// target on each 100 ms tick keeps the encoder in a permanent transient. About
/// seven changes per second is still instantaneous to a viewer.
pub const RATE_LATCH_MIN_INTERVAL: Duration = Duration::from_millis(150);

/// A drop this large is urgent: the pacer has already reduced the wire rate, so
/// the encoder must follow at once or the queue in front of the network grows.
/// It bypasses the minimum interval.
pub const RATE_LATCH_URGENT_DROP_PERCENT: u32 = 10;

/// A smaller drop still waits for the interval, but is never ignored.
pub const RATE_LATCH_DROP_PERCENT: u32 = 2;

/// A rise is invisible to a viewer below this, so it is not worth a round trip
/// on its own. Recovery from a cut moves far more than this per tick.
pub const RATE_LATCH_RISE_PERCENT: u32 = 5;

/// A change still pending after this long is applied regardless of the rules
/// above, so a deferred rise can never be lost.
pub const RATE_LATCH_CATCH_UP: Duration = Duration::from_secs(1);

/// Decides when a congestion-control target is worth pushing into the encoder.
///
/// The asymmetry is deliberate: delaying a decrease grows the transport queue,
/// while delaying an increase costs nothing. Cut detection therefore stays
/// instantaneous and every other change is coalesced.
#[derive(Debug, Clone)]
pub struct RateLatch {
    applied_bps: u32,
    applied_at: Option<Instant>,
}

impl Default for RateLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLatch {
    pub fn new() -> Self {
        Self {
            applied_bps: 0,
            applied_at: None,
        }
    }

    pub fn applied_bps(&self) -> u32 {
        self.applied_bps
    }

    /// Records that the encoder accepted `bps`. Call only after the encoder
    /// confirms; a rejected change must be retried.
    pub fn note_applied(&mut self, bps: u32, now: Instant) {
        self.applied_bps = bps;
        self.applied_at = Some(now);
    }

    /// True when `target_bps` should be handed to the encoder now.
    pub fn should_apply(&self, target_bps: u32, recommended: bool, now: Instant) -> bool {
        if target_bps == 0 {
            return false;
        }
        let Some(applied_at) = self.applied_at else {
            return true;
        };
        if target_bps == self.applied_bps {
            return false;
        }
        let base = u64::from(self.applied_bps.max(1));
        let change_percent =
            ((u64::from(target_bps).abs_diff(u64::from(self.applied_bps)) * 100) / base) as u32;
        let dropping = target_bps < self.applied_bps;
        let waited = now.saturating_duration_since(applied_at);
        if dropping {
            if change_percent >= RATE_LATCH_URGENT_DROP_PERCENT {
                return true;
            }
            if change_percent >= RATE_LATCH_DROP_PERCENT {
                return waited >= RATE_LATCH_MIN_INTERVAL;
            }
        } else if recommended
            && change_percent >= RATE_LATCH_RISE_PERCENT
            && waited >= RATE_LATCH_MIN_INTERVAL
        {
            return true;
        }
        waited >= RATE_LATCH_CATCH_UP && change_percent >= 1
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PathTelemetrySample {
    pub rtt: Duration,
    pub min_rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub peer_frames_dropped: u64,
    pub video_queue_delay: Duration,
}

pub struct BitrateController {
    min_bps: u32,
    max_bps: u32,
    current_bps: u32,
    last_notified_bps: u32,
    baseline_rtt_ms: Option<f64>,
    delay_samples: u8,
    prev_sent_packets: u64,
    prev_lost_packets: u64,
    prev_congestion_events: u64,
    prev_peer_frames_dropped: u64,
    cooldown_until: Option<Instant>,
    last_update: Instant,
    /// Fast-attack / slow-decay loss estimate. A single lost packet in one
    /// 200 ms window must not move the media bitrate; a path that keeps losing
    /// packets must.
    loss_ewma: f64,
    loss_ewma_primed: bool,
    /// Consecutive feedback samples that asked for a mild backoff. One blip is
    /// ignored so quality does not sawtooth around isolated events.
    mild_samples: u8,
}

impl BitrateController {
    pub fn new(initial_bps: u32, max_bps: u32) -> Self {
        let clamped_max = max_bps.clamp(MIN_VIDEO_BITRATE_BPS, MAX_VIDEO_BITRATE_BPS);
        let clamped_init = initial_bps.clamp(MIN_VIDEO_BITRATE_BPS, clamped_max);
        let now = Instant::now();
        Self {
            min_bps: MIN_VIDEO_BITRATE_BPS,
            max_bps: clamped_max,
            current_bps: clamped_init,
            last_notified_bps: clamped_init,
            baseline_rtt_ms: None,
            delay_samples: 0,
            prev_sent_packets: 0,
            prev_lost_packets: 0,
            prev_congestion_events: 0,
            prev_peer_frames_dropped: 0,
            cooldown_until: None,
            last_update: now,
            loss_ewma: 0.0,
            loss_ewma_primed: false,
            mild_samples: 0,
        }
    }

    pub fn current_bitrate_bps(&self) -> u32 {
        self.current_bps
    }

    pub fn set_max_bitrate_bps(&mut self, max_bps: u32) {
        self.max_bps = max_bps.clamp(self.min_bps, MAX_VIDEO_BITRATE_BPS);
        self.current_bps = self.current_bps.min(self.max_bps);
    }

    pub fn update_at(&mut self, sample: PathTelemetrySample, now: Instant) -> BitrateDecision {
        let rtt_ms = sample.rtt.as_secs_f64() * 1000.0;
        let reported_min_ms = sample.min_rtt.as_secs_f64() * 1000.0;

        let baseline = match self.baseline_rtt_ms {
            Some(prev) => {
                let candidate = if reported_min_ms > 0.0 {
                    reported_min_ms.min(rtt_ms)
                } else {
                    rtt_ms
                };
                if candidate > 0.0 {
                    prev.min(candidate) * 0.98 + candidate * 0.02
                } else {
                    prev
                }
            }
            None if rtt_ms > 0.0 => {
                if reported_min_ms > 0.0 {
                    reported_min_ms
                } else {
                    rtt_ms
                }
            }
            None => 0.0,
        };
        if baseline > 0.0 {
            self.baseline_rtt_ms = Some(baseline);
        }

        let network_queue_ms = if baseline > 0.0 && rtt_ms > baseline {
            rtt_ms - baseline
        } else {
            0.0
        };
        let video_queue_ms = sample.video_queue_delay.as_secs_f64() * 1000.0;
        let queuing_delay_ms = network_queue_ms.max(video_queue_ms);
        let delay_budget_ms = 8.0 + (baseline * 0.35).min(12.0);
        if network_queue_ms > delay_budget_ms {
            self.delay_samples = self.delay_samples.saturating_add(1);
        } else {
            self.delay_samples = 0;
        }

        let delta_sent = sample.sent_packets.saturating_sub(self.prev_sent_packets);
        let delta_lost = sample.lost_packets.saturating_sub(self.prev_lost_packets);
        let delta_cong = sample
            .congestion_events
            .saturating_sub(self.prev_congestion_events);
        let delta_drops = sample
            .peer_frames_dropped
            .saturating_sub(self.prev_peer_frames_dropped);

        self.prev_sent_packets = sample.sent_packets;
        self.prev_lost_packets = sample.lost_packets;
        self.prev_congestion_events = sample.congestion_events;
        self.prev_peer_frames_dropped = sample.peer_frames_dropped;

        let loss_pct = if delta_sent >= 10 {
            (delta_lost as f64 / delta_sent as f64) * 100.0
        } else {
            0.0
        };

        let in_cooldown = self.cooldown_until.is_some_and(|until| now < until);
        let elapsed_secs = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64()
            .clamp(0.05, 2.0);
        self.last_update = now;

        // Fast attack, slow release: congestion is believed immediately, a
        // recovered path is only credited gradually.
        self.loss_ewma = if self.loss_ewma_primed {
            if loss_pct > self.loss_ewma {
                self.loss_ewma + (loss_pct - self.loss_ewma) * 0.5
            } else {
                self.loss_ewma + (loss_pct - self.loss_ewma) * 0.2
            }
        } else {
            self.loss_ewma_primed = true;
            loss_pct
        };

        let severe_signal = self.loss_ewma > 3.0
            || loss_pct > 8.0
            || queuing_delay_ms > 90.0
            || (self.delay_samples >= 2 && network_queue_ms > delay_budget_ms + 10.0)
            || delta_drops >= 3;
        // Mild signals use the instantaneous sample, not the smoothed one:
        // otherwise a single blip keeps asking for a cut for several windows
        // while the estimate decays, which is exactly the quality sawtooth.
        let mild_signal = loss_pct >= 2.0
            || self.delay_samples >= 3
            || video_queue_ms > 35.0
            // A burst of QUIC congestion events already cost the transport
            // throughput; only slow the media rate when loss confirms it.
            || (delta_cong > 0 && self.loss_ewma >= 1.0)
            || delta_drops >= 2;
        self.mild_samples = if mild_signal && !severe_signal {
            self.mild_samples.saturating_add(1)
        } else {
            0
        };

        let (next_bps, state, reason) = if severe_signal {
            let reduced = ((self.current_bps as f64) * 0.75) as u32;
            self.cooldown_until = Some(now + Duration::from_millis(600));
            (
                reduced,
                CongestionState::SevereCongestion,
                format!(
                    "severe backoff: loss={loss_pct:.2}%, qdelay={queuing_delay_ms:.1}ms, drops={delta_drops}"
                ),
            )
        } else if self.mild_samples >= 2 {
            // Two consecutive windows of real trouble: trim 5% and require two
            // more windows before trimming again, so quality settles instead
            // of oscillating between full and reduced bitrate.
            let reduced = ((self.current_bps as f64) * 0.95) as u32;
            self.mild_samples = 0;
            self.cooldown_until = Some(now + Duration::from_millis(400));
            (
                reduced,
                CongestionState::MildCongestion,
                format!(
                    "mild backoff: loss={loss_pct:.2}% (avg {:.2}%), qdelay={queuing_delay_ms:.1}ms, cong={delta_cong}",
                    self.loss_ewma
                ),
            )
        } else if in_cooldown {
            (
                self.current_bps,
                CongestionState::Cooldown,
                "holding bitrate during post-congestion drain".to_string(),
            )
        } else if loss_pct < 0.5
            && self.mild_samples == 0
            && network_queue_ms <= delay_budget_ms
            && video_queue_ms < 12.0
        {
            let ramp_per_sec = if self.current_bps < 5_000_000 {
                f64::from(self.current_bps) * 0.35
            } else if self.current_bps < 15_000_000 {
                6_000_000.0
            } else {
                4_500_000.0
            };
            let step = (ramp_per_sec * elapsed_secs) as u32;
            let increased = self.current_bps.saturating_add(step);
            let state = if self.current_bps >= self.max_bps {
                CongestionState::Stable
            } else {
                CongestionState::RampingUp
            };
            (
                increased,
                state,
                format!("clean path ramp: rtt={rtt_ms:.1}ms, loss={loss_pct:.2}%"),
            )
        } else {
            (
                self.current_bps,
                CongestionState::Stable,
                format!("steady state: rtt={rtt_ms:.1}ms, loss={loss_pct:.2}%"),
            )
        };

        self.current_bps = next_bps.clamp(self.min_bps, self.max_bps);

        let diff = self.current_bps.abs_diff(self.last_notified_bps);
        let threshold = (self.last_notified_bps / 20).max(100_000);
        let encoder_reconfigure_recommended = diff >= threshold
            || (self.current_bps == self.min_bps && self.last_notified_bps != self.min_bps)
            || (self.current_bps == self.max_bps && self.last_notified_bps != self.max_bps);

        if encoder_reconfigure_recommended {
            self.last_notified_bps = self.current_bps;
        }

        BitrateDecision {
            target_bitrate_bps: self.current_bps,
            state,
            rtt_ms,
            min_rtt_ms: baseline,
            loss_pct,
            reason,
            encoder_reconfigure_recommended,
        }
    }

    pub fn update(&mut self, sample: PathTelemetrySample) -> BitrateDecision {
        self.update_at(sample, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cellular_rtt_variation_allows_recovery_but_sustained_delay_backs_off() {
        let mut ctrl = BitrateController::new(1_000_000, 10_000_000);
        let mut now = Instant::now();
        let mut sample = |rtt| {
            now += Duration::from_millis(200);
            ctrl.update_at(
                PathTelemetrySample {
                    rtt: Duration::from_millis(rtt),
                    min_rtt: Duration::from_millis(18),
                    ..Default::default()
                },
                now,
            )
        };
        let mut recovered_bps = 0;
        for rtt in [27, 31, 28, 36, 29, 30, 28] {
            let decision = sample(rtt);
            assert!(decision.target_bitrate_bps >= recovered_bps);
            recovered_bps = decision.target_bitrate_bps;
        }
        assert!(recovered_bps > 1_300_000);
        assert_eq!(sample(48).target_bitrate_bps, recovered_bps);
        let congested = sample(48);
        assert_eq!(congested.state, CongestionState::SevereCongestion);
        assert!(congested.target_bitrate_bps < recovered_bps);
    }

    #[test]
    fn backs_off_on_loss_and_recovers_on_clean_path() {
        let mut ctrl = BitrateController::new(30_000_000, MAX_VIDEO_BITRATE_BPS);
        let t0 = Instant::now();

        let d0 = ctrl.update_at(
            PathTelemetrySample {
                rtt: Duration::from_millis(8),
                min_rtt: Duration::from_millis(8),
                sent_packets: 1000,
                lost_packets: 0,
                congestion_events: 0,
                peer_frames_dropped: 0,
                video_queue_delay: Duration::ZERO,
            },
            t0 + Duration::from_millis(200),
        );
        assert!(d0.target_bitrate_bps >= 30_000_000);

        // A single lossy window is held, not acted on: one burst is not a trend.
        let d1 = ctrl.update_at(
            PathTelemetrySample {
                rtt: Duration::from_millis(35),
                min_rtt: Duration::from_millis(8),
                sent_packets: 2000,
                lost_packets: 50,
                congestion_events: 2,
                peer_frames_dropped: 1,
                video_queue_delay: Duration::ZERO,
            },
            t0 + Duration::from_millis(400),
        );
        assert_ne!(d1.state, CongestionState::SevereCongestion);
        assert!(d1.target_bitrate_bps <= d0.target_bitrate_bps);
        assert!(d1.target_bitrate_bps >= MIN_VIDEO_BITRATE_BPS);

        // A second lossy window confirms it and drops the media rate.
        let d1b = ctrl.update_at(
            PathTelemetrySample {
                rtt: Duration::from_millis(35),
                min_rtt: Duration::from_millis(8),
                sent_packets: 3000,
                lost_packets: 100,
                congestion_events: 2,
                peer_frames_dropped: 1,
                video_queue_delay: Duration::ZERO,
            },
            t0 + Duration::from_millis(600),
        );
        assert!(d1b.target_bitrate_bps < d1.target_bitrate_bps);

        // A clean path climbs back.
        let mut last = d1b.target_bitrate_bps;
        let mut now = t0 + Duration::from_millis(600);
        for step in 1..=8u64 {
            now += Duration::from_millis(200);
            let d = ctrl.update_at(
                PathTelemetrySample {
                    rtt: Duration::from_millis(8),
                    min_rtt: Duration::from_millis(8),
                    sent_packets: 3000 + step * 1000,
                    lost_packets: 100,
                    congestion_events: 2,
                    peer_frames_dropped: 1,
                    video_queue_delay: Duration::ZERO,
                },
                now,
            );
            last = d.target_bitrate_bps;
            assert!(last <= MAX_VIDEO_BITRATE_BPS);
        }
        assert!(last > d1b.target_bitrate_bps);
    }

    #[test]
    fn a_single_bad_report_does_not_move_the_quality_ladder() {
        let mut ctrl = BitrateController::new(25_000_000, MAX_VIDEO_BITRATE_BPS);
        let t0 = Instant::now();
        let sample = |sent: u64, lost: u64, drops: u64| PathTelemetrySample {
            rtt: Duration::from_millis(9),
            min_rtt: Duration::from_millis(9),
            sent_packets: sent,
            lost_packets: lost,
            congestion_events: 0,
            peer_frames_dropped: drops,
            video_queue_delay: Duration::ZERO,
        };
        // One window with 1.5% loss and one dropped frame: hold, do not cut.
        let first = ctrl.update_at(sample(1000, 15, 1), t0 + Duration::from_millis(200));
        assert_eq!(first.target_bitrate_bps, 25_000_000);
        assert_ne!(first.state, CongestionState::MildCongestion);
        // The next windows are clean, so quality keeps climbing instead of
        // sawtoothing down because of the earlier blip.
        let second = ctrl.update_at(sample(2000, 15, 1), t0 + Duration::from_millis(400));
        assert!(second.target_bitrate_bps >= 25_000_000);
        let third = ctrl.update_at(sample(3000, 15, 1), t0 + Duration::from_millis(600));
        assert!(third.target_bitrate_bps >= second.target_bitrate_bps);
    }

    #[test]
    fn sustained_loss_still_backs_off() {
        let mut ctrl = BitrateController::new(25_000_000, MAX_VIDEO_BITRATE_BPS);
        let mut now = Instant::now();
        let mut bps = 25_000_000;
        for i in 1..=6u64 {
            now += Duration::from_millis(200);
            let d = ctrl.update_at(
                PathTelemetrySample {
                    rtt: Duration::from_millis(9),
                    min_rtt: Duration::from_millis(9),
                    sent_packets: i * 1000,
                    lost_packets: i * 20,
                    congestion_events: 0,
                    peer_frames_dropped: 0,
                    video_queue_delay: Duration::ZERO,
                },
                now,
            );
            bps = d.target_bitrate_bps;
            assert!(bps >= MIN_VIDEO_BITRATE_BPS);
        }
        assert!(bps < 25_000_000);
    }

    #[test]
    fn backs_off_below_5mbps_and_respects_1mbps_floor() {
        let mut ctrl = BitrateController::new(6_000_000, MAX_VIDEO_BITRATE_BPS);
        let mut t = Instant::now();
        for i in 1..=10 {
            t += Duration::from_millis(200);
            let d = ctrl.update_at(
                PathTelemetrySample {
                    rtt: Duration::from_millis(60),
                    min_rtt: Duration::from_millis(10),
                    sent_packets: i * 500,
                    lost_packets: i * 50,
                    congestion_events: i,
                    peer_frames_dropped: i * 3,
                    video_queue_delay: Duration::ZERO,
                },
                t,
            );
            assert!(d.target_bitrate_bps >= MIN_VIDEO_BITRATE_BPS);
        }
        assert_eq!(ctrl.current_bitrate_bps(), MIN_VIDEO_BITRATE_BPS);
    }

    /// 20 Mbps applied, then `target` evaluated `after_ms` later.
    fn latch_case(target: u32, recommended: bool, after_ms: u64) -> bool {
        let origin = Instant::now();
        let mut latch = RateLatch::new();
        latch.note_applied(20_000_000, origin);
        latch.should_apply(
            target,
            recommended,
            origin + Duration::from_millis(after_ms),
        )
    }

    #[test]
    fn the_first_target_is_always_applied() {
        let latch = RateLatch::new();
        assert!(latch.should_apply(20_000_000, false, Instant::now()));
        assert!(!latch.should_apply(0, true, Instant::now()));
    }

    #[test]
    fn an_unchanged_target_is_never_resent() {
        assert!(!latch_case(20_000_000, true, 10_000));
    }

    #[test]
    fn a_cut_is_applied_without_waiting_for_the_interval() {
        // 15 % down, one millisecond after the last change: the wire rate has
        // already dropped, so waiting would only build a pacer queue.
        assert!(latch_case(17_000_000, true, 1));
        // A 10 % cut is the urgent boundary.
        assert!(latch_case(18_000_000, false, 0));
    }

    #[test]
    fn a_small_trim_still_waits_for_the_interval() {
        assert!(!latch_case(19_000_000, true, 50));
        assert!(latch_case(19_000_000, true, 200));
    }

    #[test]
    fn rises_are_coalesced_to_one_per_interval() {
        // 10 % up and endorsed, but inside the minimum interval.
        assert!(!latch_case(22_000_000, true, 100));
        assert!(latch_case(22_000_000, true, 200));
    }

    #[test]
    fn a_rise_the_controller_did_not_endorse_waits_for_the_catch_up() {
        // The controller applied its own 2.5 % deadband and said this move is
        // not worth a round trip; the latch defers it rather than drops it.
        assert!(!latch_case(22_000_000, false, 500));
        assert!(latch_case(22_000_000, false, 1_100));
    }

    #[test]
    fn a_tiny_rise_is_not_worth_a_reconfigure() {
        // 4.5 % up: invisible to a viewer, so it waits for the catch-up window
        // instead of costing an encoder round trip on its own.
        assert!(!latch_case(20_900_000, true, 500));
        assert!(latch_case(20_900_000, true, 1_100));
    }

    #[test]
    fn a_deferred_change_cannot_be_lost() {
        // 3 % pending for a second: applied even with no endorsement.
        assert!(!latch_case(20_600_000, false, 900));
        assert!(latch_case(20_600_000, false, 1_050));
    }

    #[test]
    fn a_rejected_change_is_retried() {
        let origin = Instant::now();
        let mut latch = RateLatch::new();
        latch.note_applied(20_000_000, origin);
        // Nothing was noted after the refusal, so the same target is offered
        // again on the next tick instead of being silently forgotten.
        assert!(latch.should_apply(24_000_000, true, origin + Duration::from_millis(200)));
        assert!(latch.should_apply(24_000_000, true, origin + Duration::from_millis(400)));
        latch.note_applied(24_000_000, origin + Duration::from_millis(400));
        assert!(!latch.should_apply(24_000_000, true, origin + Duration::from_millis(500)));
    }
}
