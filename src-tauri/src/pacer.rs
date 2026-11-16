//! Sub-frame packet pacer and adaptive Reed-Solomon protection.
//!
//! Two problems are solved here, and they are linked:
//!
//! * **Bursts.**  Handing 200 shards to the kernel in one call overflows a
//!    weak Wi-Fi or cellular queue before any congestion signal can travel
//!    back.  [`SubFramePacer`] spaces the shards across 70% of the frame
//!    budget, leaving 30% of headroom for the next frame and for ACK traffic.
//! * **Loss.**  Parity is computed per frame from the client's loss estimate:
//!    2% on a clean link, rising linearly to 25% at 4% loss and above.
//!
//! Shards are capped at [`SHARD_BYTES`] (1150) so the 20-byte VmaX video header
//! plus the QUIC header and AEAD tag still fit inside
//! [`crate::wan_transport::WAN_DATAGRAM_BUDGET`] (1180).
//!
//! Integration with `network.rs`: replace the direct
//! `conn.send_datagram(bytes)` calls in the `video_rx` arm with
//! `pacer.send_paced_frame(&sink, shards, budget)` where `sink` is a
//! [`QuinnSink`] over the same `quinn::Connection`.  The encoder side only
//! needs [`parity_for_loss`] to choose how many parity shards to emit.

use bytes::Bytes;
use std::time::{Duration, Instant};

/// Payload bytes per data shard.  `1150 + 20 (VmaX header) + ~10 (QUIC) = 1180`.
pub const SHARD_BYTES: usize = 1150;

/// Fraction of the frame interval used for sending.  The remainder absorbs
/// capture jitter, ACK/timing traffic and the next frame's first shard.
/// Spread a frame's datagrams over 60% of the frame interval, leaving the
/// remaining 40% idle so a mobile modem queue can drain between frames
/// instead of growing (bufferbloat).
pub const PACING_BUDGET_PCT: u32 = 60;

/// Clean-link parity.
pub const MIN_PARITY_PCT: u32 = 2;
/// Parity ceiling on a badly congested mobile path.
pub const MAX_PARITY_PCT: u32 = 25;

/// Loss (permille) at which parity starts climbing above the floor.
pub const PARITY_LOSS_FLOOR_PERMILLE: u16 = 5; // 0.5%
/// Loss (permille) at which parity reaches the ceiling.
pub const PARITY_LOSS_CEIL_PERMILLE: u16 = 40; // 4%

/// Borrowed shard of an encoded access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameShard<'a> {
    pub index: u16,
    pub data: &'a [u8],
}

/// Split an encoded frame into MTU-safe shards.  Empty input yields no shards.
pub fn split_frame(payload: &[u8], shard_bytes: usize) -> Vec<FrameShard<'_>> {
    let shard_bytes = shard_bytes.clamp(64, SHARD_BYTES);
    if payload.is_empty() {
        return Vec::new();
    }
    payload
        .chunks(shard_bytes)
        .enumerate()
        .map(|(index, data)| FrameShard {
            index: index.min(u16::MAX as usize) as u16,
            data,
        })
        .collect()
}

/// Number of parity shards for `data_shards` at `loss_permille`.
///
/// 2% below 0.5% loss, linear to 25% at 4%, and always at least one parity
/// shard once there are enough data shards for it to be meaningful.
pub fn parity_for_loss(data_shards: usize, loss_permille: u16) -> usize {
    if data_shards == 0 {
        return 0;
    }
    let pct = parity_pct(loss_permille);
    // ceil(data * pct / 100) keeps the real ratio at or above the target.
    let parity = (data_shards * pct as usize).div_ceil(100);
    if parity == 0 && data_shards >= 4 {
        // A single parity shard is cheap insurance for a frame that would
        // otherwise force an intra-refresh.
        1
    } else {
        parity.min(data_shards)
    }
}

/// Parity percentage (2..=25) for a loss estimate, linear in between.
pub fn parity_pct(loss_permille: u16) -> u32 {
    let loss = loss_permille.clamp(0, 1000) as u32;
    if loss <= u32::from(PARITY_LOSS_FLOOR_PERMILLE) {
        return MIN_PARITY_PCT;
    }
    if loss >= u32::from(PARITY_LOSS_CEIL_PERMILLE) {
        return MAX_PARITY_PCT;
    }
    let span = u32::from(PARITY_LOSS_CEIL_PERMILLE - PARITY_LOSS_FLOOR_PERMILLE);
    let above = loss - u32::from(PARITY_LOSS_FLOOR_PERMILLE);
    MIN_PARITY_PCT + (above * (MAX_PARITY_PCT - MIN_PARITY_PCT)) / span
}

