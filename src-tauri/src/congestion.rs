//! Delay-gradient congestion control (Google Congestion Control style) for VmaX.
//!
//! The host never guesses about the last mile.  The Android client measures the
//! arrival time of every video shard and echoes a compact 16 ms feedback report
//! back to the host.  The host turns `send_delta` / `recv_delta` into an
//! accumulated one-way-delay trend, detects bufferbloat before loss, and emits
//! a target bitrate for the NVENC rate-control loop.
//!
//! Wire compatibility:
//! * Feedback reports use channel `0x05` and a fixed 248-byte little-endian
//!   `FeedbackPacket`.  They are deliberately small enough for a single
//!   unfragmented mobile datagram.
//! * The media packet format is unchanged.  Samples reference an existing
//!   `(frame_id, chunk_idx)` pair, which is already present in
//!   `VideoChunkHeader`.
//!
//! Reference behaviour is modelled on WebRTC's `AimdRateControl`,
//! `TrendlineEstimator` and `OveruseDetector`, simplified for a one-way video
//! stream with a host-side encoder.

use crate::bitrate::{BitrateDecision, CongestionState};
use crate::encoder_recovery::NvencRateParams;
use crate::protocol::ChannelId;
use bytemuck::{Pod, Zeroable};
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const FEEDBACK_MAGIC: u32 = 0x564D_5846; // "VMXF"
pub const FEEDBACK_VERSION: u8 = 1;
pub const MAX_ARRIVAL_SAMPLES: usize = 12;
/// 100 Hz delay-gradient sampling. Faster than a 60 Hz frame so a transit
/// increase is caught before the modem buffer fills and packets drop.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(10);

pub const DEFAULT_MIN_BITRATE_BPS: u32 = 2_000_000;
pub const DEFAULT_MAX_BITRATE_BPS: u32 = crate::bitrate::MAX_VIDEO_BITRATE_BPS;

/// The client has packets missing from `nack_bitmap`.
pub const FLAG_NACK: u8 = 1 << 0;
/// The client explicitly asks for a reference refresh (RFI).
pub const FLAG_RFI: u8 = 1 << 1;
/// At least one NACK bit is set for a *complete* frame that the decoder needs.
pub const FLAG_FRAME_DAMAGED: u8 = 1 << 2;
const ARRIVAL_PARITY: u16 = 1;

/// One video shard arrival.  `frame_id` and `chunk_idx` map back to the
/// `VideoChunkHeader` the host sent, so the host can recover the matching send
/// timestamp without changing the media packet layout.
///
/// Parity uses a distinct sample flag because parity and data indices can overlap.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct ArrivalSample {
    pub frame_id: u32,
    pub chunk_idx: u16,
    pub flags: u16,
    pub arrival_ts_us: u64,
}

/// Fixed-size ACK / congestion feedback datagram (channel `0x05`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct FeedbackPacket {
    pub magic: u32,
    pub version: u8,
    pub flags: u8,
    pub sample_count: u8,
    pub reserved: u8,
    pub report_seq: u32,
    /// Client's current RTT estimate in microseconds (from QUIC / Ping).
    pub rtt_us: u32,
    /// Client monotonic clock, microseconds.  Only differences matter.
    pub client_ts_us: u64,
    pub highest_frame_id: u32,
    /// Highest frame that the reassembler and FEC fully recovered.
    pub last_intact_frame_id: u32,
    /// Bit `i` means frame `highest_frame_id - i` has at least one missing shard.
    pub nack_bitmap: u64,
    /// Cumulative video payload bytes received, for throughput estimation.
    pub recv_video_bytes: u64,
    /// Client-measured RFC 3550 interarrival jitter.
    pub recv_jitter_us: u32,
    /// Feedback-report loss measured by the host.  The client leaves this zero.
    pub ack_loss_permille: u16,
    /// Media loss estimated by the client.
    pub loss_permille: u16,
    pub samples: [ArrivalSample; MAX_ARRIVAL_SAMPLES],
}

const _: () = assert!(core::mem::size_of::<FeedbackPacket>() == 248);
const _: () = assert!(core::mem::size_of::<ArrivalSample>() == 16);

impl FeedbackPacket {
    pub fn new(report_seq: u32, client_ts_us: u64) -> Self {
        Self {
            magic: FEEDBACK_MAGIC,
            version: FEEDBACK_VERSION,
            report_seq,
            client_ts_us,
            ..Self::default()
        }
    }

