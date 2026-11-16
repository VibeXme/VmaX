//! NVENC intra-refresh (no periodic IDR) and the client micro-jitter buffer.
//!
//! ## Host: never emit a periodic I-frame
//!
//! Every periodic IDR is a burst that a mobile uplink cannot absorb: it costs
//! one or two frame intervals of latency and, on a congested path, triggers the
//! next delay spike.  With intra refresh the encoder codes a moving vertical
//! band of macroblocks as intra, so the picture is fully refreshed every
//! `intra_refresh_period` frames while every frame keeps the same size.
//!
//! NVENC fields (H.264 and HEVC both):
//!
//! ```text
//! NV_ENC_CONFIG_H264::enableIntraRefresh   = 1
//! NV_ENC_CONFIG_H264::intraRefreshPeriod   = 45
//! NV_ENC_CONFIG_H264::intraRefreshCnt      = 5
//! NV_ENC_CONFIG_H264::idrPeriod            = NVENC_INFINITE_GOPLENGTH
//! NV_ENC_RC_PARAMS::enableIntraRefresh     = 1   (rcParams, required)
//! NV_ENC_RC_PARAMS::intraRefreshPeriod     = 45
//! NV_ENC_RC_PARAMS::intraRefreshCnt        = 5
//! ```
//!
//! ## Client: micro-jitter buffer
//!
//! [`MicroJitterBuffer`] sizes a 2 ms (LAN) .. 14 ms (4G/5G) hold from the 95th
//! percentile of the last 100 frame arrival deltas, and drops any frame older
//! than the newest complete one.  `MediaCodec` keys for instant decode on a
//! 120 Hz phone:
//!
//! ```kotlin
//! MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_HEVC, w, h).apply {
//!     setInteger(MediaFormat.KEY_LOW_LATENCY, 1)            // API 30+
//!     setInteger(MediaFormat.KEY_OPERATING_RATE, 120)       // match display Hz
//!     setInteger(MediaFormat.KEY_PRIORITY, 0)               // 0 = realtime
//!     setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 0)         // 0 = no copy buffer cap
//!     setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
//! }
//! ```
//!
//! Integration: `android_jni.rs` already owns a [`crate::jitter_buffer`]
//! producer/consumer pair; [`MicroJitterBuffer`] is the same contract with the
//! tighter 2..14 ms window and can be substituted one-for-one.

use crate::jitter_buffer::{BufferedFrame, SpscRing};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `NVENC_INFINITE_GOPLENGTH`.  Any periodic IDR is a latency spike we refuse
/// to pay for.
pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xFFFF_FFFF;

/// A full picture refresh every 45 frames (~0.75 s at 60 FPS, ~0.4 s at 120).
pub const INTRA_REFRESH_PERIOD: u32 = 45;
/// Five macroblock rows per frame: the band moves 9 times faster than the
/// period, so a single lost frame is healed by the next band.
pub const INTRA_REFRESH_CNT: u32 = 5;

/// LAN: one display frame is never buffered.
pub const MIN_HOLD: Duration = Duration::from_millis(2);
/// Cellular: enough to ride out one reordering/recovery interval.
pub const MAX_HOLD: Duration = Duration::from_millis(14);
/// Sliding window for the 95th percentile.
pub const JITTER_WINDOW: usize = 100;

/// Exact NVENC intra-refresh configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencIntraRefresh {
    pub enable_intra_refresh: bool,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub idr_period: u32,
}

impl Default for NvencIntraRefresh {
    fn default() -> Self {
        Self {
            enable_intra_refresh: true,
            intra_refresh_period: INTRA_REFRESH_PERIOD,
            intra_refresh_cnt: INTRA_REFRESH_CNT,
            idr_period: NVENC_INFINITE_GOPLENGTH,
        }
    }
}

