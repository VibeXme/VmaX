//! Micro-paced datagram scheduler, safe-MTU sharding and adaptive FEC.
//!
//! The important property is that a video frame is never handed to the kernel
//! as one large burst.  `PacedSender` owns a dedicated OS thread (never a Tokio
//! worker), spaces shards with a token-bucket schedule, and sleeps the last
//! few hundred microseconds with a spin loop.  That keeps weak Wi-Fi and
//! cellular modem queues from filling up before the congestion controller has
//! a chance to react.

use bytes::Bytes;
use quinn::Connection;
use std::collections::VecDeque;
use std::io;
use std::net::UdpSocket as StdUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Hard application-level datagram cap.  QUIC adds its own header and AEAD
/// tag; 1170 bytes keeps the final UDP payload below the smallest common
/// mobile-carrier MTU (1280) even with IPv6 extension headers.
pub const MAX_APP_DATAGRAM_BYTES: usize = 1170;
/// Keep QUIC's application datagram queue small enough that congestion is
/// visible to the capture loop before it turns into user-visible input/video
/// latency. 512 KiB is still large enough for a keyframe burst, but at 60 Mbps
/// it represents ~70 ms instead of >500 ms of hidden queueing.
pub const QUIC_DATAGRAM_BUFFER_BYTES: usize = 512 * 1024;

pub fn paced_wire_rate(video_bps: u32, parity_permille: u16) -> u32 {
    // Include parity and transport headers so a full encoder frame does not
    // consume more than one frame interval on a healthy link.
    (u64::from(video_bps) * (1060 + u64::from(parity_permille)) / 1000).min(u64::from(u32::MAX))
        as u32
}

/// One frame at 60 FPS is 16.67 ms; used to derive the pacing budget.
#[inline]
pub fn frame_budget(fps: u16) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))
}

/// Spacing between the shards of one frame.
///
/// `spread` is the budget a frame is allowed to occupy, not the time it needs.
/// A small access unit on a fast link serialises in a fraction of a
/// millisecond, and stretching it across the whole budget used to add most of a
/// frame interval of pure delay to every small frame. The frame is now
/// compressed into the lesser of its own serialization time and the budget,
/// while oversized frames still use the whole budget instead of piling up.
pub fn frame_shard_spacing(
    wire_bytes: u64,
    rate_bps: u64,
    spread: Duration,
    shards: usize,
) -> Duration {
    let gaps = shards.saturating_sub(1).max(1) as u32;
    if wire_bytes == 0 || rate_bps == 0 {
        return spread / gaps;
    }
    let serialization = Duration::from_secs_f64(wire_bytes as f64 * 8.0 / rate_bps as f64);
    serialization.min(spread) / gaps
}

/// Clamp a QUIC-advertised datagram size to VmaX's safe payload budget.
#[inline]
pub fn safe_datagram_size(quinn_max: Option<usize>) -> usize {
    quinn_max
        .unwrap_or(MAX_APP_DATAGRAM_BYTES)
        .min(MAX_APP_DATAGRAM_BYTES)
}

/// Best-effort socket tuning.  Buffer sizing is mandatory; DSCP is optional
/// because Windows and some Android vendor kernels reject `IP_TOS`.
pub fn tune_udp_socket(socket: &StdUdpSocket) -> io::Result<()> {
    crate::wan_transport::tune_wan_socket(&socket2::SockRef::from(socket))
}

