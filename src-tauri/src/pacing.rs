//! Deterministic host frame cadence.
//!
//! A stream looks "native" when the interval between presented pictures is the
//! display's refresh interval, every time, with no long frames and no bursts.
//! Reporters call that frame pacing; players feel it as smoothness.
//!
//! The old capture loop polled DXGI with a zero timeout and then slept for the
//! remainder of the frame. Two things went wrong with that:
//!
//! * `AcquireNextFrame(0)` returns `WAIT_TIMEOUT` whenever the desktop has not
//!   presented since the last poll, so a 60 Hz desktop polled at 60 Hz produced
//!   an occasional duplicate and an occasional missed composite. Duplicate and
//!   missed frames are what judder looks like.
//! * The sleep happened *after* capture and encode, so the loop period was
//!   `work + wait`. When encoding a keyframe or scaling a 4K surface took
//!   longer than usual the cadence absorbed the overrun one frame at a time
//!   (drift), and when it recovered the loop fired a burst of catch-up frames.
//!
//! This module replaces both with a deadline grid and an encode-aware capture
//! budget:
//!
//! * capture blocks (event-driven) only for the time left before the next
//!   deadline minus the measured encode cost, so a frame is started precisely
//!   when the desktop is about to present;
//! * the grid never accumulates drift, and a slot that is missed is *skipped*,
//!   not repaid as a burst;
//! * the encode cost is an EMA, so the budget adapts to the host hardware
//!   instead of assuming a fast GPU.

use std::time::{Duration, Instant};

/// How much of the frame budget to reserve for the work that follows capture
/// (colour conversion, encode, hand-off). Never let the capture wait consume
/// the whole slot.
const ENCODE_GUARD: Duration = Duration::from_micros(750);
/// Seed for the encode-cost EMA before the first frame has been measured.
const COLD_ENCODE_ESTIMATE: Duration = Duration::from_millis(3);
/// Sleep coarsely and spin only this far to hit the deadline precisely.
const SPIN_BUDGET: Duration = Duration::from_micros(800);
/// Longest the capture call may block. Idle desktop modes drop to a few frames
/// per second, and a control message (keyframe request, bitrate change) must
/// still be serviced promptly.
const MAX_CAPTURE_WAIT: Duration = Duration::from_millis(32);

/// Drift-free cadence for the host capture/encode loop.
pub struct FrameCadence {
    interval: Duration,
    next_deadline: Instant,
    encode_ema: Duration,
    late_slots: u64,
    skipped_slots: u64,
}

impl FrameCadence {
    pub fn new(now: Instant, fps: u16) -> Self {
        let interval = interval_for(fps);
        Self {
            interval,
            next_deadline: now + interval,
            encode_ema: COLD_ENCODE_ESTIMATE,
            late_slots: 0,
            skipped_slots: 0,
        }
    }

    /// Re-anchor the grid after a pipeline restart (encoder recreate, resume
    /// from a paused session). Drift accumulated while work was not streaming
    /// must not be replayed as a burst.
    pub fn resync(&mut self, now: Instant) {
        self.next_deadline = now + self.interval;
    }