    /// Wire bytes including the one-byte VmaX channel tag.
    pub fn encode_datagram(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(1 + core::mem::size_of::<Self>());
        out.put_u8(ChannelId::Feedback as u8);
        out.extend_from_slice(bytemuck::bytes_of(self));
        out.freeze()
    }

    pub fn decode_datagram(buf: &[u8]) -> Option<Self> {
        if buf.len() != 1 + core::mem::size_of::<Self>()
            || buf.first().copied() != Some(ChannelId::Feedback as u8)
        {
            return None;
        }
        let packet: Self = bytemuck::pod_read_unaligned(&buf[1..]);
        if packet.magic != FEEDBACK_MAGIC
            || packet.version != FEEDBACK_VERSION
            || packet.sample_count as usize > MAX_ARRIVAL_SAMPLES
        {
            return None;
        }
        Some(packet)
    }

    pub fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// Client-side feedback builder.  Call `on_video_datagram` from the datagram
/// receive loop, `note_frame_completed` when the reassembler emits a full
/// frame, then `build` every 16 ms from the client's telemetry tick.
pub struct FeedbackBuilder {
    origin: Instant,
    report_seq: u32,
    samples: VecDeque<ArrivalSample>,
    last_report: Instant,
    last_arrival_us: Option<u64>,
    last_sent_us: Option<u64>,
    last_jitter_frame: Option<u32>,
    jitter_us: f64,
    recv_video_bytes: u64,
    highest_frame_id: u32,
    last_intact_frame_id: u32,
    nack_bitmap: u64,
    loss_permille: u16,
    rtt_us: u32,
    flags: u8,
}

impl Default for FeedbackBuilder {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            report_seq: 0,
            samples: VecDeque::with_capacity(MAX_ARRIVAL_SAMPLES),
            last_report: Instant::now(),
            last_arrival_us: None,
            last_sent_us: None,
            last_jitter_frame: None,
            jitter_us: 0.0,
            recv_video_bytes: 0,
            highest_frame_id: 0,
            last_intact_frame_id: 0,
            nack_bitmap: 0,
            loss_permille: 0,
            rtt_us: 0,
            flags: 0,
        }
    }
}

impl FeedbackBuilder {
    pub fn new(now: Instant) -> Self {
        Self {
            origin: now,
            last_report: now,
            ..Self::default()
        }
    }