impl NvencIntraRefresh {
    /// Clamp to values NVENC accepts: `intraRefreshCnt` must be smaller than
    /// the period, and both must be non-zero.
    pub fn sanitized(self) -> Self {
        let period = self.intra_refresh_period.max(2);
        Self {
            enable_intra_refresh: self.enable_intra_refresh,
            intra_refresh_period: period,
            intra_refresh_cnt: self.intra_refresh_cnt.clamp(1, period - 1),
            // Force the infinite GOP whenever intra refresh is on; a finite
            // idrPeriod would reintroduce the spike we are removing.
            idr_period: if self.enable_intra_refresh {
                NVENC_INFINITE_GOPLENGTH
            } else {
                self.idr_period.max(1)
            },
        }
    }

    /// On-demand wave after an unrecoverable frame (`LAST_INTACT_FRAME_ID`).
    /// This is the RFI path: refresh only the band, never a full IDR.
    pub fn on_demand_wave(&self) -> Self {
        Self {
            enable_intra_refresh: true,
            intra_refresh_period: self.intra_refresh_period.max(2),
            intra_refresh_cnt: self.intra_refresh_cnt.max(1),
            idr_period: NVENC_INFINITE_GOPLENGTH,
        }
    }

    pub fn to_recovery_config(self) -> crate::encoder_recovery::IntraRefreshConfig {
        let clean = self.sanitized();
        crate::encoder_recovery::IntraRefreshConfig {
            enabled: clean.enable_intra_refresh,
            period_frames: clean.intra_refresh_period,
            refresh_frames: clean.intra_refresh_cnt,
        }
    }
}

#[derive(Debug, Default)]
struct MicroStats {
    pushed: AtomicU64,
    emitted: AtomicU64,
    dropped_stale: AtomicU64,
    dropped_full: AtomicU64,
}

/// Sliding 100-sample 95th-percentile estimator of `recv_delta - send_delta`.
struct P95Jitter {
    samples: [u32; JITTER_WINDOW],
    index: usize,
    filled: usize,
    last_arrival_us: Option<u64>,
    last_capture_us: Option<u64>,
}

impl P95Jitter {
    fn new() -> Self {
        Self {
            samples: [0; JITTER_WINDOW],
            index: 0,
            filled: 0,
            last_arrival_us: None,
            last_capture_us: None,
        }
    }

    fn observe(&mut self, capture_us: u64, arrival_us: u64) {
        if let (Some(prev_arrival), Some(prev_capture)) =
            (self.last_arrival_us, self.last_capture_us)
        {
            let recv_delta = arrival_us.saturating_sub(prev_arrival) as i64;
            let send_delta = capture_us.saturating_sub(prev_capture) as i64;
            let sample = (recv_delta - send_delta)
                .unsigned_abs()
                .min(u64::from(u32::MAX)) as u32;
            self.samples[self.index] = sample;
            self.index = (self.index + 1) % JITTER_WINDOW;
            self.filled = (self.filled + 1).min(JITTER_WINDOW);
        }
        self.last_arrival_us = Some(arrival_us);
        self.last_capture_us = Some(capture_us);
    }

    fn p95_us(&self) -> u32 {
        if self.filled == 0 {
            return 0;
        }
        let mut copy = self.samples;
        let window = &mut copy[..self.filled];
        window.sort_unstable();
        let index = ((self.filled * 95).div_ceil(100)).saturating_sub(1);
        window[index.min(self.filled - 1)]
    }