    pub fn set_fps(&mut self, fps: u16) {
        let interval = interval_for(fps);
        if interval == self.interval {
            return;
        }
        self.interval = interval;
        let now = Instant::now();
        // A rate change must not leave a slot longer than the new interval.
        if self.next_deadline > now + interval {
            self.next_deadline = now + interval;
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Records how long the last capture-to-bitstream step took. The EMA is
    /// clamped to one frame: an over-budget encoder must not push the capture
    /// wait negative for a whole second, it just loses its head start.
    pub fn note_encode(&mut self, took: Duration) {
        let prev = self.encode_ema.as_secs_f64();
        let sample = took.as_secs_f64().min(self.interval.as_secs_f64());
        let next = prev + (sample - prev) * 0.2;
        self.encode_ema = Duration::from_secs_f64(next.max(0.0));
    }

    pub fn encode_estimate(&self) -> Duration {
        self.encode_ema
    }

    /// Milliseconds `AcquireNextFrame` may block.
    ///
    /// This is the remaining budget minus the measured encode cost: the
    /// capture wakes as soon as the desktop presents, and if it does not
    /// present in time we stop waiting early enough that the duplicate frame
    /// still leaves on the grid.
    pub fn capture_wait_ms(&self, now: Instant) -> u32 {
        let remaining = self.next_deadline.saturating_duration_since(now);
        let reserve = self.encode_ema.saturating_add(ENCODE_GUARD);
        let budget = remaining
            .saturating_sub(reserve)
            .min(self.interval)
            .min(MAX_CAPTURE_WAIT);
        u32::try_from(budget.as_millis()).unwrap_or(u32::MAX)
    }

    /// Advances the grid after a frame attempt.
    ///
    /// `next_deadline` is the end of the slot that just ran. When the slot is
    /// still open, the returned instant is when the loop should start the next
    /// one. When the slot has already been missed the grid re-anchors to
    /// `now + interval` and returns `None`: one skipped slot, never a burst.
    pub fn advance(&mut self, now: Instant) -> Option<Instant> {
        if now < self.next_deadline {
            let deadline = self.next_deadline;
            self.next_deadline += self.interval;
            return Some(deadline);
        }
        self.late_slots = self.late_slots.saturating_add(1);
        let lateness = now.saturating_duration_since(self.next_deadline);
        if lateness >= self.interval {
            self.skipped_slots = self
                .skipped_slots
                .saturating_add((lateness.as_nanos() / self.interval.as_nanos().max(1)) as u64);
        }
        self.next_deadline = now + self.interval;
        None
    }

    /// Blocks until the next slot. Used by the host loop after a frame attempt.
    pub fn wait_next_frame(&mut self) {
        let now = Instant::now();
        if let Some(deadline) = self.advance(now) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining > SPIN_BUDGET {
                std::thread::sleep(remaining - SPIN_BUDGET);
            }
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
        }
    }

    pub fn late_slots(&self) -> u64 {
        self.late_slots
    }

    pub fn skipped_slots(&self) -> u64 {
        self.skipped_slots
    }
}

fn interval_for(fps: u16) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 360)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn capture_wait_reserves_measured_encode_time() {
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        for _ in 0..12 {
            cadence.note_encode(Duration::from_millis(6));
        }
        // 16.67 ms slot, ~6 ms encode plus the guard: the acquire may block
        // for roughly 9-10 ms and still leave the frame on the grid.
        let wait = cadence.capture_wait_ms(start);
        assert!((9..=10).contains(&wait), "wait was {wait} ms");
    }

    #[test]
    fn capture_wait_never_goes_negative() {
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        for _ in 0..30 {
            cadence.note_encode(Duration::from_millis(40));
        }
        assert_eq!(cadence.capture_wait_ms(start), 0);
        // Even a late slot must not block the capture thread.
        assert_eq!(
            cadence.capture_wait_ms(start + Duration::from_millis(100)),
            0
        );
    }

    #[test]
    fn the_grid_does_not_drift_over_a_second() {
        // Simulate 60 slots without sleeping: the deadline grid must land on
        // the exact 60 Hz boundary, not 60 x (work + sleep).
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        let mut now = start;
        for _ in 0..60 {
            if let Some(deadline) = cadence.advance(now) {
                now = deadline;
            }
        }
        let expected = cadence.interval() * 60;
        let actual = now.saturating_duration_since(start);
        let error = if actual > expected {
            actual - expected
        } else {
            expected - actual
        };
        assert!(error <= Duration::from_millis(1), "drifted by {error:?}");
        assert_eq!(cadence.late_slots(), 0);
    }

    #[test]
    fn a_late_slot_is_skipped_not_repaid() {
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        // A 40 ms encode overran two slots.
        let late = start + Duration::from_millis(40);
        assert!(cadence.advance(late).is_none());
        assert!(cadence.late_slots() >= 1);
        // The next slot starts a full interval after the overrun, so the loop
        // cannot fire a catch-up burst.
        assert!(cadence.capture_wait_ms(late) <= 16);
    }

    #[test]
    fn fps_change_reanchors_the_grid() {
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        cadence.set_fps(120);
        assert_eq!(cadence.interval(), Duration::from_secs_f64(1.0 / 120.0));
        assert!(cadence.capture_wait_ms(start) <= 8);
    }

    #[test]
    fn resync_clears_stale_deadlines() {
        let start = t0();
        let mut cadence = FrameCadence::new(start, 60);
        let later = start + Duration::from_secs(5);
        cadence.resync(later);
        assert!(cadence.capture_wait_ms(later) > 0);
        assert!(cadence.advance(later).is_some());
    }
}