    /// `sent_ts_us` is `VideoChunkHeader::timestamp_us` (capture timestamp on
    /// the host).  `arrival` is the client monotonic Instant.
    pub fn on_video_datagram(
        &mut self,
        frame_id: u32,
        chunk_idx: u16,
        sent_ts_us: u64,
        arrival: Instant,
        payload_len: usize,
        parity: bool,
    ) {
        self.highest_frame_id = self.highest_frame_id.max(frame_id);
        self.recv_video_bytes = self.recv_video_bytes.saturating_add(payload_len as u64);

        let arrival_us = arrival.saturating_duration_since(self.origin).as_micros() as u64;
        if self.last_jitter_frame.is_none_or(|last| frame_id > last) {
            if let (Some(prev_arrival), Some(prev_sent)) = (self.last_arrival_us, self.last_sent_us)
            {
                // RFC 3550 interarrival jitter, using capture timestamps to remove
                // the host's frame cadence from the arrival delta.
                let recv_delta = arrival_us.saturating_sub(prev_arrival) as i64;
                let send_delta = sent_ts_us.saturating_sub(prev_sent) as i64;
                let d = (recv_delta - send_delta).unsigned_abs() as f64;
                self.jitter_us += (d - self.jitter_us) / 16.0;
            }
            self.last_arrival_us = Some(arrival_us);
            self.last_sent_us = Some(sent_ts_us);
            self.last_jitter_frame = Some(frame_id);
        }

        self.samples.push_back(ArrivalSample {
            frame_id,
            chunk_idx,
            flags: if parity { ARRIVAL_PARITY } else { 0 },
            arrival_ts_us: arrival_us,
        });
        while self.samples.len() > MAX_ARRIVAL_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// `frame_id` is complete and decodable.  This is the value echoed in
    /// `last_intact_frame_id` and drives host-side RFI decisions.
    pub fn note_frame_completed(&mut self, frame_id: u32) {
        self.note_completed_video(frame_id, false);
    }

    pub fn note_completed_video(&mut self, frame_id: u32, independent: bool) {
        if independent
            || self.last_intact_frame_id == 0
            || frame_id == self.last_intact_frame_id.wrapping_add(1)
        {
            self.last_intact_frame_id = frame_id;
        } else if frame_id > self.last_intact_frame_id {
            self.note_missing(self.last_intact_frame_id.wrapping_add(1));
        }
    }

    pub fn note_missing(&mut self, frame_id: u32) {
        if frame_id > self.highest_frame_id {
            return;
        }
        let bit = self.highest_frame_id.saturating_sub(frame_id);
        if bit < 64 {
            self.nack_bitmap |= 1u64 << bit;
            self.flags |= FLAG_NACK | FLAG_FRAME_DAMAGED;
        }
    }

    pub fn note_rfi(&mut self) {
        self.flags |= FLAG_RFI;
    }

    pub fn set_loss_permille(&mut self, loss_permille: u16) {
        self.loss_permille = loss_permille;
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt_us = rtt.as_micros().min(u128::from(u32::MAX)) as u32;
    }

    pub fn p95_jitter_us(&self) -> u32 {
        self.jitter_us.min(f64::from(u32::MAX)) as u32
    }

    /// Returns `None` until the 20 ms report interval has elapsed.
    pub fn build(&mut self, now: Instant) -> Option<FeedbackPacket> {
        if now.saturating_duration_since(self.last_report) < FEEDBACK_INTERVAL {
            return None;
        }
        self.last_report = now;

        let count = self.samples.len().min(MAX_ARRIVAL_SAMPLES);
        let mut samples = [ArrivalSample::default(); MAX_ARRIVAL_SAMPLES];
        for (dst, sample) in samples[..count]
            .iter_mut()
            .zip(self.samples.iter().skip(self.samples.len() - count))
        {
            *dst = *sample;
        }
        self.samples.clear();

        let packet = FeedbackPacket {
            magic: FEEDBACK_MAGIC,
            version: FEEDBACK_VERSION,
            flags: std::mem::take(&mut self.flags),
            sample_count: count as u8,
            reserved: 0,
            report_seq: self.report_seq,
            rtt_us: self.rtt_us,
            client_ts_us: now.saturating_duration_since(self.origin).as_micros() as u64,
            highest_frame_id: self.highest_frame_id,
            last_intact_frame_id: self.last_intact_frame_id,
            nack_bitmap: self.nack_bitmap,
            recv_video_bytes: self.recv_video_bytes,
            recv_jitter_us: self.p95_jitter_us(),
            ack_loss_permille: 0,
            loss_permille: self.loss_permille,
            samples,
        };
        self.report_seq = self.report_seq.wrapping_add(1);
        self.nack_bitmap = 0;
        Some(packet)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayState {
    Normal,
    Overusing,
    Underusing,
}

/// Sliding-window linear regression over accumulated one-way delay.
pub struct DelayGradient {
    window: VecDeque<(f64, f64)>, // (time_us since first sample, accumulated_delay_us)
    accumulated_delay_us: f64,
    previous: Option<(u64, u64)>, // (send_us, arrival_us)
    state: DelayState,
    state_since: Option<Instant>,
    threshold_ms_per_s: f64,
    hold: Duration,
    last_trend_ms_per_s: f64,
}

impl DelayGradient {
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(24),
            accumulated_delay_us: 0.0,
            previous: None,
            state: DelayState::Normal,
            state_since: None,
            // WebRTC's trendline threshold is ~12.5 ms/s; start slightly
            // tighter because game traffic is delay sensitive.
            threshold_ms_per_s: 9.0,
            hold: Duration::from_millis(10),
            last_trend_ms_per_s: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.previous = None;
        self.accumulated_delay_us = 0.0;
        self.state = DelayState::Normal;
        self.state_since = None;
        self.last_trend_ms_per_s = 0.0;
    }

    /// Feed a pair of timestamps in microseconds.  `send_us` and `arrival_us`
    /// may use different clock origins; the offset cancels in the deltas.
    pub fn push(&mut self, send_us: u64, arrival_us: u64, now: Instant) -> DelayState {
        let Some((prev_send, prev_arrival)) = self.previous.replace((send_us, arrival_us)) else {
            return self.state;
        };

        if send_us < prev_send || arrival_us < prev_arrival {
            self.previous = Some((prev_send, prev_arrival));
            return self.state;
        }
        let send_delta = send_us.saturating_sub(prev_send) as f64;
        let recv_delta = arrival_us.saturating_sub(prev_arrival) as f64;
        let delay_delta = recv_delta - send_delta;
        // Clamp pathological counter jumps (clock changes, wrap).
        let delay_delta = delay_delta.clamp(-250_000.0, 250_000.0);
        self.accumulated_delay_us += delay_delta;

        let x = arrival_us as f64;
        self.window.push_back((x, self.accumulated_delay_us));
        while self.window.len() > 20 {
            self.window.pop_front();
        }
        self.last_trend_ms_per_s = self.trend_slope_ms_per_s();
        self.update_state(now)
    }

    fn trend_slope_ms_per_s(&self) -> f64 {
        if self.window.len() < 3 {
            return 0.0;
        }
        let n = self.window.len() as f64;
        let x0 = self.window.front().map(|s| s.0).unwrap_or(0.0);
        let (mut sum_x, mut sum_y, mut sum_xy, mut sum_xx) = (0.0, 0.0, 0.0, 0.0);
        for &(x, y) in &self.window {
            let x = x - x0;
            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_xx += x * x;
        }
        let denom = n * sum_xx - sum_x * sum_x;
        if denom.abs() < f64::EPSILON {
            return 0.0;
        }
        // slope is us/us.  Convert to ms/s: (us/1000) / (us/1e6) = slope*1000.
        let slope = (n * sum_xy - sum_x * sum_y) / denom;
        slope * 1000.0
    }

    fn update_state(&mut self, now: Instant) -> DelayState {
        let trend = self.last_trend_ms_per_s;
        let next = if trend > self.threshold_ms_per_s {
            DelayState::Overusing
        } else if trend < -self.threshold_ms_per_s {
            DelayState::Underusing
        } else {
            DelayState::Normal
        };

        if next == self.state {
            self.state_since = None;
            return self.state;
        }
        let since = self.state_since.get_or_insert(now);
        if now.saturating_duration_since(*since) >= self.hold {
            self.state = next;
            self.state_since = None;
        }
        self.state
    }

    pub fn state(&self) -> DelayState {
        self.state
    }

    fn queue_growth_us(&self) -> f64 {
        let baseline = self.window.iter().map(|(_, delay)| *delay)
            .fold(self.window.front().map_or(0.0, |(_, delay)| *delay).min(self.accumulated_delay_us), f64::min);
        self.accumulated_delay_us - baseline
    }

    pub fn trend_ms_per_s(&self) -> f64 {
        self.last_trend_ms_per_s
    }
}

impl Default for DelayGradient {
    fn default() -> Self {
        Self::new()
    }
}

/// Send-side history used to map an echoed `(frame_id, chunk_idx)` back to the
/// host monotonic send timestamp.
struct SendHistory {
    origin: Instant,
    by_key: HashMap<u64, (u32, u64)>, // key -> (bytes, sent_us)
    order: VecDeque<(u64, Instant)>,
    capacity: usize,
}

impl SendHistory {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            by_key: HashMap::with_capacity(2048),
            order: VecDeque::with_capacity(2048),
            capacity: 4096,
        }
    }