/// Sleep on the dedicated pacer thread, spinning only near the send deadline.
#[inline]
pub fn spin_sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        // A 150 us spin per shard consumes a significant fraction of a core
        // at 1080p. Unix timers can sleep through most of that interval.
        let spin = if cfg!(windows) { Duration::from_micros(150) } else { Duration::from_micros(30) };
        if remaining > spin * 2 {
            let sleep = remaining - spin;
            #[cfg(target_os = "windows")]
            if windows_pacing_wait::sleep(sleep) || sleep < Duration::from_millis(1) {
                std::hint::spin_loop();
                continue;
            }
            thread::sleep(sleep);
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_pacing_wait {
    use std::time::Duration;
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        System::Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW,
            SYNCHRONIZATION_SYNCHRONIZE, SetWaitableTimer, TIMER_MODIFY_STATE,
            WaitForSingleObject,
        },
    };
    use windows_core::PCWSTR;

    struct Timer(HANDLE);

    impl Drop for Timer {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    thread_local! {
        static TIMER: Option<Timer> = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_MODIFY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
            ).ok().map(Timer)
        };
    }

    pub(super) fn sleep(duration: Duration) -> bool {
        // Relative timers use negative 100 ns units. Thread sleep on older
        // Windows runtimes can round sub-ms packet waits up to the next tick.
        let ticks = (duration.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64;
        TIMER.with(|timer| {
            timer.as_ref().is_some_and(|timer| unsafe {
                SetWaitableTimer(timer.0, &-ticks, 0, None, None, false).is_ok()
                    && WaitForSingleObject(timer.0, u32::MAX) == WAIT_OBJECT_0
            })
        })
    }
}

/// Testable pacing core.  A packet's send instant is `max(now, next_slot)`;
/// after it is handed to the kernel, the next slot advances by exactly the
/// serialization time at the current target rate.  This produces even spacing
/// across the frame budget without the sawtooth of a burst-then-idle pacer.
#[derive(Debug, Clone)]
pub struct PaceEngine {
    rate_bps: u64,
    next_slot: Option<Instant>,
}

impl PaceEngine {
    pub fn new(rate_bps: u32) -> Self {
        Self {
            rate_bps: u64::from(rate_bps.max(1)),
            next_slot: None,
        }
    }

    pub fn set_rate_bps(&mut self, rate_bps: u32) {
        self.rate_bps = u64::from(rate_bps.max(1));
    }

    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// Returns the instant at which this packet should be sent and advances
    /// the internal schedule.
    pub fn slot_for(&mut self, len: usize, now: Instant) -> Instant {
        let wire_us =
            (len as u64).saturating_mul(8).saturating_mul(1_000_000) / self.rate_bps.max(1);
        let slot = match self.next_slot {
            Some(next) if next > now => next,
            _ => now,
        };
        self.next_slot = Some(slot + Duration::from_micros(wire_us.max(1)));
        slot
    }

    /// Drop any accumulated catch-up credit.  Call after a stall or a path
    /// migration so a backlog is not blasted at the new path.
    pub fn reset(&mut self, now: Instant) {
        self.next_slot = Some(now);
    }
}

#[derive(Debug, Default)]
pub struct PacerStats {
    sent_packets: AtomicU64,
    sent_bytes: AtomicU64,
    send_errors: AtomicU64,
    rejected_packets: AtomicU64,
    queue_bytes: AtomicU64,
    max_pacing_delay_us: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PacerSnapshot {
    pub sent_packets: u64,
    pub sent_bytes: u64,
    pub send_errors: u64,
    pub rejected_packets: u64,
    pub queue_bytes: u64,
    pub max_pacing_delay_us: u64,
}

impl PacerStats {
    pub fn sent_packets(&self) -> u64 { self.sent_packets.load(Ordering::Relaxed) }

