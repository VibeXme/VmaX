//! Per-frame telemetry: the 16-byte trailer every video header carries, plus
//! the client-side smoothing that turns raw samples into the numbers the top
//! bar renders.
//!
//! Layout appended to [`crate::protocol::VideoChunkHeader`]:
//!
//! ```text
//! off  size field
//! 0    4    host_encode_us   u32   CSC start -> NvEncLockBitstream done
//! 4    1    codec_id         u8    0 H.264, 1 HEVC, 2 HEVC 4:4:4, 3 HEVC P010
//! 5    1    chroma_range     u8    bit0 full range, bit1 4:4:4, bit2 hardware
//! 6    2    width            u16
//! 8    2    height           u16
//! 10   2    reserved         u16   (must be 0)
//! 12   4    capture_to_send_us u32 (capture timestamp -> datagram submit)
//! ```
//!
//! `frame_id` (u64) already lives in the fixed header and keys every sample.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const TELEMETRY_LEN: usize = 16;

pub const FLAG_FULL_RANGE: u8 = 1 << 0;
pub const FLAG_444: u8 = 1 << 1;
pub const FLAG_HARDWARE: u8 = 1 << 2;

/// Values per frame, written by the host and read by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTelemetry {
    pub host_encode_us: u32,
    pub codec_id: u8,
    pub chroma_and_range: u8,
    pub width: u16,
    pub height: u16,
    pub capture_to_send_us: u32,
}

impl FrameTelemetry {
    pub fn full_range_hardware(codec_id: u8, width: u16, height: u16) -> Self {
        Self {
            host_encode_us: 0,
            codec_id,
            chroma_and_range: FLAG_FULL_RANGE | FLAG_HARDWARE,
            width,
            height,
            capture_to_send_us: 0,
        }
    }

    pub fn is_full_range(&self) -> bool {
        self.chroma_and_range & FLAG_FULL_RANGE != 0
    }

    pub fn is_444(&self) -> bool {
        self.chroma_and_range & FLAG_444 != 0
    }

    pub fn is_hardware(&self) -> bool {
        self.chroma_and_range & FLAG_HARDWARE != 0
    }

    pub fn encode(&self, out: &mut bytes::BytesMut) {
        out.extend_from_slice(&self.host_encode_us.to_le_bytes());
        out.extend_from_slice(&[self.codec_id, self.chroma_and_range]);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.capture_to_send_us.to_le_bytes());
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < TELEMETRY_LEN {
            return None;
        }
        Some(Self {
            host_encode_us: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            codec_id: buf[4],
            chroma_and_range: buf[5],
            width: u16::from_le_bytes([buf[6], buf[7]]),
            height: u16::from_le_bytes([buf[8], buf[9]]),
            capture_to_send_us: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        })
    }
}

/// Exponential moving average used for every displayed latency.
#[derive(Debug, Clone, Copy)]
pub struct Ema {
    alpha: f32,
    value: Option<f32>,
}

impl Ema {
    pub const fn new(alpha: f32) -> Self {
        Self { alpha, value: None }
    }

    pub fn push(&mut self, sample: f32) -> f32 {
        let v = match self.value {
            // Seed on the first sample so the bar does not ramp from zero.
            None => sample,
            Some(prev) => prev + self.alpha * (sample - prev),
        };
        self.value = Some(v);
        v
    }

    pub fn value(&self) -> Option<f32> {
        self.value
    }
}

/// Client-side decode timing, keyed by the host's `frame_id`.
///
/// The sample is opened when the reassembled access unit is submitted to the
/// hardware decoder and closed when the decoded surface is dequeued, so the
/// number is real decoder latency rather than a structure size estimate.
#[derive(Debug)]
pub struct DecodeProbe {
    pending: VecDeque<(u64, Instant)>,
    capacity: usize,
}

impl DecodeProbe {
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn begin(&mut self, frame_id: u64, now: Instant) {
        if self.pending.len() >= self.capacity {
            self.pending.pop_front();
        }
        self.pending.push_back((frame_id, now));
    }

