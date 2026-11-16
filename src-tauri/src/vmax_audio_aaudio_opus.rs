//! VmaX Audio Pipeline: 48kHz Stereo Opus (10ms) + Adaptive Jitter Buffer + PLC
//! Designed for QUIC datagrams + Android AAudio (PERFORMANCE_MODE_LOW_LATENCY)

use std::collections::BTreeMap;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// 10ms frame = 480 samples per channel (matches VmaX 10ms Opus+RNNoise framing)
pub const FRAME_SAMPLES_10MS: usize = 480;

#[derive(Debug, Clone, Copy)]
pub struct AudioConfig {
    pub game_audio_bitrate_bps: u32, // Recommended: 64_000 - 96_000
    pub mic_bitrate_bps: u32,        // Recommended: 24_000 - 32_000
    pub enable_rnnoise_on_mic: bool, // Keep FALSE on game audio to preserve fidelity
    pub target_jitter_frames: usize, // 1-2 on LAN (10-20ms), 3-4 on 5G/Internet (30-40ms)
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            game_audio_bitrate_bps: 80_000,
            mic_bitrate_bps: 28_000,
            enable_rnnoise_on_mic: true,
            target_jitter_frames: 2,
        }
    }
}

pub enum JitterPop {
    Packet(Vec<u8>),
    /// Trigger Opus Packet Loss Concealment (opus_decode with NULL data)
    ConcealLoss,
    /// Buffer underrun / still priming
    Wait,
}

/// Sequence-ordered jitter buffer for QUIC audio datagrams
pub struct AdaptiveAudioJitterBuffer {
    queue: BTreeMap<u32, Vec<u8>>,
    next_seq: Option<u32>,
    target_frames: usize,
    max_frames: usize,
    primed: bool,
}

impl AdaptiveAudioJitterBuffer {
    pub fn new(target_frames: usize) -> Self {
        Self {
            queue: BTreeMap::new(),
            next_seq: None,
            target_frames: target_frames.clamp(1, 8),
            max_frames: 12,
            primed: false,
        }
    }

    pub fn push(&mut self, seq: u32, opus_payload: Vec<u8>) {
        if let Some(expected) = self.next_seq {
            if seq < expected {
                return; // Late packet already concealed
            }
        }
        self.queue.insert(seq, opus_payload);

        // Drop oldest frames if latency builds up (prevents audio drift)
        while self.queue.len() > self.max_frames {
            if let Some((&oldest, _)) = self.queue.iter().next() {
                self.queue.remove(&oldest);
                self.next_seq = Some(oldest.wrapping_add(1));
            }
        }
    }

    pub fn pop_10ms_frame(&mut self) -> JitterPop {
        if !self.primed {
            if self.queue.len() >= self.target_frames {
                self.primed = true;
                if let Some((&first_seq, _)) = self.queue.iter().next() {
                    self.next_seq = Some(first_seq);
                }
            } else {
                return JitterPop::Wait;
            }
        }

        let seq = match self.next_seq {
            Some(s) => s,
            None => return JitterPop::Wait,
        };
        self.next_seq = Some(seq.wrapping_add(1));

        if let Some(pkt) = self.queue.remove(&seq) {
            JitterPop::Packet(pkt)
        } else {
            // Packet lost in transit -> use Opus PLC instead of silence
            JitterPop::ConcealLoss
        }
    }
}