    /// 2 ms base + 1.2x p95, clamped to 2..14 ms.  The base covers the display
    /// pipeline; the multiplier covers one reordering event.
    fn target_hold_us(&self) -> u32 {
        let p95 = self.p95_us() as f32;
        ((MIN_HOLD.as_micros() as f32 + p95 * 1.2) as u32).clamp(
            MIN_HOLD.as_micros() as u32,
            MAX_HOLD.as_micros() as u32,
        )
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MicroJitterSnapshot {
    pub pushed: u64,
    pub emitted: u64,
    pub dropped_stale: u64,
    pub dropped_full: u64,
    pub p95_jitter_us: u32,
    pub hold_us: u32,
    pub queue_depth: u32,
}

/// Lock-free client buffer: one network-side producer, one decoder consumer.
pub struct MicroJitterBuffer {
    ring: Arc<SpscRing<BufferedFrame>>,
    estimator: P95Jitter,
    hold_us: Arc<AtomicU32>,
    stats: Arc<MicroStats>,
    origin: Instant,
    current_hold_us: u32,
    last_pushed: Option<u32>,
    last_emitted: Option<u32>,
}

impl MicroJitterBuffer {
    pub fn new(origin: Instant, capacity: usize) -> Self {
        Self {
            ring: Arc::new(SpscRing::with_capacity(capacity)),
            estimator: P95Jitter::new(),
            hold_us: Arc::new(AtomicU32::new(MIN_HOLD.as_micros() as u32)),
            stats: Arc::new(MicroStats::default()),
            origin,
            current_hold_us: MIN_HOLD.as_micros() as u32,
            last_pushed: None,
            last_emitted: None,
        }
    }

    /// Network thread: a reassembled frame is ready.
    pub fn push(&mut self, frame: BufferedFrame) -> bool {
        if self.last_pushed.is_some_and(|last| frame.frame_id <= last) {
            self.stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.last_pushed = Some(frame.frame_id);

        let arrival_us = frame
            .arrival
            .saturating_duration_since(self.origin)
            .as_micros() as u64;
        self.estimator.observe(frame.timestamp_us, arrival_us);

        let target = self.estimator.target_hold_us();
        // Grow immediately, shrink at 100 µs per frame: a shrunk buffer must
        // never re-expose a jitter spike it just learned about.
        self.current_hold_us = if target > self.current_hold_us {
            target
        } else {
            self.current_hold_us.saturating_sub(100).max(target)
        };
        self.hold_us.store(self.current_hold_us, Ordering::Relaxed);

        match self.ring.push(frame) {
            Ok(()) => {
                self.stats.pushed.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Decoder thread: emit the newest frame whose hold has expired.
    pub fn next(&mut self, now: Instant) -> Option<BufferedFrame> {
        let mut newest: Option<BufferedFrame> = None;
        while let Some(frame) = self.ring.pop() {
            let replaced = newest
                .as_ref()
                .is_some_and(|current| frame.frame_id > current.frame_id);
            if newest.is_some() {
                self.stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
            }
            if newest.is_none() || replaced {
                newest = Some(frame);
            }
        }
        let frame = newest?;
        if self.last_emitted.is_some_and(|last| frame.frame_id <= last) {
            self.stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let due = frame.arrival + Duration::from_micros(u64::from(self.current_hold_us));
        if now < due {
            // Not ready yet; keep it for the next tick by re-pushing.
            let _ = self.ring.push(frame);
            return None;
        }
        self.last_emitted = Some(frame.frame_id);
        self.stats.emitted.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    pub fn hold_delay(&self) -> Duration {
        Duration::from_micros(u64::from(self.current_hold_us))
    }

    pub fn p95_jitter_us(&self) -> u32 {
        self.estimator.p95_us()
    }

    /// Stream discontinuity: ids restart, jitter history is meaningless.
    pub fn reset_stream(&mut self) {
        self.estimator.reset();
        self.current_hold_us = MIN_HOLD.as_micros() as u32;
        self.hold_us.store(self.current_hold_us, Ordering::Relaxed);
        self.last_pushed = None;
        self.last_emitted = None;
        while self.ring.pop().is_some() {}
    }

    pub fn stats(&self) -> MicroJitterSnapshot {
        MicroJitterSnapshot {
            pushed: self.stats.pushed.load(Ordering::Relaxed),
            emitted: self.stats.emitted.load(Ordering::Relaxed),
            dropped_stale: self.stats.dropped_stale.load(Ordering::Relaxed),
            dropped_full: self.stats.dropped_full.load(Ordering::Relaxed),
            p95_jitter_us: self.estimator.p95_us(),
            hold_us: self.current_hold_us,
            queue_depth: self.ring.len() as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(id: u32, arrival: Instant) -> BufferedFrame {
        BufferedFrame {
            frame_id: id,
            timestamp_us: u64::from(id) * 16_667,
            codec: 1,
            is_keyframe: false,
            arrival,
            payload: Bytes::from_static(b"nal"),
        }
    }

    #[test]
    fn intra_refresh_disables_periodic_idr() {
        let cfg = NvencIntraRefresh::default();
        assert!(cfg.enable_intra_refresh);
        assert_eq!(cfg.intra_refresh_period, 45);
        assert_eq!(cfg.intra_refresh_cnt, 5);
        assert_eq!(cfg.idr_period, NVENC_INFINITE_GOPLENGTH);
    }

    #[test]
    fn sanitizing_keeps_refresh_count_below_period() {
        let cfg = NvencIntraRefresh {
            enable_intra_refresh: true,
            intra_refresh_period: 4,
            intra_refresh_cnt: 99,
            idr_period: 120,
        }
        .sanitized();
        assert_eq!(cfg.intra_refresh_cnt, 3);
        assert_eq!(cfg.idr_period, NVENC_INFINITE_GOPLENGTH);
    }

    #[test]
    fn on_demand_wave_never_becomes_an_idr() {
        let wave = NvencIntraRefresh::default().on_demand_wave();
        assert_eq!(wave.idr_period, NVENC_INFINITE_GOPLENGTH);
        let recovery = wave.to_recovery_config();
        assert!(recovery.enabled);
        assert_eq!(recovery.refresh_frames, 5);
    }

    #[test]
    fn clean_lan_holds_the_two_millisecond_floor() {
        let buffer = MicroJitterBuffer::new(Instant::now(), 8);
        assert_eq!(buffer.estimator.target_hold_us(), 2_000);
    }

    #[test]
    fn cellular_jitter_clamps_at_the_fourteen_millisecond_ceiling() {
        let mut buffer = MicroJitterBuffer::new(Instant::now(), 8);
        for i in 0..120u64 {
            let jitter = if i % 2 == 0 { 0 } else { 60_000 };
            buffer
                .estimator
                .observe(i * 16_667, i * 16_667 + jitter);
        }
        assert!(buffer.estimator.p95_us() >= 30_000);
        assert_eq!(buffer.estimator.target_hold_us(), MAX_HOLD.as_micros() as u32);
    }

    #[test]
    fn newer_frame_discards_the_older_one() {
        let t0 = Instant::now();
        let mut buffer = MicroJitterBuffer::new(t0, 8);
        assert!(buffer.push(frame(1, t0)));
        assert!(buffer.push(frame(2, t0 + Duration::from_micros(500))));
        let out = buffer.next(t0 + Duration::from_millis(50)).expect("frame");
        assert_eq!(out.frame_id, 2);
        assert!(buffer.stats().dropped_stale >= 1);
    }

    #[test]
    fn hold_deadline_is_respected_then_released() {
        let t0 = Instant::now();
        let mut buffer = MicroJitterBuffer::new(t0, 8);
        assert!(buffer.push(frame(1, t0)));
        assert!(buffer.next(t0 + Duration::from_micros(100)).is_none());
        assert!(buffer.next(t0 + Duration::from_millis(5)).is_some());
    }

    #[test]
    fn stream_reset_accepts_restarted_frame_ids() {
        let t0 = Instant::now();
        let mut buffer = MicroJitterBuffer::new(t0, 8);
        assert!(buffer.push(frame(500, t0)));
        assert_eq!(buffer.next(t0 + Duration::from_millis(20)).map(|f| f.frame_id), Some(500));
        buffer.reset_stream();
        assert!(buffer.push(frame(1, t0 + Duration::from_millis(25))));
        assert_eq!(
            buffer.next(t0 + Duration::from_millis(45)).map(|f| f.frame_id),
            Some(1)
        );
    }
}
