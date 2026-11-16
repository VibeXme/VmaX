//! Reference-frame invalidation (RFI), intra-refresh waves and the NVENC
//! recovery knobs that replace periodic IDR frames.
//!
//! Normal operation uses an infinite GOP plus a rolling intra-refresh band
//! (`intraRefreshPeriod = 45`, `intraRefreshCnt = 5`).  A lost frame is
//! repaired by asking NVENC to start an intra-refresh wave on the next frame,
//! not by emitting a multi-megabyte IDR.  An IDR remains the last-resort path
//! for a brand-new decoder or a sustained loss episode.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// NVENC intra-refresh settings.  These map directly to
/// `NV_ENC_CONFIG_H264::enableIntraRefresh`, `intraRefreshPeriod` and
/// `intraRefreshCnt` (and the HEVC equivalents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntraRefreshConfig {
    pub enabled: bool,
    /// Start a new refresh band every N encoded frames.
    pub period_frames: u32,
    /// Refresh this many frames per band.  Must be < `period_frames`.
    pub refresh_frames: u32,
}

impl Default for IntraRefreshConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            period_frames: 60,
            refresh_frames: 5,
        }
    }
}

impl IntraRefreshConfig {
    pub fn sanitized(self) -> Self {
        let period = self.period_frames.max(2);
        let refresh = self.refresh_frames.clamp(1, period - 1);
        Self {
            enabled: self.enabled,
            period_frames: period,
            refresh_frames: refresh,
        }
    }
}

/// The exact NVENC rate-control fields required by a delay-gradient decision.
/// `vbv_buffer_size` is one frame of bits, so NVENC cannot build a burst that
/// the pacer then has to absorb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencRateParams {
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    pub enable_intra_refresh: bool,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
}

impl NvencRateParams {
    pub fn new(bitrate_bps: u32, fps: u32, ir: IntraRefreshConfig) -> Self {
        let ir = ir.sanitized();
        let frame_bits = bitrate_bps / fps.max(1);
        Self {
            average_bit_rate: bitrate_bps,
            max_bit_rate: bitrate_bps,
            vbv_buffer_size: frame_bits.max(1),
            vbv_initial_delay: frame_bits.max(1),
            enable_intra_refresh: ir.enabled,
            intra_refresh_period: ir.period_frames,
            intra_refresh_cnt: ir.refresh_frames,
        }
    }
}

pub mod feedback_flags {
    pub const NACK: u8 = 1 << 0;
    pub const RFI: u8 = 1 << 1;
    pub const FRAME_DAMAGED: u8 = 1 << 2;
}

/// Normalised view of a `congestion::FeedbackPacket`.  Keeping this separate
/// means the recovery state machine is unit-testable without a network stack.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameFeedback {
    pub highest_frame_id: u32,
    pub last_intact_frame_id: u32,
    pub nack_bitmap: u64,
    pub flags: u8,
    pub loss_permille: u16,
}

impl FrameFeedback {
    pub fn missing_in_window(&self) -> u32 {
        self.nack_bitmap.count_ones()
    }

    pub fn gap(&self) -> u32 {
        self.highest_frame_id
            .saturating_sub(self.last_intact_frame_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    None,
    /// Ask NVENC to start an intra-refresh wave on the next picture.
    IntraRefresh {
        frames: u32,
    },
    /// Ask NVENC to encode against a long-term reference instead of the most
    /// recent frame.  `slot` is the NVENC LTR index (0..=3), not a frame id.
    UseLongTermReference {
        slot: u8,
    },
    /// Last resort: a real IDR.  This costs a latency spike and should only
    /// happen for a new decoder or a sustained unrecoverable loss episode.
    IdrFallback,
}

impl RecoveryAction {
    pub fn apply<E: EncoderRecoverySink>(self, encoder: &mut E) -> Result<(), String> {
        match self {
            Self::None => Ok(()),
            Self::IntraRefresh { frames } => encoder.force_intra_refresh(frames),
            Self::UseLongTermReference { slot } => encoder.use_long_term_reference(slot),
            Self::IdrFallback => encoder.force_idr(),
        }
    }
}

/// Host-side encoder operations.  Your NVENC implementation can implement
/// this directly; the MFT fallback simply maps intra refresh to a keyframe
/// because Media Foundation has no equivalent rolling-refresh control.
pub trait EncoderRecoverySink {
    fn apply_rate(&mut self, params: NvencRateParams) -> Result<(), String>;
    fn force_intra_refresh(&mut self, frames: u32) -> Result<(), String>;
    fn force_idr(&mut self) -> Result<(), String>;

    fn use_long_term_reference(&mut self, slot: u8) -> Result<(), String> {
        let _ = slot;
        Err("long-term reference selection is not supported by this encoder".to_string())
    }
}

/// Four-slot NVENC long-term-reference ring.  Mark a frame after encoding it;
/// when RFI reports a stale decoder, select the newest marked frame at or
/// before `last_intact_frame_id`.
#[derive(Debug, Clone, Default)]
pub struct LtrTracker {
    slots: [Option<u32>; 4],
    next: usize,
}

impl LtrTracker {
    pub fn mark(&mut self, frame_id: u32) -> u8 {
        let slot = self.next % self.slots.len();
        self.slots[slot] = Some(frame_id);
        self.next = self.next.wrapping_add(1);
        slot as u8
    }

    pub fn select(&self, last_intact_frame_id: u32) -> Option<u8> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| match *frame {
                Some(id) if id <= last_intact_frame_id => Some((id, index as u8)),
                _ => None,
            })
            .max_by_key(|&(id, _)| id)
            .map(|(_, slot)| slot)
    }