    /// Close the sample for `frame_id`. Returns microseconds, or `None` if the
    /// frame was never submitted (e.g. an FEC-only or keepalive packet).
    pub fn finish(&mut self, frame_id: u64, now: Instant) -> Option<u32> {
        if let Some(pos) = self.pending.iter().position(|(id, _)| *id == frame_id) {
            let (_, start) = self.pending.remove(pos)?;
            return Some(
                now.saturating_duration_since(start)
                    .as_micros()
                    .min(u128::from(u32::MAX)) as u32,
            );
        }
        None
    }

    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

/// Sliding one-second byte counter for the displayed bitrate. Counts every
/// datagram that belongs to the session: video, audio and FEC parity.
#[derive(Debug)]
pub struct BitrateCounter {
    started: Instant,
    buckets: std::collections::VecDeque<(Instant, u64)>,
    bytes: u64,
    window: Duration,
    ready: bool,
}

impl BitrateCounter {
    pub fn new(window: Duration) -> Self {
        Self {
            started: Instant::now(),
            buckets: std::collections::VecDeque::new(),
            bytes: 0,
            window,
            ready: false,
        }
    }

    pub fn add(&mut self, bytes: usize, now: Instant) {
        self.ready = now.saturating_duration_since(self.started) >= self.window;
        while self
            .buckets
            .front()
            .is_some_and(|(stamp, _)| now.saturating_duration_since(*stamp) > self.window)
        {
            if let Some((_, expired)) = self.buckets.pop_front() {
                self.bytes -= expired;
            }
        }
        if bytes == 0 {
            return;
        }
        let bytes = bytes as u64;
        if let Some((stamp, count)) = self.buckets.back_mut()
            && now.saturating_duration_since(*stamp) < Duration::from_millis(20)
        {
            *count += bytes;
        } else {
            self.buckets.push_back((now, bytes));
        }
        self.bytes += bytes;
    }

    pub fn mbps(&self) -> f32 {
        (self.bits_per_second() / 1_000_000.0) as f32
    }

    pub fn bits_per_second(&self) -> f64 {
        if self.ready {
            self.bytes as f64 * 8.0 / self.window.as_secs_f64().max(1e-3)
        } else {
            0.0
        }
    }
}

/// Everything the telemetry bar renders, emitted at 4-10 Hz.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryTick {
    pub decode_ms: f32,
    pub encode_ms: f32,
    pub network_ms: f32,
    pub bitrate_mbps: f32,
    pub hardware: bool,
    pub codec: String,
    pub width: u16,
    pub height: u16,
    pub chroma: String,
    pub range: String,
    pub audio: String,
    /// True while the host is in idle heartbeat mode (no frame traffic).
    pub idle: bool,
}

/// Aggregates the EMA-smoothed values. One instance per client session.
#[derive(Debug)]
pub struct TelemetryCore {
    encode: Ema,
    decode: Ema,
    network: Ema,
    pub bitrate: BitrateCounter,
    pub probe: DecodeProbe,
    last_frame: Option<FrameTelemetry>,
    pub idle: bool,
    audio_label: String,
}

pub const EMA_ALPHA: f32 = 0.15;

impl TelemetryCore {
    pub fn new() -> Self {
        Self {
            // 30-frame EMA at alpha 0.15: the displayed value settles in about
            // 30 samples and stops jittering at the 4 Hz emit rate.
            encode: Ema::new(EMA_ALPHA),
            decode: Ema::new(EMA_ALPHA),
            network: Ema::new(EMA_ALPHA),
            bitrate: BitrateCounter::new(Duration::from_secs(1)),
            probe: DecodeProbe::new(120),
            last_frame: None,
            idle: false,
            audio_label: "Opus Audio".to_string(),
        }
    }

    pub fn set_audio_label(&mut self, label: impl Into<String>) {
        self.audio_label = label.into();
    }

    pub fn on_frame_meta(&mut self, meta: FrameTelemetry) {
        if meta.host_encode_us > 0 {
            self.encode.push(meta.host_encode_us as f32 / 1000.0);
        }
        self.last_frame = Some(meta);
    }

    pub fn on_decode(&mut self, micros: u32) {
        self.decode.push(micros as f32 / 1000.0);
    }

    /// `rtt_ms` is the QUIC round trip; one-way latency is half of it.
    pub fn on_rtt(&mut self, rtt_ms: f32) {
        self.network.push((rtt_ms / 2.0).max(0.0));
    }