    fn key(frame_id: u32, chunk_idx: u16, parity: bool) -> u64 {
        (u64::from(frame_id) << 17) | (u64::from(parity) << 16) | u64::from(chunk_idx)
    }

    fn record(&mut self, frame_id: u32, chunk_idx: u16, parity: bool, bytes: usize, sent: Instant) {
        let key = Self::key(frame_id, chunk_idx, parity);
        let sent_us = sent.saturating_duration_since(self.origin).as_micros() as u64;
        self.by_key
            .insert(key, (bytes.min(u32::MAX as usize) as u32, sent_us));
        self.order.push_back((key, sent));
        while self.order.len() > self.capacity {
            if let Some((old, _)) = self.order.pop_front() {
                self.by_key.remove(&old);
            }
        }
    }

    fn lookup(&self, frame_id: u32, chunk_idx: u16, parity: bool) -> Option<(u32, u64)> {
        self.by_key.get(&Self::key(frame_id, chunk_idx, parity)).copied()
    }
}

struct AckLossTracker {
    window: VecDeque<bool>,
    last_report_seq: Option<u32>,
}

impl AckLossTracker {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(128),
            last_report_seq: None,
        }
    }

    /// Returns the current ACK-loss estimate in permille.
    fn observe(&mut self, report_seq: u32) -> u16 {
        let Some(last) = self.last_report_seq else {
            self.last_report_seq = Some(report_seq);
            return 0;
        };
        let expected = last.wrapping_add(1);
        if report_seq == last || report_seq.wrapping_sub(last) > 1_000 {
            // Duplicate or reordered report; do not count it as loss.
            return self.permille();
        }
        let missing = report_seq.wrapping_sub(expected).min(64);
        for _ in 0..missing {
            self.window.push_back(true);
        }
        self.window.push_back(false);
        while self.window.len() > 100 {
            self.window.pop_front();
        }
        self.last_report_seq = Some(report_seq);
        self.permille()
    }

    fn permille(&self) -> u16 {
        if self.window.is_empty() {
            return 0;
        }
        let missing = self.window.iter().filter(|&&lost| lost).count();
        ((missing * 1000) / self.window.len()).min(1000) as u16
    }
}