    pub fn clear(&mut self) {
        self.slots = [None; 4];
        self.next = 0;
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    pub intra_refresh: IntraRefreshConfig,
    pub enable_ltr: bool,
    /// Minimum spacing between two RFI waves.
    pub min_ir_interval: Duration,
    /// More than this many waves in one second escalates to an IDR.
    pub max_ir_per_second: u32,
    /// A gap this large is not worth trying to refresh gradually.
    pub max_recoverable_gap: u32,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            intra_refresh: IntraRefreshConfig::default(),
            enable_ltr: true,
            min_ir_interval: Duration::from_millis(120),
            max_ir_per_second: 4,
            max_recoverable_gap: 12,
        }
    }
}

/// State machine that converts client NACK / LAST_INTACT_FRAME_ID reports into
/// encoder actions.  It is deliberately conservative: one missing frame is
/// treated as reordering/FEC territory, a small gap gets an intra-refresh
/// wave, and only a sustained gap escalates to an IDR.
pub struct RecoveryController {
    config: RecoveryConfig,
    ltr: LtrTracker,
    last_intact: u32,
    highest: u32,
    last_ir: Option<Instant>,
    ir_history: VecDeque<Instant>,
    escalated: u32,
}

impl RecoveryController {
    pub fn new(config: RecoveryConfig) -> Self {
        Self {
            config,
            ltr: LtrTracker::default(),
            last_intact: 0,
            highest: 0,
            last_ir: None,
            ir_history: VecDeque::with_capacity(8),
            escalated: 0,
        }
    }

    pub fn config(&self) -> &RecoveryConfig {
        &self.config
    }

    pub fn note_encoded_frame(&mut self, frame_id: u32) -> Option<u8> {
        self.config.enable_ltr.then(|| self.ltr.mark(frame_id))
    }

    pub fn on_frame_completed(&mut self, frame_id: u32) {
        self.last_intact = self.last_intact.max(frame_id);
        if self.last_intact >= self.highest {
            self.escalated = 0;
        }
    }

    pub fn on_feedback(&mut self, feedback: FrameFeedback, now: Instant) -> RecoveryAction {
        self.highest = self.highest.max(feedback.highest_frame_id);
        self.last_intact = self.last_intact.max(feedback.last_intact_frame_id);

        if self.last_intact >= self.highest && feedback.nack_bitmap == 0 {
            self.escalated = 0;
            return RecoveryAction::None;
        }

        let gap = feedback.gap();
        let missing = feedback.missing_in_window();
        let hard_rfi = feedback.flags & (feedback_flags::RFI | feedback_flags::FRAME_DAMAGED) != 0;
        let looks_unrecoverable = gap > self.config.max_recoverable_gap || missing > 20;

        if looks_unrecoverable {
            if self.last_ir.is_some_and(|last| {
                now.saturating_duration_since(last) < self.config.min_ir_interval
            }) {
                return RecoveryAction::None;
            }
            self.escalated = self.escalated.saturating_add(1);
            if self.escalated >= 2 {
                self.ltr.clear();
                self.last_ir = Some(now);
                return RecoveryAction::IdrFallback;
            }
            return self.issue_intra_refresh(now);
        }

        // A one-frame gap is normal reordering or a single FEC block that is
        // still in flight.  Do not react until the client explicitly marks it
        // damaged or it persists for a second report.
        if gap <= 1 && !hard_rfi {
            return RecoveryAction::None;
        }

        if hard_rfi
            && self.config.enable_ltr
            && let Some(slot) = self.ltr.select(feedback.last_intact_frame_id)
        {
            return RecoveryAction::UseLongTermReference { slot };
        }

        self.issue_intra_refresh(now)
    }