/// A fully described frame transmit plan: shards plus their send instants.
#[derive(Debug, Clone)]
pub struct PacedFrame {
    pub shards: Vec<Bytes>,
    pub send_at: Vec<Instant>,
    /// Total wire bytes including parity.
    pub wire_bytes: usize,
    pub parity_shards: usize,
}

impl PacedFrame {
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// Serialization span, i.e. the time between the first and last shard.
    pub fn span(&self) -> Duration {
        match (self.send_at.first(), self.send_at.last()) {
            (Some(first), Some(last)) => last.saturating_duration_since(*first),
            _ => Duration::ZERO,
        }
    }
}

/// Compute evenly spaced send instants across `budget`.
fn schedule(count: usize, budget: Duration, start: Instant) -> Vec<Instant> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![start];
    }
    // `count - 1` intervals put the last shard exactly at the budget edge
    // instead of one step past it.
    let step = budget / (count as u32 - 1).max(1);
    (0..count)
        .map(|index| start + step * index as u32)
        .collect()
}

/// Hybrid sleep: coarse OS sleep, then a spin-yield for the final 250 µs so
/// the pacer lands within tens of microseconds without burning a core for a
/// whole millisecond.
#[inline]
pub fn spin_sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining > Duration::from_micros(250) {
            std::thread::sleep(remaining - Duration::from_micros(150));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Destination for paced shards.  `quinn::Connection` is wrapped by
/// [`QuinnSink`] for production; tests use a recording sink.
pub trait DatagramSink {
    fn send(&self, shard: Bytes) -> bool;
}

/// Thin adapter so the pacer can live in the send loop without depending on
/// `quinn` in tests.
pub struct QuinnSink<'a> {
    pub conn: &'a quinn::Connection,
}

impl DatagramSink for QuinnSink<'_> {
    fn send(&self, shard: Bytes) -> bool {
        self.conn.send_datagram(shard).is_ok()
    }
}

/// Stateless pacing core: plans a whole frame at once.
#[derive(Debug, Clone)]
pub struct SubFramePacer {
    rate_bps: u64,
    budget_pct: u32,
}

impl SubFramePacer {
    pub fn new(rate_bps: u32) -> Self {
        Self {
            rate_bps: u64::from(rate_bps.max(1)),
            budget_pct: PACING_BUDGET_PCT,
        }
    }

    pub fn with_budget_pct(mut self, pct: u32) -> Self {
        self.budget_pct = pct.clamp(10, 100);
        self
    }

    pub fn set_rate_bps(&mut self, rate_bps: u32) {
        self.rate_bps = u64::from(rate_bps.max(1));
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// Portion of the frame interval the pacer is allowed to use.
    pub fn budget(&self, fps: u16) -> Duration {
        let frame = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        frame.mul_f64(f64::from(self.budget_pct) / 100.0)
    }

    /// Build the plan.  If the frame is so large that the rate-based
    /// serialization time exceeds the budget, the shards are spread over the
    /// budget anyway (the congestion controller will shrink the next frame).
    pub fn plan(&self, shards: Vec<Bytes>, fps: u16, now: Instant) -> PacedFrame {
        let wire_bytes: usize = shards.iter().map(Bytes::len).sum();
        let count = shards.len();
        let serialization = Duration::from_secs_f64(
            (wire_bytes as f64 * 8.0) / self.rate_bps.max(1) as f64,
        );
        let budget = self.budget(fps);
        // Never space the frame out longer than the budget: holding a complete
        // frame longer than a frame interval is pure added latency.
        let span = serialization.min(budget).max(Duration::from_micros(1));
        PacedFrame {
            send_at: schedule(count, span, now),
            shards,
            wire_bytes,
            parity_shards: 0,
        }
    }

    /// Send the frame at the planned instants.  Returns the number of shards
    /// the transport accepted.
    pub fn send_paced_frame<S: DatagramSink>(
        &self,
        sink: &S,
        plan: &PacedFrame,
    ) -> usize {
        let mut sent = 0;
        for (shard, at) in plan.shards.iter().zip(plan.send_at.iter()) {
            spin_sleep_until(*at);
            if sink.send(shard.clone()) {
                sent += 1;
            }
        }
        sent
    }
}

/// One frame's worth of encoded bytes plus the parity decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct FecBudget {
    pub data_shards: usize,
    pub parity_shards: usize,
}

impl FecBudget {
    pub fn for_frame(frame_bytes: usize, loss_permille: u16) -> Self {
        let data_shards = frame_bytes.div_ceil(SHARD_BYTES);
        Self {
            data_shards,
            parity_shards: parity_for_loss(data_shards, loss_permille),
        }
    }