    /// Bytes accepted by the pacer that have not left it yet.
    ///
    /// Live: the pacing thread subtracts each shard as it departs, so a reader
    /// on another thread sees the real queue rather than the value at the last
    /// completion.
    pub fn queued_bytes(&self) -> u64 {
        self.queue_bytes.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> PacerSnapshot {
        PacerSnapshot {
            sent_packets: self.sent_packets.load(Ordering::Relaxed),
            sent_bytes: self.sent_bytes.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            rejected_packets: self.rejected_packets.load(Ordering::Relaxed),
            queue_bytes: self.queue_bytes.load(Ordering::Relaxed),
            max_pacing_delay_us: self.max_pacing_delay_us.load(Ordering::Relaxed),
        }
    }
}

struct PacedFrame {
    shards: Vec<PacerShard>,
    deadline: Instant,
    spread: Duration,
    wire_bytes: u64,
}

type PacerQueue = Arc<(Mutex<Option<PacedFrame>>, Condvar)>;

/// One encoded shard with the identity the client will echo in its feedback
/// report.  Passing it through the pacer lets the pacing thread stamp the
/// actual wire departure time instead of the time the frame was queued.
#[derive(Debug, Clone)]
pub struct PacerShard {
    pub datagram: Bytes,
    pub frame_id: u64,
    pub chunk_idx: u16,
    pub parity: bool,
    pub weighted_parity: bool,
}

/// A datagram that has actually left the pacer.  `sent` is a monotonic host
/// instant, which is exactly what the delay-gradient estimator needs.
#[derive(Debug, Clone, Copy)]
pub struct SentShard {
    pub frame_id: u32,
    pub chunk_idx: u16,
    pub bytes: usize,
    pub parity: bool,
    pub sent: Instant,
}

/// Owns the dedicated pacing thread.  `send_frame` is non-blocking and
/// replaces any pending frame when the worker is still pacing a prior frame.
pub struct PacedSender {
    pending: PacerQueue,
    rate_bps: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    stats: Arc<PacerStats>,
    sent_log: Arc<Mutex<VecDeque<SentShard>>>,
    join: Option<JoinHandle<()>>,
}

impl PacedSender {
    pub fn spawn(conn: Connection, initial_rate_bps: u32, max_queued_frames: usize) -> Self {
        Self::spawn_with_stats(
            conn,
            initial_rate_bps,
            max_queued_frames,
            Arc::new(PacerStats::default()),
        )
    }

    /// Same as [`PacedSender::spawn`], but the caller keeps the statistics
    /// object.
    ///
    /// The host capture loop reads the queue through it to decide whether a new
    /// frame can still fit, so it must be able to observe the pacer's live
    /// queue rather than a snapshot taken at the last frame boundary.
    /// The queue limit is fixed at one pending frame for every caller.
    pub fn spawn_with_stats(
        conn: Connection,
        initial_rate_bps: u32,
        _max_queued_frames: usize,
        stats: Arc<PacerStats>,
    ) -> Self {
        let pending = Arc::new((Mutex::new(None), Condvar::new()));
        let worker_pending = Arc::clone(&pending);
        let rate_bps = Arc::new(AtomicU64::new(u64::from(initial_rate_bps.max(1))));
        let sent_log = Arc::new(Mutex::new(VecDeque::with_capacity(4096)));
        let thread_rate = Arc::clone(&rate_bps);
        let thread_stats = Arc::clone(&stats);
        let thread_log = Arc::clone(&sent_log);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = Arc::clone(&stopped);

        let join = thread::Builder::new()
            .name("vmax-datagram-pacer".to_string())
            .spawn(move || {
                pacing_loop(
                    conn,
                    worker_pending,
                    thread_rate,
                    Arc::clone(&thread_stats),
                    thread_log,
                    initial_rate_bps,
                    thread_stopped,
                );
            })
            .ok();

        Self {
            pending,
            rate_bps,
            stopped,
            stats,
            sent_log,
            join,
        }
    }