    fn issue_intra_refresh(&mut self, now: Instant) -> RecoveryAction {
        if self
            .last_ir
            .is_some_and(|last| now.saturating_duration_since(last) < self.config.min_ir_interval)
        {
            return RecoveryAction::None;
        }
        self.ir_history.push_back(now);
        while self
            .ir_history
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) > Duration::from_secs(1))
        {
            self.ir_history.pop_front();
        }
        if self.ir_history.len() as u32 > self.config.max_ir_per_second {
            self.escalated = self.escalated.saturating_add(1);
            self.last_ir = Some(now);
            return RecoveryAction::IdrFallback;
        }
        self.last_ir = Some(now);
        RecoveryAction::IntraRefresh {
            frames: self.config.intra_refresh.refresh_frames,
        }
    }

    /// Client joined, codec changed, or capture restarted: no reference exists
    /// yet, so this is the one case where an IDR is the correct answer.
    pub fn request_new_decoder_refresh(&mut self) -> RecoveryAction {
        self.ltr.clear();
        self.escalated = 0;
        RecoveryAction::IdrFallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(highest: u32, last_intact: u32, nack: u64) -> FrameFeedback {
        FrameFeedback {
            highest_frame_id: highest,
            last_intact_frame_id: last_intact,
            nack_bitmap: nack,
            flags: if nack != 0 { feedback_flags::NACK } else { 0 },
            loss_permille: 0,
        }
    }

    #[test]
    fn clean_feedback_needs_no_recovery() {
        let mut ctrl = RecoveryController::new(RecoveryConfig::default());
        assert_eq!(
            ctrl.on_feedback(feedback(100, 100, 0), Instant::now()),
            RecoveryAction::None
        );
    }

    #[test]
    fn single_frame_gap_waits_for_fec_or_reordering() {
        let mut ctrl = RecoveryController::new(RecoveryConfig::default());
        assert_eq!(
            ctrl.on_feedback(feedback(100, 99, 0), Instant::now()),
            RecoveryAction::None
        );
    }

    #[test]
    fn small_gap_triggers_an_intra_refresh_wave() {
        let mut ctrl = RecoveryController::new(RecoveryConfig::default());
        let action = ctrl.on_feedback(feedback(100, 96, 0b111), Instant::now());
        assert!(matches!(action, RecoveryAction::IntraRefresh { frames: 5 }));
    }

    #[test]
    fn repeated_small_gaps_escalate_to_idr() {
        let mut ctrl = RecoveryController::new(RecoveryConfig {
            max_ir_per_second: 1,
            min_ir_interval: Duration::ZERO,
            ..RecoveryConfig::default()
        });
        let now = Instant::now();
        let first = ctrl.on_feedback(feedback(100, 96, 0b111), now);
        assert!(matches!(first, RecoveryAction::IntraRefresh { .. }));
        let second = ctrl.on_feedback(feedback(108, 104, 0b111), now + Duration::from_millis(10));
        assert_eq!(second, RecoveryAction::IdrFallback);
    }

    #[test]
    fn huge_gap_escalates_quickly() {
        let mut ctrl = RecoveryController::new(RecoveryConfig::default());
        let now = Instant::now();
        let _ = ctrl.on_feedback(feedback(100, 50, u64::MAX), now);
        let action = ctrl.on_feedback(
            feedback(110, 50, u64::MAX),
            now + Duration::from_millis(200),
        );
        assert_eq!(action, RecoveryAction::IdrFallback);
    }

    #[test]
    fn ltr_selects_the_newest_safe_reference() {
        let mut ltr = LtrTracker::default();
        ltr.mark(10);
        ltr.mark(20);
        ltr.mark(30);
        assert_eq!(ltr.select(25), Some(1));
        assert_eq!(ltr.select(5), None);
    }

    #[test]
    fn nvenc_vbv_is_one_frame() {
        let params = NvencRateParams::new(12_000_000, 120, IntraRefreshConfig::default());
        assert_eq!(params.vbv_buffer_size, 100_000);
        assert_eq!(params.intra_refresh_period, 60);
        assert_eq!(params.intra_refresh_cnt, 5);
    }
}