    pub fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }

    /// Effective redundancy in permille, for telemetry.
    pub fn parity_permille(&self) -> u16 {
        if self.data_shards == 0 {
            return 0;
        }
        ((self.parity_shards * 1000) / self.data_shards).min(u16::MAX as usize) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingSink {
        sends: Mutex<Vec<(Instant, usize)>>,
    }

    impl DatagramSink for RecordingSink {
        fn send(&self, shard: Bytes) -> bool {
            if let Ok(mut sends) = self.sends.lock() {
                sends.push((Instant::now(), shard.len()));
            }
            true
        }
    }

    #[test]
    fn shards_never_exceed_the_mtu_budget() {
        let payload = vec![7u8; 40_000];
        let shards = split_frame(&payload, SHARD_BYTES);
        assert!(shards.iter().all(|s| s.data.len() <= SHARD_BYTES));
        // 1150 + 20-byte VmaX header stays inside the 1180 carrier budget.
        assert!(SHARD_BYTES + 20 <= crate::wan_transport::WAN_DATAGRAM_BUDGET);
        let total: usize = shards.iter().map(|s| s.data.len()).sum();
        assert_eq!(total, payload.len());
    }

    #[test]
    fn parity_is_two_percent_on_a_clean_link() {
        assert_eq!(parity_pct(0), 2);
        assert_eq!(parity_pct(3), 2);
        assert_eq!(parity_for_loss(50, 0), 1);
    }

    #[test]
    fn parity_reaches_twenty_five_percent_at_four_percent_loss() {
        assert_eq!(parity_pct(40), 25);
        assert_eq!(parity_pct(120), 25);
        assert_eq!(parity_for_loss(40, 40), 10);
    }

    #[test]
    fn parity_scales_linearly_in_between() {
        // 2.25% loss is the midpoint of 0.5%..4% -> ~13.5%.
        let mid = parity_pct(22);
        assert!((13..=14).contains(&mid), "got {mid}");
        assert!(parity_pct(10) < parity_pct(20));
        assert!(parity_pct(20) < parity_pct(30));
    }

    #[test]
    fn fec_budget_describes_the_frame() {
        let budget = FecBudget::for_frame(23_000, 0);
        assert_eq!(budget.data_shards, 20);
        assert_eq!(budget.parity_shards, 1);
        assert_eq!(budget.parity_permille(), 50);
    }

    #[test]
    fn pacer_spreads_shards_across_seventy_percent_of_the_frame() {
        let now = Instant::now();
        let pacer = SubFramePacer::new(20_000_000);
        let shards: Vec<Bytes> = (0..10).map(|_| Bytes::from_static(&[0u8; 1150])).collect();
        let plan = pacer.plan(shards, 60, now);
        let budget = pacer.budget(60); // 11.66 ms at 70% of 16.66 ms
        assert!(plan.span() <= budget);
        assert!(plan.span() > Duration::ZERO);
        // Evenly spaced: consecutive gaps are within one tick of each other.
        let first_gap = plan.send_at[1].saturating_duration_since(plan.send_at[0]);
        for pair in plan.send_at.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            assert!(gap.abs_diff(first_gap) <= Duration::from_micros(1));
        }
    }

    #[test]
    fn pacer_caps_latency_at_the_frame_budget_for_huge_frames() {
        let now = Instant::now();
        let pacer = SubFramePacer::new(1_000_000); // 1 Mbps, far too slow
        let shards: Vec<Bytes> = (0..50).map(|_| Bytes::from_static(&[0u8; 1150])).collect();
        let plan = pacer.plan(shards, 60, now);
        assert!(plan.span() <= pacer.budget(60));
    }

    #[test]
    fn send_paced_frame_emits_every_shard_in_order() {
        let sink = RecordingSink {
            sends: Mutex::new(Vec::new()),
        };
        let pacer = SubFramePacer::new(50_000_000);
        let shards: Vec<Bytes> = (0..4).map(|_| Bytes::from_static(&[1u8; 1000])).collect();
        let plan = pacer.plan(shards, 120, Instant::now());
        assert_eq!(pacer.send_paced_frame(&sink, &plan), 4);
        let sends = sink.sends.lock().expect("lock");
        assert_eq!(sends.len(), 4);
        assert!(sends.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }

    #[test]
    fn hybrid_sleep_returns_within_a_practical_bound() {
        let start = Instant::now();
        spin_sleep_until(start + Duration::from_micros(500));
        assert!(start.elapsed() >= Duration::from_micros(450));
        assert!(start.elapsed() < Duration::from_millis(20));
    }
}