struct ThroughputEstimator {
    last_bytes: Option<u64>,
    last_at: Option<Instant>,
    bps: u64,
}

impl ThroughputEstimator {
    fn new() -> Self {
        Self {
            last_bytes: None,
            last_at: None,
            bps: 0,
        }
    }

    fn observe(&mut self, total_bytes: u64, now: Instant) {
        let (Some(prev), Some(at)) = (self.last_bytes, self.last_at) else {
            self.last_bytes = Some(total_bytes);
            self.last_at = Some(now);
            return;
        };
        let dt = now.saturating_duration_since(at).as_secs_f64();
        if dt < 0.001 || total_bytes < prev {
            self.last_bytes = Some(total_bytes);
            self.last_at = Some(now);
            return;
        }
        let sample = ((total_bytes - prev) as f64 * 8.0 / dt) as u64;
        self.bps = if self.bps == 0 {
            sample
        } else {
            // Fast attack, slow decay: a stalled link must show up immediately.
            if sample < self.bps {
                (self.bps as f64 * 0.5 + sample as f64 * 0.5) as u64
            } else {
                (self.bps as f64 * 0.85 + sample as f64 * 0.15) as u64
            }
        };
        self.last_bytes = Some(total_bytes);
        self.last_at = Some(now);
    }

    fn bps(&self) -> u64 {
        self.bps
    }
}

/// Host-side controller.  Feed it every `FeedbackPacket`; it returns a
/// `BitrateDecision` that maps directly onto your existing
/// `NetworkEvent::BitrateUpdated` path.
pub struct GccController {
    min_bps: u32,
    max_bps: u32,
    current_bps: u32,
    last_notified_bps: u32,
    delay: DelayGradient,
    sends: SendHistory,
    ack_loss: AckLossTracker,
    throughput: ThroughputEstimator,
    last_increase: Option<Instant>,
    last_feedback_samples: Option<Instant>,
    cooldown_until: Option<Instant>,
    last_loss_permille: u16,
    last_ack_loss_permille: u16,
    last_jitter_us: u32,
    last_rtt_us: u32,
    min_rtt_us: u32,
}

impl GccController {
    pub fn new(initial_bps: u32, max_bps: u32) -> Self {
        let max_bps = max_bps.clamp(1, DEFAULT_MAX_BITRATE_BPS);
        let min_bps = DEFAULT_MIN_BITRATE_BPS.min(max_bps);
        Self {
            min_bps,
            max_bps,
            current_bps: initial_bps.clamp(min_bps, max_bps),
            last_notified_bps: initial_bps.clamp(min_bps, max_bps),
            delay: DelayGradient::new(),
            sends: SendHistory::new(),
            ack_loss: AckLossTracker::new(),
            throughput: ThroughputEstimator::new(),
            last_increase: None,
            last_feedback_samples: None,
            cooldown_until: None,
            last_loss_permille: 0,
            last_ack_loss_permille: 0,
            last_jitter_us: 0,
            last_rtt_us: 0,
            min_rtt_us: u32::MAX,
        }
    }

    pub fn current_bitrate_bps(&self) -> u32 {
        self.current_bps
    }

    pub fn set_max_bitrate_bps(&mut self, max_bps: u32) {
        self.max_bps = max_bps.clamp(1, DEFAULT_MAX_BITRATE_BPS);
        self.min_bps = DEFAULT_MIN_BITRATE_BPS.min(self.max_bps);
        self.current_bps = self.current_bps.clamp(self.min_bps, self.max_bps);
    }

    /// Record a data shard immediately before `quinn::Connection::send_datagram`.
    pub fn record_sent(&mut self, frame_id: u32, chunk_idx: u16, parity: bool, bytes: usize, sent: Instant) {
        self.sends.record(frame_id, chunk_idx, parity, bytes, sent);
    }

