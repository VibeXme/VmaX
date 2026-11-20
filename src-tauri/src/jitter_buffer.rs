//! Client-side adaptive micro-jitter buffer.
//!
//! The network thread is the single producer; the MediaCodec feed thread is the
//! single consumer.  Frames cross the threads through a bounded, lock-free
//! SPSC ring (Vyukov sequence counters).  Jitter statistics and the hold delay
//! are off the hot path: the producer owns the estimator, the consumer reads a
//! single atomic hold value.
//!
//! Design target:
//! * 1 ms hold on a clean LAN (one display frame is never "buffered").
//! * Up to 15 ms hold on a jittery 4G/5G path.
//! * If a newer complete frame is available, the older one is dropped
//!   immediately.  A deep queue is always worse than a dropped stale frame.

use bytes::Bytes;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const MIN_HOLD: Duration = Duration::from_millis(2);
pub const MAX_HOLD: Duration = Duration::from_millis(14);
pub const JITTER_WINDOW: usize = 100;
/// If a held frame is this far past its due time, emit it rather than freeze.
pub const MAX_LATE_GRACE: Duration = Duration::from_millis(33);

/// One reassembled access unit waiting for the decoder.
#[derive(Debug, Clone)]
pub struct BufferedFrame {
    pub frame_id: u32,
    pub timestamp_us: u64,
    pub codec: u8,
    pub is_keyframe: bool,
    pub arrival: Instant,
    pub payload: Bytes,
}

#[repr(align(64))]
struct PaddedAtomicU64(AtomicU64);

impl PaddedAtomicU64 {
    fn new(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }

    #[inline]
    fn load(&self, order: Ordering) -> u64 {
        self.0.load(order)
    }