    /// Queue all shards of one encoded access unit.  Returns `false` if the
    /// frame was rejected before entering the pacer; no partial send occurs.
    pub fn send_frame(&self, shards: Vec<PacerShard>, deadline: Instant, spread: Duration) -> bool {
        if shards.is_empty() {
            return true;
        }
        let bytes: usize = shards.iter().map(|shard| shard.datagram.len()).sum();
        let too_large = shards
            .iter()
            .any(|shard| shard.datagram.len() > MAX_APP_DATAGRAM_BYTES);
        if too_large {
            self.stats.rejected_packets.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let Ok(mut pending) = self.pending.0.lock() else {
            return false;
        };
        if self.stopped.load(Ordering::Relaxed) || self.join.is_none() {
            return false;
        }
        let wire_bytes = bytes as u64;
        self.stats.queue_bytes.fetch_add(wire_bytes, Ordering::Relaxed);
        if let Some(previous) = pending.replace(PacedFrame { shards, deadline, spread, wire_bytes }) {
            self.stats.queue_bytes.fetch_sub(previous.wire_bytes, Ordering::Relaxed);
            self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.pending.1.notify_one();
        true
    }

    pub fn set_rate_bps(&self, rate_bps: u32) {
        let rate = u64::from(rate_bps.max(1));
        self.rate_bps.store(rate, Ordering::Relaxed);
    }

    /// Drains the departure log.  The host calls this on every feedback
    /// report, which is exactly when the send history is about to be used.
    pub fn drain_sent(&self) -> Vec<SentShard> {
        self.sent_log
            .lock()
            .map(|mut log| log.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn stats(&self) -> PacerSnapshot {
        self.stats.snapshot()
    }

    pub fn shutdown(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Ok(mut pending) = self.pending.0.lock() {
            if let Some(frame) = pending.take() {
                self.stats.queue_bytes.fetch_sub(frame.wire_bytes, Ordering::Relaxed);
            }
            self.pending.1.notify_one();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PacedSender {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn pacing_loop(
    conn: Connection,
    queue: PacerQueue,
    rate_bps: Arc<AtomicU64>,
    stats: Arc<PacerStats>,
    sent_log: Arc<Mutex<VecDeque<SentShard>>>,
    initial_rate: u32,
    stopped: Arc<AtomicBool>,
) {
    let mut engine = PaceEngine::new(initial_rate);
    loop {
        let pending = queue.0.lock().unwrap_or_else(|error| error.into_inner());
        let mut pending = queue.1.wait_while(pending, |frame| {
            frame.is_none() && !stopped.load(Ordering::Relaxed)
        }).unwrap_or_else(|error| error.into_inner());
        let Some(PacedFrame { shards, deadline, spread, wire_bytes }) = pending.take() else {
            break;
        };
        drop(pending);
        let mut failed = false;
        let mut remaining_bytes = wire_bytes;
        let mut next_shard = Instant::now();
        let spacing = frame_shard_spacing(
            wire_bytes,
            rate_bps.load(Ordering::Relaxed),
            spread,
            shards.len(),
        );
        for shard in shards {
            if stopped.load(Ordering::Relaxed) {
                stats.queue_bytes.fetch_sub(remaining_bytes, Ordering::Relaxed);
                return;
            }
            let bytes = shard.datagram.len() as u64;
            remaining_bytes -= bytes;
            if failed || Instant::now() >= deadline {
                if !failed {
                    stats.send_errors.fetch_add(1, Ordering::Relaxed);
                }
                failed = true;
                stats.queue_bytes.fetch_sub(bytes, Ordering::Relaxed);
                continue;
            }
            let rate = rate_bps.load(Ordering::Relaxed);
            engine.set_rate_bps(rate.min(u64::from(u32::MAX)) as u32);
            let now = Instant::now();
            // Apply the wire rate and the frame's minimum shard spacing.
            let slot = engine.slot_for(shard.datagram.len(), now.max(next_shard));
            next_shard = slot + spacing;
            let delay = slot.saturating_duration_since(now);
            if !delay.is_zero() {
                stats.max_pacing_delay_us.fetch_max(
                    delay.as_micros().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                spin_sleep_until(slot.min(deadline));
            }
            let result = if Instant::now() < deadline
                && conn.datagram_send_buffer_space() >= bytes as usize
            {
                conn.send_datagram(shard.datagram).ok()
            } else {
                None
            };
            match result {
                Some(()) => {
                    stats.sent_packets.fetch_add(1, Ordering::Relaxed);
                    stats.sent_bytes.fetch_add(bytes, Ordering::Relaxed);
                    if !shard.weighted_parity
                        && let Ok(mut log) = sent_log.lock()
                    {
                        log.push_back(SentShard {
                            frame_id: shard.frame_id as u32,
                            chunk_idx: shard.chunk_idx,
                            bytes: bytes as usize,
                            parity: shard.parity,
                            sent: Instant::now(),
                        });
                        while log.len() > 8192 {
                            log.pop_front();
                        }
                    }
                }
                None => {
                    failed = true;
                    stats.send_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            stats.queue_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

/// Network signals that drive FEC.  `ack_loss_permille` is the feedback-report
/// loss measured by the host, not media loss.
#[derive(Debug, Clone, Copy, Default)]
pub struct FecSignal {
    pub loss_permille: u16,
    pub ack_loss_permille: u16,
    pub jitter_us: u32,
    pub rtt_ms: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecDecision {
    /// Effective redundancy, 2..=25 percent.
    pub parity_pct: u8,
    /// Data shards per FEC block.  One parity shard per block when
    /// `two_shard` is false, two parity shards when it is true.
    pub group_size: u8,
    pub two_shard: bool,
}

impl FecDecision {
    pub fn parity_permille(self) -> u16 {
        if self.group_size == 0 {
            0
        } else if self.two_shard {
            (2 * 1000) / u16::from(self.group_size.max(1))
        } else {
            1000 / u16::from(self.group_size.max(1))
        }
    }

    /// Values ready for `protocol::packetize_video`.
    pub fn packetizer(self) -> (u8, bool) {
        (self.group_size, self.two_shard)
    }
}

impl Default for FecDecision {
    fn default() -> Self {
        Self {
            parity_pct: 2,
            group_size: 50,
            two_shard: false,
        }
    }
}

/// Loss-responsive Reed-Solomon controller.  Clean links stay at 2%, a 3%
/// ACK-loss path climbs to roughly 20%, and a badly congested mobile link tops
/// out at 25%.  Increases are immediate; decreases are intentionally slow to
/// avoid parity oscillation around the loss threshold.
pub struct AdaptiveFec {
    current: FecDecision,
    last_decrease: Option<Instant>,
    last_signal: FecSignal,
}

impl AdaptiveFec {
    pub fn new() -> Self {
        Self {
            current: FecDecision::default(),
            last_decrease: None,
            last_signal: FecSignal::default(),
        }
    }

    /// Start from the session's configured parity budget.  The controller
    /// relaxes to 2% after feedback proves the path is clean, but the first
    /// keyframe is protected at the configured (usually 10%) level.
    pub fn with_group_size(group_size: u8) -> Self {
        let group_size = group_size.min(50);
        let parity_pct = if group_size == 0 {
            0
        } else {
            (100.0 / f32::from(group_size)).round() as u8
        };
        Self {
            current: FecDecision {
                parity_pct,
                group_size,
                two_shard: false,
            },
            last_decrease: None,
            last_signal: FecSignal::default(),
        }
    }

    pub fn current(&self) -> FecDecision {
        self.current
    }

    pub fn last_signal(&self) -> FecSignal {
        self.last_signal
    }

    pub fn observe(&mut self, signal: FecSignal, now: Instant) -> FecDecision {
        self.last_signal = signal;
        let loss = f32::from(signal.loss_permille);
        let ack_loss = f32::from(signal.ack_loss_permille);
        let mut target = if signal.rtt_ms >= 45 || signal.jitter_us >= 4_000 {
            10.0
        } else {
            2.0
        };
        target += loss * 0.20; // 3% media loss -> +6%
        target += ack_loss * 0.60; // 3% feedback loss -> +18%
        if signal.jitter_us > 8_000 {
            target += 2.0;
        }
        if signal.rtt_ms > 120 {
            target += 1.0;
        }
        target = if signal.loss_permille > 20 || signal.ack_loss_permille > 30 {
            25.0
        } else {
            target.clamp(2.0, 25.0)
        };

        // A single parity shard gives 100/k; above 10% use two shards, which
        // preserves the existing partial-group packet budget.
        let desired = if target > 10.0 {
            FecDecision {
                parity_pct: target.round() as u8,
                group_size: ((200.0 / target).round() as u8).clamp(8, 32),
                two_shard: true,
            }
        } else {
            FecDecision {
                parity_pct: target.round() as u8,
                group_size: ((100.0 / target).round() as u8).clamp(8, 50),
                two_shard: false,
            }
        };

        if desired.group_size < self.current.group_size
            || desired.two_shard && !self.current.two_shard
        {
            // More protection (smaller group / adding a second shard): act now.
            self.current = desired;
            self.last_decrease = Some(now);
        } else if desired != self.current {
            // Relaxing protection is deliberately rate-limited so parity does
            // not oscillate around the loss threshold.
            let can_relax = self.last_decrease.is_none_or(|last| {
                now.saturating_duration_since(last) >= Duration::from_millis(500)
            });
            if can_relax {
                self.current = desired;
                self.last_decrease = Some(now);
            }
        }
        self.current
    }
}

impl Default for AdaptiveFec {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_spaces_equal_packets_at_the_target_rate() {
        let origin = Instant::now();
        let mut engine = PaceEngine::new(8_000_000); // 1 MB/s
        let first = engine.slot_for(1_000, origin);
        let second = engine.slot_for(1_000, origin);
        let third = engine.slot_for(1_000, origin);
        assert_eq!(first, origin);
        assert_eq!(
            second.saturating_duration_since(first),
            Duration::from_millis(1)
        );
        assert_eq!(
            third.saturating_duration_since(second),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn pacer_does_not_accumulate_unbounded_catch_up_credit() {
        let origin = Instant::now();
        let mut engine = PaceEngine::new(8_000_000);
        let _ = engine.slot_for(1_000, origin);
        let after_stall = origin + Duration::from_secs(2);
        assert_eq!(engine.slot_for(1_000, after_stall), after_stall);
    }

    #[test]
    fn small_frames_are_not_stretched_across_the_whole_pacing_budget() {
        let spread = Duration::from_millis(10);
        // 20 KB at 25 Mbit/s serialises in ~6.4 ms, so the shards should
        // occupy that, not the full 10 ms budget.
        let tight = frame_shard_spacing(20_000, 25_000_000, spread, 21);
        assert!(tight < Duration::from_micros(400));
        // A frame larger than the budget still uses the whole budget.
        assert_eq!(frame_shard_spacing(200_000, 25_000_000, spread, 2), spread);
        // Degenerate counts and rates stay bounded.
        assert!(frame_shard_spacing(1_000, 25_000_000, spread, 1) <= spread);
        assert_eq!(frame_shard_spacing(0, 25_000_000, spread, 4), spread / 3);
    }

    #[test]
    fn fec_stays_at_two_percent_on_a_clean_link() {
        let mut fec = AdaptiveFec::new();
        let decision = fec.observe(FecSignal::default(), Instant::now());
        assert_eq!(decision.parity_pct, 2);
        assert_eq!(decision.group_size, 50);
        assert!(!decision.two_shard);
    }

    #[test]
    fn fec_climbs_to_twenty_five_percent_on_ack_loss() {
        let mut fec = AdaptiveFec::new();
        let decision = fec.observe(
            FecSignal {
                loss_permille: 30,
                ack_loss_permille: 35,
                jitter_us: 12_000,
                rtt_ms: 180,
            },
            Instant::now(),
        );
        assert!(decision.parity_pct >= 20);
        assert!(decision.two_shard);
        assert!(decision.parity_permille() <= 250);
    }

    #[test]
    fn fec_decay_waits_for_the_hold_interval() {
        let mut fec = AdaptiveFec::new();
        let now = Instant::now();
        let _ = fec.observe(
            FecSignal {
                loss_permille: 100,
                ack_loss_permille: 100,
                ..Default::default()
            },
            now,
        );
        let high = fec.current();
        let immediate = fec.observe(FecSignal::default(), now + Duration::from_millis(50));
        assert_eq!(immediate, high);
    }

    #[test]
    fn safe_datagram_size_never_exceeds_the_mobile_budget() {
        assert_eq!(safe_datagram_size(None), MAX_APP_DATAGRAM_BYTES);
        assert_eq!(safe_datagram_size(Some(9000)), MAX_APP_DATAGRAM_BYTES);
        assert_eq!(safe_datagram_size(Some(1000)), 1000);
    }

    #[test]
    fn spin_sleep_returns_within_a_practical_bound() {
        let start = Instant::now();
        spin_sleep_until(start + Duration::from_micros(400));
        assert!(start.elapsed() >= Duration::from_micros(350));
    }
}