    pub fn tick(&self) -> TelemetryTick {
        let last = self.last_frame;
        let hardware = last.map(|m| m.is_hardware()).unwrap_or(true);
        let (width, height) = last.map(|m| (m.width, m.height)).unwrap_or((0, 0));
        TelemetryTick {
            decode_ms: self.decode.value().unwrap_or(0.0),
            encode_ms: self.encode.value().unwrap_or(0.0),
            network_ms: self.network.value().unwrap_or(0.0),
            bitrate_mbps: self.bitrate.mbps(),
            hardware,
            codec: last
                .map(|m| crate::nvenc::CodecId::label_for_wire(m.codec_id))
                .unwrap_or("H.265")
                .to_string(),
            width,
            height,
            chroma: if last.is_some_and(|m| m.is_444()) {
                "4:4:4".to_string()
            } else {
                "4:2:0".to_string()
            },
            range: if last.map(|m| m.is_full_range()).unwrap_or(true) {
                "Full Range".to_string()
            } else {
                "Limited".to_string()
            },
            audio: self.audio_label.clone(),
            idle: self.idle,
        }
    }
}

impl Default for TelemetryCore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_record_is_sixteen_bytes_and_round_trips() {
        let meta = FrameTelemetry {
            host_encode_us: 2450,
            codec_id: 1,
            chroma_and_range: FLAG_FULL_RANGE | FLAG_HARDWARE,
            width: 1920,
            height: 1080,
            capture_to_send_us: 900,
        };
        let mut buf = bytes::BytesMut::new();
        meta.encode(&mut buf);
        assert_eq!(buf.len(), TELEMETRY_LEN);
        let decoded = FrameTelemetry::decode(&buf).expect("decode");
        assert_eq!(decoded, meta);
        assert!(decoded.is_full_range());
        assert!(decoded.is_hardware());
        assert!(!decoded.is_444());
        assert!(FrameTelemetry::decode(&buf[..4]).is_none());
    }

    #[test]
    fn ema_seeds_on_the_first_sample_then_smooths() {
        let mut ema = Ema::new(0.15);
        assert_eq!(ema.push(10.0), 10.0);
        assert!((ema.push(20.0) - 11.5).abs() < 1e-4);
        assert_eq!(ema.value().unwrap_or_default(), 11.5);
    }

    #[test]
    fn decode_probe_measures_submission_to_dequeue() {
        let mut probe = DecodeProbe::new(4);
        let start = Instant::now();
        probe.begin(7, start);
        assert_eq!(probe.in_flight(), 1);
        let d = probe
            .finish(7, start + Duration::from_micros(5_500))
            .expect("sample");
        assert_eq!(d, 5_500);
        assert_eq!(probe.finish(7, start).is_none(), true);
        assert_eq!(probe.in_flight(), 0);
    }

    #[test]
    fn decode_probe_is_bounded() {
        let mut probe = DecodeProbe::new(2);
        let now = Instant::now();
        for id in 0..10 {
            probe.begin(id, now);
        }
        assert_eq!(probe.in_flight(), 2);
    }

    #[test]
    fn bitrate_counter_reports_over_a_one_second_window() {
        let mut counter = BitrateCounter::new(Duration::from_secs(1));
        let start = Instant::now();
        counter.add(1_000_000, start);
        assert_eq!(counter.mbps(), 0.0, "no window has elapsed yet");
        // 8_000_000 bits in exactly one second = 8 Mbps.
        counter.add(0, start + Duration::from_secs(1));
        assert!((counter.mbps() - 8.0).abs() < 1e-3);
        counter.add(0, start + Duration::from_millis(1_021));
        assert_eq!(counter.mbps(), 0.0, "old traffic expires during idle");
    }

    #[test]
    fn tick_reports_full_range_hardware_hevc_text() {
        let mut core = TelemetryCore::new();
        core.on_frame_meta(FrameTelemetry::full_range_hardware(1, 1920, 1080));
        core.on_decode(5_500);
        core.on_rtt(45.7);
        let tick = core.tick();
        assert_eq!(tick.codec, "H.265");
        assert_eq!(tick.chroma, "4:2:0");
        assert_eq!(tick.range, "Full Range");
        assert!(tick.hardware);
        assert_eq!((tick.width, tick.height), (1920, 1080));
        assert!((tick.decode_ms - 5.5).abs() < 1e-4);
        assert!((tick.network_ms - 22.85).abs() < 1e-3);
        assert_eq!(tick.audio, "Opus Audio");
    }

    #[test]
    fn empty_core_does_not_panic_or_report_nonsense() {
        let tick = TelemetryCore::new().tick();
        assert_eq!(tick.width, 0);
        assert_eq!(tick.range, "Full Range");
    }
}