    #[inline]
    fn claim(&self, value: u64) -> bool {
        self.0
            .compare_exchange(
                value,
                value.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

struct Slot<T> {
    sequence: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}

// The sequence protocol guarantees that only the producer writes an empty slot
// and only the consumer reads a published slot.
unsafe impl<T: Send> Sync for Slot<T> {}

/// Bounded lock-free single-producer/single-consumer ring.
pub struct SpscRing<T> {
    slots: Box<[Slot<T>]>,
    mask: usize,
    head: PaddedAtomicU64,
    tail: PaddedAtomicU64,
}

impl<T> SpscRing<T> {
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let slots = (0..capacity)
            .map(|index| Slot {
                sequence: AtomicU64::new(index as u64),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            mask: capacity - 1,
            head: PaddedAtomicU64::new(0),
            tail: PaddedAtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn len(&self) -> usize {
        self.tail
            .load(Ordering::Relaxed)
            .wrapping_sub(self.head.load(Ordering::Relaxed)) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Producer side.  Returns the value back when the ring is full.
    pub fn push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let slot = &self.slots[(tail as usize) & self.mask];
        if slot.sequence.load(Ordering::Acquire) != tail || !self.tail.claim(tail) {
            return Err(value);
        }
        // SAFETY: the empty slot was claimed exclusively above.
        unsafe {
            (*slot.value.get()).write(value);
        }
        slot.sequence.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Consumer side.
    pub fn pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let slot = &self.slots[(head as usize) & self.mask];
        if slot.sequence.load(Ordering::Acquire) != head.wrapping_add(1) || !self.head.claim(head) {
            return None;
        }
        // SAFETY: the producer published this slot and will not touch it until
        // the consumer advances the sequence below.
        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.sequence
            .store(head.wrapping_add(self.mask as u64 + 1), Ordering::Release);
        Some(value)
    }
}

impl<T> Drop for SpscRing<T> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

#[derive(Debug, Default)]
struct JitterStats {
    pushed: AtomicU64,
    emitted: AtomicU64,
    dropped_stale: AtomicU64,
    dropped_full: AtomicU64,
    forced_late: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStatsSnapshot {
    pub pushed: u64,
    pub emitted: u64,
    pub dropped_stale: u64,
    pub dropped_full: u64,
    pub forced_late: u64,
    pub p95_jitter_us: u32,
    pub hold_us: u32,
    pub queue_depth: u32,
}

/// Sliding 100-frame 95th-percentile jitter estimator.
struct JitterEstimator {
    window: [u32; JITTER_WINDOW],
    index: usize,
    filled: usize,
    last_arrival_us: Option<u64>,
    last_capture_us: Option<u64>,
}

impl JitterEstimator {
    fn new() -> Self {
        Self {
            window: [0; JITTER_WINDOW],
            index: 0,
            filled: 0,
            last_arrival_us: None,
            last_capture_us: None,
        }
    }

    fn observe(&mut self, capture_ts_us: u64, arrival_us: u64) {
        if let (Some(prev_arrival), Some(prev_capture)) =
            (self.last_arrival_us, self.last_capture_us)
        {
            let recv_delta = arrival_us.saturating_sub(prev_arrival) as i64;
            let send_delta = capture_ts_us.saturating_sub(prev_capture) as i64;
            let sample = (recv_delta - send_delta)
                .unsigned_abs()
                .min(u64::from(u32::MAX)) as u32;
            self.window[self.index] = sample;
            self.index = (self.index + 1) % JITTER_WINDOW;
            self.filled = (self.filled + 1).min(JITTER_WINDOW);
        }
        self.last_arrival_us = Some(arrival_us);
        self.last_capture_us = Some(capture_ts_us);
    }

    fn p95_us(&self) -> u32 {
        if self.filled == 0 {
            return 0;
        }
        let mut copy = [0u32; JITTER_WINDOW];
        copy[..self.filled].copy_from_slice(&self.window[..self.filled]);
        let samples = &mut copy[..self.filled];
        samples.sort_unstable();
        let index = ((self.filled * 95).div_ceil(100)).saturating_sub(1);
        samples[index.min(self.filled - 1)]
    }

    fn target_hold_us(&self) -> u32 {
        // 1 ms base + 1.4x the p95 jitter, capped at 15 ms.  The multiplier
        // leaves room for one retransmission/reordering interval without
        // building a full display frame of delay.
        let p95 = self.p95_us() as f32;
        ((MIN_HOLD.as_micros() as f32 + p95 * 1.4) as u32)
            .clamp(MIN_HOLD.as_micros() as u32, MAX_HOLD.as_micros() as u32)
    }
}

struct QueuedFrame {
    epoch: u64,
    frame: BufferedFrame,
}

type FrameQueue = Arc<Mutex<VecDeque<QueuedFrame>>>;

pub struct JitterProducer {
    queue: FrameQueue,
    capacity: usize,
    epoch: Arc<AtomicU64>,
    estimator: JitterEstimator,
    hold_us: Arc<AtomicU32>,
    stats: Arc<JitterStats>,
    origin: Instant,
    current_hold_us: u32,
    last_pushed: Option<u32>,
}

pub struct JitterConsumer {
    queue: FrameQueue,
    epoch: Arc<AtomicU64>,
    hold_us: Arc<AtomicU32>,
    stats: Arc<JitterStats>,
    observed_epoch: u64,
    needs_keyframe: bool,
    awaiting_keyframe: bool,
    last_emitted: Option<u32>,
}

impl JitterProducer {
    pub fn new(origin: Instant, capacity: usize) -> (Self, JitterConsumer) {
        let capacity = capacity.max(1);
        let queue = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
        let hold_us = Arc::new(AtomicU32::new(MIN_HOLD.as_micros() as u32));
        let stats = Arc::new(JitterStats::default());
        let epoch = Arc::new(AtomicU64::new(0));
        let producer = Self {
            queue: Arc::clone(&queue),
            capacity,
            epoch: Arc::clone(&epoch),
            estimator: JitterEstimator::new(),
            hold_us: Arc::clone(&hold_us),
            stats: Arc::clone(&stats),
            origin,
            current_hold_us: MIN_HOLD.as_micros() as u32,
            last_pushed: None,
        };
        let consumer = JitterConsumer {
            queue,
            epoch,
            hold_us,
            stats,
            observed_epoch: 0,
            needs_keyframe: false,
            awaiting_keyframe: true,
            last_emitted: None,
        };
        (producer, consumer)
    }

    /// Called on the network thread immediately after reassembly.  Returns
    /// `false` when an out-of-order frame is rejected.
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
        self.current_hold_us = if target > self.current_hold_us {
            target
        } else {
            self.current_hold_us.saturating_sub(100).max(target)
        };
        self.hold_us.store(self.current_hold_us, Ordering::Relaxed);

        let epoch = self.epoch.load(Ordering::Relaxed);
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        if queue.len() == self.capacity {
            queue.pop_front();
            self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(QueuedFrame { epoch, frame });
        self.stats.pushed.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn superseded_frames(&self) -> u64 {
        self.stats.dropped_full.load(Ordering::Relaxed)
    }

    /// Clears the monotonic frame-id guard and the jitter estimate after a
    /// stream discontinuity (reconnect, codec switch, server restart).  Frame
    /// ids restart from a low value on a new stream; keeping the old `last`
    /// value would silently discard every frame of the new stream.
    pub fn reset_stream(&mut self) {
        self.epoch.fetch_add(1, Ordering::Release);
        self.estimator = JitterEstimator::new();
        self.current_hold_us = MIN_HOLD.as_micros() as u32;
        self.hold_us.store(self.current_hold_us, Ordering::Relaxed);
        self.last_pushed = None;
    }

    pub fn hold_delay(&self) -> Duration {
        Duration::from_micros(u64::from(self.current_hold_us))
    }

    pub fn p95_jitter_us(&self) -> u32 {
        self.estimator.p95_us()
    }

    pub fn stats(&self) -> JitterStatsSnapshot {
        JitterStatsSnapshot {
            pushed: self.stats.pushed.load(Ordering::Relaxed),
            emitted: self.stats.emitted.load(Ordering::Relaxed),
            dropped_stale: self.stats.dropped_stale.load(Ordering::Relaxed),
            dropped_full: self.stats.dropped_full.load(Ordering::Relaxed),
            forced_late: self.stats.forced_late.load(Ordering::Relaxed),
            p95_jitter_us: self.estimator.p95_us(),
            hold_us: self.current_hold_us,
            queue_depth: self.queue.lock().unwrap_or_else(|error| error.into_inner()).len() as u32,
        }
    }
}

impl JitterConsumer {
    /// Preserve frame dependencies. When a frame is missing, resume only
    /// after receiving a new keyframe.
    pub fn next(&mut self, now: Instant) -> Option<BufferedFrame> {
        loop {
            let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
            let queued = queue.front()?;
            let epoch = self.epoch.load(Ordering::Acquire);
            if epoch != self.observed_epoch {
                self.observed_epoch = epoch;
                self.last_emitted = None;
                self.awaiting_keyframe = true;
                self.needs_keyframe = false;
            }
            if queued.epoch != epoch {
                queue.pop_front();
                continue;
            }
            let frame = &queued.frame;
            let gap = self
                .last_emitted
                .is_some_and(|last| last.wrapping_add(1) != frame.frame_id);
            if !frame.is_keyframe && (gap || self.awaiting_keyframe) {
                if !self.awaiting_keyframe || self.last_emitted.is_none() {
                    self.needs_keyframe = true;
                }
                self.awaiting_keyframe = true;
                self.stats.dropped_stale.fetch_add(1, Ordering::Relaxed);
                queue.pop_front();
                continue;
            }
            let hold = Duration::from_micros(u64::from(self.hold_us.load(Ordering::Relaxed)));
            if now < frame.arrival + hold {
                return None;
            }
            if now.saturating_duration_since(frame.arrival) >= hold + MAX_LATE_GRACE {
                self.stats.forced_late.fetch_add(1, Ordering::Relaxed);
            }
            self.last_emitted = Some(frame.frame_id);
            self.awaiting_keyframe = false;
            self.needs_keyframe = false;
            self.stats.emitted.fetch_add(1, Ordering::Relaxed);
            return queue.pop_front().map(|queued| queued.frame);
        }
    }

    /// Resume from a keyframe after the downstream decoder rejects a frame.
    pub fn decoder_rejected_frame(&mut self) {
        self.awaiting_keyframe = true;
        self.needs_keyframe = true;
    }

    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.needs_keyframe)
    }

    pub fn hold_delay(&self) -> Duration {
        Duration::from_micros(u64::from(self.hold_us.load(Ordering::Relaxed)))
    }

    /// Instant at which the pending frame becomes due.  The decoder thread
    /// sleeps exactly this long instead of polling every millisecond.
    pub fn pending_due(&self) -> Option<Instant> {
        let queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        let frame = queue.front()?;
        let hold_us = u64::from(self.hold_us.load(Ordering::Relaxed));
        Some(frame.frame.arrival + Duration::from_micros(hold_us))
    }

    pub fn has_pending(&self) -> bool {
        !self.queue.lock().unwrap_or_else(|error| error.into_inner()).is_empty()
    }

    /// Stream discontinuity (codec switch, reconnect).  Drop everything so a
    /// stale IDR does not hold the new stream hostage.
    pub fn reset(&mut self) {
        self.awaiting_keyframe = true;
        self.needs_keyframe = false;
        self.last_emitted = None;
        self.queue.lock().unwrap_or_else(|error| error.into_inner()).clear();
    }

    pub fn stats(&self) -> JitterStatsSnapshot {
        JitterStatsSnapshot {
            pushed: self.stats.pushed.load(Ordering::Relaxed),
            emitted: self.stats.emitted.load(Ordering::Relaxed),
            dropped_stale: self.stats.dropped_stale.load(Ordering::Relaxed),
            dropped_full: self.stats.dropped_full.load(Ordering::Relaxed),
            forced_late: self.stats.forced_late.load(Ordering::Relaxed),
            p95_jitter_us: 0,
            hold_us: self.hold_us.load(Ordering::Relaxed),
            queue_depth: self.queue.lock().unwrap_or_else(|error| error.into_inner()).len() as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, arrival: Instant, key: bool) -> BufferedFrame {
        BufferedFrame {
            frame_id: id,
            timestamp_us: u64::from(id) * 16_667,
            codec: 1,
            is_keyframe: key,
            arrival,
            payload: Bytes::from_static(b"nal"),
        }
    }

    #[test]
    fn spsc_ring_is_fifo_and_bounded() {
        let ring = SpscRing::with_capacity(4);
        for i in 0..4 {
            assert!(ring.push(i).is_ok());
        }
        assert_eq!(ring.push(99), Err(99));
        for i in 0..4 {
            assert_eq!(ring.pop(), Some(i));
        }
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn single_slot_replaces_stale_video_and_resumes_on_keyframe() {
        let now = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(now, 1);
        assert!(producer.push(frame(1, now, true)));
        assert_eq!(consumer.next(now + MAX_HOLD).unwrap().frame_id, 1);
        assert!(producer.push(frame(2, now, false)));
        assert!(producer.push(frame(3, now, false)));
        assert_eq!(producer.superseded_frames(), 1);
        assert!(consumer.next(now + MAX_HOLD).is_none());
        assert!(consumer.take_keyframe_request());
        assert!(producer.push(frame(4, now, true)));
        assert_eq!(consumer.next(now + MAX_HOLD).unwrap().frame_id, 4);
    }

    #[test]
    fn queued_delta_frames_keep_their_references_and_recover_after_gap() {
        let t0 = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(t0, 8);
        for (id, key) in [(1, true), (2, false), (3, false), (5, false)] {
            assert!(producer.push(frame(id, t0, key)));
        }
        let due = t0 + Duration::from_millis(20);
        for id in 1..=3 {
            assert_eq!(consumer.next(due).map(|frame| frame.frame_id), Some(id));
        }
        assert!(consumer.next(due).is_none());
        assert!(consumer.take_keyframe_request());
        assert!(!consumer.take_keyframe_request());
        assert!(producer.push(frame(6, t0, true)));
        assert_eq!(consumer.next(due).map(|frame| frame.frame_id), Some(6));
    }

    #[test]
    fn decoder_error_requests_a_keyframe_and_drops_dependent_frames() {
        let t0 = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(t0, 8);
        for (id, key) in [(1, true), (2, false), (3, true), (4, false)] {
            assert!(producer.push(frame(id, t0, key)));
        }
        let due = t0 + Duration::from_millis(20);
        assert_eq!(consumer.next(due).unwrap().frame_id, 1);
        consumer.decoder_rejected_frame();
        assert!(consumer.take_keyframe_request());
        assert_eq!(consumer.next(due).unwrap().frame_id, 3);
        assert_eq!(consumer.next(due).unwrap().frame_id, 4);
    }

    #[test]
    fn pending_keyframe_is_delivered_before_its_delta() {
        let now = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(now, 8);
        assert!(producer.push(frame(1, now, true)));
        assert!(producer.push(frame(2, now + Duration::from_millis(1), false)));
        assert_eq!(
            consumer
                .next(now + Duration::from_millis(20))
                .map(|f| f.frame_id),
            Some(1)
        );
        assert_eq!(
            consumer
                .next(now + Duration::from_millis(20))
                .map(|f| f.frame_id),
            Some(2)
        );
    }

    #[test]
    fn hold_delay_stays_in_the_1_to_15ms_window() {
        let t0 = Instant::now();
        let (mut producer, _consumer) = JitterProducer::new(t0, 8);
        for i in 0..120u32 {
            let jitter = if i % 2 == 0 { 0 } else { 20_000 };
            producer
                .estimator
                .observe(u64::from(i) * 16_667, u64::from(i) * 16_667 + jitter);
        }
        let target = producer.estimator.target_hold_us();
        assert!(target >= MIN_HOLD.as_micros() as u32);
        assert!(target <= MAX_HOLD.as_micros() as u32);
        assert!(producer.estimator.p95_us() >= 15_000);
    }

    #[test]
    fn clean_lan_holds_only_one_millisecond() {
        let (producer, _consumer) = JitterProducer::new(Instant::now(), 8);
        assert_eq!(
            producer.estimator.target_hold_us(),
            MIN_HOLD.as_micros() as u32
        );
    }

    #[test]
    fn consumer_does_not_emit_before_the_hold_deadline() {
        let t0 = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(t0, 8);
        assert!(producer.push(frame(1, t0, true)));
        assert!(consumer.next(t0 + Duration::from_micros(100)).is_none());
        assert!(consumer.next(t0 + Duration::from_millis(2)).is_some());
    }

    #[test]
    fn forced_late_emission_prevents_a_frozen_decoder() {
        let t0 = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(t0, 8);
        assert!(producer.push(frame(1, t0, true)));
        let out = consumer.next(t0 + Duration::from_millis(100));
        assert!(out.is_some());
        assert_eq!(consumer.stats().forced_late, 1);
    }

    #[test]
    fn reconnect_accepts_restarted_frame_ids() {
        let t0 = Instant::now();
        let (mut producer, mut consumer) = JitterProducer::new(t0, 8);
        assert!(producer.push(frame(900, t0, true)));
        assert_eq!(
            consumer
                .next(t0 + Duration::from_millis(20))
                .map(|out| out.frame_id),
            Some(900)
        );

        // The stream restarts at frame 1.  The producer rejects ordinary
        // reordering, so the consumer must treat this as a new epoch instead
        // of pinning itself to the old high-water mark.
        assert!(producer.push(frame(901, t0, true)));
        producer.reset_stream();
        assert!(producer.push(frame(1, t0 + Duration::from_millis(30), true)));
        assert_eq!(
            consumer
                .next(t0 + Duration::from_millis(50))
                .map(|out| out.frame_id),
            Some(1)
        );
    }
}