    /// Feed one client report.  `now` is the host receive time.
    pub fn on_feedback_at(&mut self, packet: &FeedbackPacket, now: Instant) -> BitrateDecision {
        self.last_ack_loss_permille = self.ack_loss.observe(packet.report_seq);
        self.last_loss_permille = packet.loss_permille.min(1000);
        self.last_jitter_us = packet.recv_jitter_us;
        self.last_rtt_us = packet.rtt_us;
        if packet.rtt_us > 0 {
            self.min_rtt_us = self.min_rtt_us.min(packet.rtt_us);
        }

        self.throughput.observe(packet.recv_video_bytes, now);

        // Reconstruct consecutive (send, arrival) pairs from the last samples.
        let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(packet.sample_count as usize);
        let count = packet.sample_count as usize;
        for sample in packet.samples.iter().take(count) {
            let parity = sample.flags & ARRIVAL_PARITY != 0;
            if let Some((_bytes, sent_us)) =
                self.sends.lookup(sample.frame_id, sample.chunk_idx, parity)
            {
                pairs.push((sent_us, sample.arrival_ts_us));
            }
        }
        pairs.sort_by_key(|&(_, arrival)| arrival);
        let has_samples = !pairs.is_empty();
        if !has_samples {
            // No shard arrived during this window: the screen is idle, the
            // sender paused, or every packet was lost. The last trend is
            // stale, and holding an Overusing state here would keep cutting
            // the encoder on each report while nothing is queued. Loss
            // backoff above still covers the total-loss case.
            if self.last_feedback_samples.is_some_and(|last| now.saturating_duration_since(last) > Duration::from_millis(80)) {
                self.delay.reset();
            }
        }
        if has_samples { self.last_feedback_samples = Some(now); }
        for pair in pairs {
            self.delay.push(pair.0, pair.1, now);
        }

        let delay_state = self.delay.state();
        let ack_loss = self.last_ack_loss_permille;
        let loss = self.last_loss_permille;
        let in_cooldown = self.cooldown_until.is_some_and(|until| now < until);
        let throughput_bps = self.throughput.bps();

        let (next_bps, state, reason) = if in_cooldown {
            (
                self.current_bps,
                CongestionState::Cooldown,
                "holding during post-congestion drain".into(),
            )
        } else if loss >= 50 || ack_loss >= 60 {
            // Loss fallback.  GCC is delay-first, but a 5%/6% loss link has
            // already crossed the point where delay probing is safe.
            let reduced = self.current_bps * 3 / 4;
            self.cooldown_until = Some(now + Duration::from_millis(500));
            (
                reduced,
                CongestionState::SevereCongestion,
                format!("loss backoff: media={loss}‰ ack={ack_loss}‰"),
            )
        } else if has_samples && (delay_state == DelayState::Overusing || self.delay.queue_growth_us() >= 4_000.0) {
            let throughput_cap = if throughput_bps > 0 {
                (throughput_bps as f64 * 0.90) as u32
            } else {
                u32::MAX
            };
            let reduced = (self.current_bps * 3 / 4).min(throughput_cap);
            self.cooldown_until = Some(now + Duration::from_millis(300));
            (
                reduced,
                CongestionState::SevereCongestion,
                format!(
                    "delay gradient {:.1} ms/s: queue building (rtt={:.1} ms)",
                    self.delay.trend_ms_per_s(),
                    packet.rtt_us as f64 / 1000.0
                ),
            )
        } else if loss >= 10 || ack_loss >= 30 {
            let reduced = ((self.current_bps as f64) * 0.90) as u32;
            self.cooldown_until = Some(now + Duration::from_millis(250));
            (
                reduced,
                CongestionState::MildCongestion,
                format!("mild loss backoff: media={loss}‰ ack={ack_loss}‰"),
            )
        } else {
            let can_increase = self.last_increase.is_none_or(|last| {
                now.saturating_duration_since(last) >= Duration::from_millis(100)
            });
            if can_increase && has_samples && loss < 5 && self.delay.trend_ms_per_s() <= 0.0 {
                self.last_increase = Some(now);
                let step = 40_000;
                (
                    self.current_bps.saturating_add(step),
                    CongestionState::RampingUp,
                    format!(
                        "clean path: trend={:.1} ms/s loss={}‰",
                        self.delay.trend_ms_per_s(),
                        loss
                    ),
                )
            } else {
                (
                    self.current_bps,
                    CongestionState::Stable,
                    "steady state".to_string(),
                )
            }
        };

        // Never let the target run far ahead of what the client actually
        // receives during a decrease; allow 25% headroom for FEC/pacing.
        let next_bps = if throughput_bps > 0
            && matches!(
                state,
                CongestionState::SevereCongestion | CongestionState::MildCongestion
            ) {
            next_bps.min((throughput_bps as f64 * 1.10) as u32)
        } else {
            next_bps
        };
        self.current_bps = next_bps.clamp(self.min_bps, self.max_bps);

        let diff = self.current_bps.abs_diff(self.last_notified_bps);
        let threshold = (self.last_notified_bps / 40).max(100_000);
        let encoder_reconfigure_recommended = diff >= threshold
            || (self.current_bps == self.min_bps && self.last_notified_bps != self.min_bps)
            || (self.current_bps == self.max_bps && self.last_notified_bps != self.max_bps);
        if encoder_reconfigure_recommended {
            self.last_notified_bps = self.current_bps;
        }

        BitrateDecision {
            target_bitrate_bps: self.current_bps,
            state,
            rtt_ms: packet.rtt_us as f64 / 1000.0,
            min_rtt_ms: if self.min_rtt_us == u32::MAX {
                0.0
            } else {
                f64::from(self.min_rtt_us) / 1000.0
            },
            loss_pct: f64::from(loss) / 10.0,
            reason,
            encoder_reconfigure_recommended,
        }
    }

    pub fn on_feedback(&mut self, packet: &FeedbackPacket) -> BitrateDecision {
        self.on_feedback_at(packet, Instant::now())
    }

    /// NVENC reconfiguration values for the current target.  `vbv_buffer_size`
    /// is deliberately one frame worth of bits, as requested: the encoder must
    /// never accumulate a multi-frame burst that the network then has to pace.
    pub fn nvenc_params(
        &self,
        fps: u16,
        ir: crate::encoder_recovery::IntraRefreshConfig,
    ) -> NvencRateParams {
        NvencRateParams::new(self.current_bps, u32::from(fps.max(1)), ir)
    }

    pub fn delay_trend_ms_per_s(&self) -> f64 {
        self.delay.trend_ms_per_s()
    }

    pub fn ack_loss_permille(&self) -> u16 {
        self.last_ack_loss_permille
    }

    pub fn jitter_us(&self) -> u32 {
        self.last_jitter_us
    }

    pub fn rtt_us(&self) -> u32 {
        self.last_rtt_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet_with_samples(
        samples: &[(u32, u16, u64)],
        now_us: u64,
        report_seq: u32,
    ) -> FeedbackPacket {
        let mut packet = FeedbackPacket::new(report_seq, now_us);
        packet.sample_count = samples.len() as u8;
        for (i, &(frame, chunk, arrival)) in samples.iter().enumerate() {
            packet.samples[i] = ArrivalSample {
                frame_id: frame,
                chunk_idx: chunk,
                flags: 0,
                arrival_ts_us: arrival,
            };
        }
        packet
    }

    #[test]
    fn feedback_packet_round_trips_on_the_wire() {
        let mut packet = FeedbackPacket::new(7, 123_456);
        packet.sample_count = 2;
        packet.samples[0] = ArrivalSample {
            frame_id: 9,
            chunk_idx: 4,
            flags: 0,
            arrival_ts_us: 999,
        };
        let wire = packet.encode_datagram();
        assert_eq!(wire.len(), 249);
        let decoded = FeedbackPacket::decode_datagram(&wire).expect("decode");
        assert_eq!(decoded.report_seq, 7);
        assert_eq!(decoded.sample_count, 2);
        assert_eq!(decoded.samples[0].frame_id, 9);
    }

    #[test]
    fn builder_emits_reports_after_the_interval() {
        let t0 = Instant::now();
        let mut builder = FeedbackBuilder::new(t0);
        builder.on_video_datagram(1, 0, 0, t0, 1200, false);
        assert!(builder.build(t0 + Duration::from_millis(5)).is_none());
        let report = builder
            .build(t0 + Duration::from_millis(21))
            .expect("report");
        assert_eq!(report.sample_count, 1);
        assert_eq!(report.highest_frame_id, 1);
    }

    #[test]
    fn feedback_marks_parity_samples() {
        let t0 = Instant::now();
        let mut builder = FeedbackBuilder::new(t0);
        // A data shard and a parity shard of the same block share
        // `(frame_id, chunk_idx)`; the parity bit must survive the report.
        builder.on_video_datagram(4, 0, 0, t0, 1000, false);
        builder.on_video_datagram(4, 0, 0, t0, 1100, true);
        let report = builder
            .build(t0 + Duration::from_millis(21))
            .expect("report");
        assert_eq!(report.sample_count, 2);
        assert_eq!(report.samples[0].flags & ARRIVAL_PARITY, 0);
        assert_ne!(report.samples[1].flags & ARRIVAL_PARITY, 0);
    }

    #[test]
    fn send_history_distinguishes_parity_from_data_shards() {
        let t0 = Instant::now();
        let mut history = SendHistory::new();
        history.record(7, 3, false, 1000, t0);
        history.record(7, 3, true, 1200, t0 + Duration::from_millis(2));

        let (data_bytes, data_sent) = history.lookup(7, 3, false).expect("data");
        let (parity_bytes, parity_sent) = history.lookup(7, 3, true).expect("parity");
        assert_eq!(data_bytes, 1000);
        assert_eq!(parity_bytes, 1200);
        assert_ne!(data_sent, parity_sent);
    }

    #[test]
    fn growing_queue_delay_is_detected_before_loss() {
        let origin = Instant::now();
        let mut controller = GccController::new(20_000_000, 80_000_000);
        for i in 0..8u32 {
            controller.record_sent(
                1,
                i as u16,
                false,
                1180,
                origin + Duration::from_micros(u64::from(i) * 2_000),
            );
        }

        let mut seq = 0;
        let mut target = controller.current_bitrate_bps();
        // Each packet is sent 2 ms later, but arrives 3.5 ms later: 1.5 ms of
        // queue growth per packet, no loss at all.
        for i in 0..8u64 {
            let packet = packet_with_samples(&[(1, i as u16, 10_000 + i * 3_500)], i * 20_000, seq);
            seq += 1;
            let decision =
                controller.on_feedback_at(&packet, origin + Duration::from_millis(20 + i * 20));
            target = decision.target_bitrate_bps;
        }
        assert!(
            target < 20_000_000,
            "target should decrease on growing delay, got {target}"
        );
    }

    #[test]
    fn idle_feedback_stops_queue_reduction() {
        let origin = Instant::now();
        let mut controller = GccController::new(20_000_000, 80_000_000);
        for i in 0..8u32 {
            controller.record_sent(
                1,
                i as u16,
                false,
                1180,
                origin + Duration::from_micros(u64::from(i) * 2_000),
            );
        }

        let mut seq = 0u32;
        // Send cadence is 2 ms, arrival cadence 3.5 ms: the queue grows
        // without any loss and drives the trendline into Overusing.
        for i in 0..8u64 {
            let packet = packet_with_samples(&[(1, i as u16, 10_000 + i * 3_500)], i * 20_000, seq);
            seq += 1;
            controller.on_feedback_at(&packet, origin + Duration::from_millis(20 + i * 20));
        }
        let bottom = controller.current_bitrate_bps();
        assert!(
            bottom < 20_000_000,
            "overuse should reduce first, got {bottom}"
        );

        // Screen went idle: reports keep arriving every 16 ms but carry no
        // shard samples. After the post-congestion cooldown the controller
        // must stop cutting and hold or ramp.
        let mut previous = bottom;
        for i in 0..10u64 {
            let packet = packet_with_samples(&[], 200_000 + i * 20_000, seq);
            seq += 1;
            let decision = controller.on_feedback_at(
                &packet,
                origin + Duration::from_millis(500 + i * 20),
            );
            assert!(
                decision.target_bitrate_bps >= previous,
                "idle report {i} cut bitrate from {previous} to {}",
                decision.target_bitrate_bps
            );
            previous = decision.target_bitrate_bps;
        }
    }

    #[test]
    fn clean_path_ramps_up() {
        let origin = Instant::now();
        let mut controller = GccController::new(5_000_000, 80_000_000);
        for i in 0..12u32 {
            controller.record_sent(
                1,
                i as u16,
                false,
                1180,
                origin + Duration::from_micros(u64::from(i) * 2_000),
            );
        }
        let mut seq = 0;
        let mut previous = controller.current_bitrate_bps();
        for i in 0..12u64 {
            let packet = packet_with_samples(&[(1, i as u16, 20_000 + i * 2_000)], i * 20_000, seq);
            seq += 1;
            let decision =
                controller.on_feedback_at(&packet, origin + Duration::from_millis(20 + i * 20));
            assert!(decision.target_bitrate_bps >= previous);
            previous = decision.target_bitrate_bps;
        }
        assert!(previous > 5_000_000);
    }

    #[test]
    fn report_loss_is_measured_by_the_host() {
        let mut controller = GccController::new(10_000_000, 80_000_000);
        let now = Instant::now();
        for seq in [0u32, 1, 2, 5, 6] {
            let packet = FeedbackPacket::new(seq, 0);
            controller.on_feedback_at(&packet, now);
        }
        assert!(controller.ack_loss_permille() > 0);
    }
}
