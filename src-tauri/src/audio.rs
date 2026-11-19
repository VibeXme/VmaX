use crate::protocol::{AudioCodec, AudioPacket, AudioPacketHeader, audio_flags};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use nnnoiseless::DenoiseState;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const RNNOISE_SAMPLE_RATE: u32 = 48_000;
const RNNOISE_FRAME_LEN: usize = DenoiseState::FRAME_SIZE; // 480 samples (10 ms @ 48 kHz)
/// 16-bit PCM halves wire bandwidth vs f32 (zero C deps) while staying under 1200B MTU
const PACKET_FRAME_COUNT_MONO: usize = 480; // 10 ms mono i16 = 960 B + 16B hdr = 976 B
const PACKET_FRAME_COUNT_STEREO: usize = 240; // 5 ms stereo i16 = 960 B + 16B hdr = 976 B
const PLAYBACK_PREFILL_MS: usize = 30;
const PLAYBACK_MAX_BUFFER_MS: usize = 95;

struct PlaybackBuffer {
    samples: VecDeque<f32>,
    last_seq: Option<u32>,
    channels: usize,
    pending: Vec<(u32, Vec<f32>)>,
    max_samples: usize,
    callback_len: usize,
    playing: bool,
    phase: f64,
    rate: f64,
    last_frame: [f32; 8],
}

impl PlaybackBuffer {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(16_384),
            last_seq: None,
            channels: 2,
            pending: Vec::with_capacity(16),
            max_samples: 0,
            callback_len: 0,
            playing: false,
            phase: 0.0,
            rate: 1.0,
            last_frame: [0.0; 8],
        }
    }

    fn push(&mut self, seq: u32, samples: Vec<f32>, max_samples: usize) {
        if samples.is_empty() {
            return;
        }
        self.max_samples = max_samples;
        if let Some(previous) = self.last_seq {
            let distance = seq.wrapping_sub(previous);
            if distance == 0 || distance >= 1 << 31 {
                return;
            }
            // Only reset on a major stall (> ~90ms at 5ms/pkt), not brief Wi-Fi jitter bursts.
            if distance > 18 {
                self.samples.clear();
                self.pending.clear();
                self.playing = false;
                self.phase = 0.0;
                self.rate = 1.0;
            } else if distance > 1 {
                if !self.pending.iter().any(|(id, _)| *id == seq) {
                    self.pending.push((seq, samples));
                }
                return;
            }
        }
        self.append(seq, samples);
        self.drain_pending(0);
    }

    fn append(&mut self, seq: u32, samples: Vec<f32>) {
        self.last_seq = Some(seq);
        let limit = self
            .max_samples
            .max(self.callback_len.saturating_add(self.max_samples / 2));
        self.samples.extend(samples);
        let excess = self.samples.len().saturating_sub(limit);
        if excess > 0 {
            self.samples.drain(..excess);
            self.phase = 0.0;
        }
    }

    fn drain_pending(&mut self, needed: usize) {
        while let Some(previous) = self.last_seq {
            let Some((index, distance)) = self
                .pending
                .iter()
                .enumerate()
                .map(|(index, (seq, _))| (index, seq.wrapping_sub(previous)))
                .min_by_key(|(_, distance)| *distance)
            else {
                break;
            };
            if distance > 1 && self.samples.len() >= needed {
                break;
            }
            let (seq, samples) = self.pending.swap_remove(index);
            if distance > 1 {
                debug!(
                    missing = distance - 1,
                    previous, seq, "audio packets missed playout deadline"
                );
                self.conceal_gap((distance as usize - 1).min(4), &samples);
            }
            self.append(seq, samples);
        }
    }

    /// Bridge a short loss from the last emitted sample into the next packet.
    /// This removes the hard zero-fill edge that causes audible clipping.
    fn conceal_gap(&mut self, missing_packets: usize, next_packet: &[f32]) {
        let channels = self.channels.max(1);
        let frame_count = next_packet.len() / channels;
        if frame_count == 0 {
            return;
        }
        for packet_index in 0..missing_packets {
            let blend = (packet_index + 1) as f32 / (missing_packets + 1) as f32;
            for frame in 0..frame_count {
                let frame_blend = blend * (frame + 1) as f32 / frame_count as f32;
                for channel in 0..channels {
                    let from = self.last_frame.get(channel).copied().unwrap_or(0.0);
                    let to = next_packet[frame * channels + channel];
                    self.samples.push_back(from + (to - from) * frame_blend);
                }
            }
        }
    }

    fn render<T: cpal::Sample + cpal::FromSample<f32>>(
        &mut self,
        out: &mut [T],
        prefill: usize,
        channels: usize,
    ) {
        self.callback_len = out.len();
        let needed = if self.playing {
            out.len()
        } else {
            out.len().saturating_add(prefill)
        };
        self.drain_pending(needed);
        if !self.playing {
            self.playing = self.samples.len() >= out.len().saturating_add(prefill);
        }
        if !self.playing {
            // Smoothly fade held samples to silence instead of hard-cutting to zero
            for frame in out.chunks_exact_mut(channels) {
                for (ch, slot) in frame.iter_mut().enumerate() {
                    let held = if ch < self.last_frame.len() {
                        self.last_frame[ch] *= 0.96;
                        self.last_frame[ch]
                    } else {
                        0.0
                    };
                    *slot = T::from_sample(held);
                }
            }
            self.phase = 0.0;
            self.rate = 1.0;
            return;
        }

        let target = out.len() + prefill;
        let error = (self.samples.len() as f64 - target as f64) / target.max(1) as f64;
        let desired_rate = 1.0 + (error * 0.008).clamp(-0.005, 0.005);
        self.rate += (desired_rate - self.rate) * 0.02;

        for frame in out.chunks_exact_mut(channels) {
            if self.samples.len() < channels {
                // Buffer briefly ran dry mid-callback: decay smoothly without resetting `playing`
                for (ch, slot) in frame.iter_mut().enumerate() {
                    let held = if ch < self.last_frame.len() {
                        self.last_frame[ch] *= 0.92;
                        self.last_frame[ch]
                    } else {
                        0.0
                    };
                    *slot = T::from_sample(held);
                }
                continue;
            }

            for (channel, slot) in frame.iter_mut().enumerate() {
                let a = self.samples.get(channel).copied().unwrap_or(0.0);
                let b = self.samples.get(channels + channel).copied().unwrap_or(a);
                let sample = a + (b - a) * self.phase as f32;
                if channel < self.last_frame.len() {
                    self.last_frame[channel] = sample;
                }
                *slot = T::from_sample(sample);
            }
            self.phase += self.rate;
            let consumed = self.phase as usize;
            self.phase -= consumed as f64;
            let drain_count = (consumed * channels).min(self.samples.len());
            self.samples.drain(..drain_count);
        }

        if self.samples.is_empty() {
            self.playing = false;
        }
    }
}

pub struct AudioCaptureHandle {
    stop_flag: Arc<AtomicBool>,
    rnnoise_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCaptureHandle {
    pub fn set_rnnoise_enabled(&self, enabled: bool) {
        self.rnnoise_flag.store(enabled, Ordering::Relaxed);
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioCaptureHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

pub struct AudioPlaybackHandle {
    stop_flag: Arc<AtomicBool>,
    ring: Arc<Mutex<PlaybackBuffer>>,
    thread: Option<std::thread::JoinHandle<()>>,
    target_rate: u32,
    target_channels: u16,
}

impl AudioPlaybackHandle {
    pub fn push_packet(&self, pkt: &AudioPacket) {
        let samples = if pkt.header.codec == AudioCodec::PcmI16 as u8 {
            bytes_i16_to_f32_vec(&pkt.payload)
        } else if pkt.header.codec == AudioCodec::PcmF32 as u8 {
            bytes_to_f32_vec(&pkt.payload)
        } else {
            return;
        };

        if samples.is_empty() {
            return;
        }

        let channels = (pkt.header.channels as usize).max(1);
        let target_ch = self.target_channels.max(1) as usize;
        if !matches!(channels, 1 | 2) || samples.len() % channels != 0 {
            return;
        }
        let remixed = remix_channels(&samples, channels, target_ch);
        let resampled =
            resample_cubic_interleaved(&remixed, RNNOISE_SAMPLE_RATE, self.target_rate, target_ch);

        let Ok(mut guard) = self.ring.lock() else {
            return;
        };
        guard.channels = target_ch;
        let configured =
            (self.target_rate as usize * PLAYBACK_MAX_BUFFER_MS / 1_000).max(1) * target_ch;
        guard.push(pkt.header.seq, resampled, configured);
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioPlaybackHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn bytes_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

fn bytes_i16_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<2>();
    chunks
        .iter()
        .map(|chunk| f32::from(i16::from_le_bytes(*chunk)) / 32768.0)
        .collect()
}

fn f32_slice_to_i16_bytes(samples: &[f32]) -> Bytes {
    let mut buf = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        buf.extend_from_slice(&clamped.to_le_bytes());
    }
    Bytes::from(buf)
}

fn remix_channels(input: &[f32], src_ch: usize, dst_ch: usize) -> Vec<f32> {
    if src_ch == dst_ch {
        return input.to_vec();
    }
    let frames = input.len() / src_ch.max(1);
    let mut out = Vec::with_capacity(frames * dst_ch);
    for f in 0..frames {
        let base = f * src_ch;
        if dst_ch == 1 {
            let sum: f32 = input[base..base + src_ch].iter().copied().sum();
            out.push(sum / src_ch as f32);
        } else {
            let mono_or_l = input[base];
            let r = if src_ch > 1 {
                input[base + 1]
            } else {
                mono_or_l
            };
            for c in 0..dst_ch {
                out.push(if c % 2 == 0 { mono_or_l } else { r });
            }
        }
    }
    out
}

/// 4-point Catmull-Rom cubic interpolator: avoids the metallic aliasing distortion
/// produced by linear interpolation when converting 44.1 kHz <-> 48 kHz audio.
fn resample_cubic_interleaved(
    input: &[f32],
    src_rate: u32,
    dst_rate: u32,
    channels: usize,
) -> Vec<f32> {
    if src_rate == dst_rate || input.is_empty() || channels == 0 {
        return input.to_vec();
    }
    let src_frames = input.len() / channels;
    if src_frames < 4 {
        return input.to_vec();
    }
    let dst_frames = ((src_frames as f64) * (dst_rate as f64 / src_rate as f64))
        .round()
        .max(1.0) as usize;
    let mut out = Vec::with_capacity(dst_frames * channels);
    let ratio = (src_frames - 1) as f64 / (dst_frames.saturating_sub(1).max(1)) as f64;

    for i in 0..dst_frames {
        let pos = (i as f64) * ratio;
        let idx = (pos.floor() as usize).min(src_frames - 2);
        let t = (pos - idx as f64) as f32;
        let t2 = t * t;
        let t3 = t2 * t;

        let i0 = idx.saturating_sub(1);
        let i1 = idx;
        let i2 = (idx + 1).min(src_frames - 1);
        let i3 = (idx + 2).min(src_frames - 1);

        for ch in 0..channels {
            let p0 = input[i0 * channels + ch];
            let p1 = input[i1 * channels + ch];
            let p2 = input[i2 * channels + ch];
            let p3 = input[i3 * channels + ch];

            let sample = 0.5
                * ((2.0 * p1)
                    + (-p0 + p2) * t
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
                    + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3);
            out.push(sample.clamp(-1.0, 1.0));
        }
    }
    out
}

#[derive(Serialize)]
pub struct AudioOutput {
    id: String,
    name: String,
}

pub fn list_audio_outputs() -> Result<Vec<AudioOutput>, String> {
    cpal::default_host()
        .output_devices()
        .map_err(|err| err.to_string())
        .map(|devices| {
            devices
                .filter_map(|device| {
                    let id = device.id().ok()?.to_string();
                    let name = device.description().ok()?.name().to_owned();
                    Some(AudioOutput { id, name })
                })
                .collect()
        })
}

pub fn start_host_audio_capture(
    audio_tx: mpsc::Sender<AudioPacket>,
    audio_bitrate_kbps: u16,
    rnnoise_enabled: bool,
    output_id: Option<String>,
    process_id: Option<u32>,
    exclude_discord: bool,
) -> Result<AudioCaptureHandle, String> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let rnnoise_flag = Arc::new(AtomicBool::new(rnnoise_enabled));
    let stop_worker = Arc::clone(&stop_flag);
    let rnnoise_worker = Arc::clone(&rnnoise_flag);

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("vmax-audio-capture".to_string())
        .spawn(move || {
            #[cfg(target_os = "windows")]
            if exclude_discord || process_id.is_some() || output_id.is_none() {
                let target = if let Some(pid) = process_id {
                    (pid, true)
                } else if exclude_discord {
                    (discord_root_pid().unwrap_or_else(|_| std::process::id()), false)
                } else {
                    (std::process::id(), false)
                };
                if let Err(err) = capture_process_audio(target.0, target.1, &audio_tx, &stop_worker, &init_tx) {
                    warn!(%err, "filtered audio capture stopped");
                    let _ = init_tx.send(Err(err));
                }
                return;
            }
            #[cfg(not(target_os = "windows"))]
            let _ = (process_id, exclude_discord);
            let host = cpal::default_host();

            let mut candidates: Vec<(cpal::Device, cpal::SupportedStreamConfig, bool)> = Vec::new();
            let device = match output_id.as_deref() {
                Some(id) => host.output_devices().ok().and_then(|mut outputs| {
                    outputs.find(|device| device.id().is_ok_and(|candidate| candidate.to_string() == id))
                }),
                None => host.default_output_device(),
            };
            if let Some(out_dev) = device
                && let Ok(cfg) = out_dev.default_output_config()
            {
                candidates.push((out_dev, cfg, true));
            }
            if candidates.is_empty() {
                let _ = init_tx.send(Err(
                    "The selected playback device is unavailable. Choose another host audio output.".to_string(),
                ));
                return;
            }

            let mut opened: Option<(Stream, u32, usize, bool)> = None;
            let mut last_err = String::from("unable to open audio capture device");

            for (device, config, is_loopback) in candidates {
                let src_rate = config.sample_rate();
                let src_channels = config.channels() as usize;
                let sample_format = config.sample_format();
                let stream_cfg: StreamConfig = config.into();
                let desired_channels: usize = if audio_bitrate_kbps >= 96 { 2 } else { 1 };
                let seq_counter = Arc::new(AtomicU32::new(1));
                let processor = Arc::new(Mutex::new(HostAudioProcessor::new(
                    src_rate,
                    src_channels,
                    desired_channels,
                    is_loopback,
                )));

                let tx_cb = audio_tx.clone();
                let seq_cb = Arc::clone(&seq_counter);
                let proc_cb = Arc::clone(&processor);
                let rnn_cb = Arc::clone(&rnnoise_worker);
                let err_cb = |err: cpal::Error| {
                    warn!(error = %err, "host audio capture stream error");
                };

                let build_res: Result<Stream, String> = match sample_format {
                    SampleFormat::F32 => device
                        .build_input_stream(
                            stream_cfg,
                            move |data: &[f32], _| {
                                if let Ok(mut guard) = proc_cb.lock() {
                                    guard.push_and_emit(
                                        data,
                                        rnn_cb.load(Ordering::Relaxed),
                                        &seq_cb,
                                        &tx_cb,
                                    );
                                }
                            },
                            err_cb,
                            None,
                        )
                        .map_err(|e| format!("failed to build f32 capture stream: {e}")),
                    SampleFormat::I16 => device
                        .build_input_stream(
                            stream_cfg,
                            move |data: &[i16], _| {
                                let converted: Vec<f32> =
                                    data.iter().map(|&s| s as f32 / 32768.0).collect();
                                if let Ok(mut guard) = proc_cb.lock() {
                                    guard.push_and_emit(
                                        &converted,
                                        rnn_cb.load(Ordering::Relaxed),
                                        &seq_cb,
                                        &tx_cb,
                                    );
                                }
                            },
                            err_cb,
                            None,
                        )
                        .map_err(|e| format!("failed to build i16 capture stream: {e}")),
                    SampleFormat::U16 => device
                        .build_input_stream(
                            stream_cfg,
                            move |data: &[u16], _| {
                                let converted: Vec<f32> = data
                                    .iter()
                                    .map(|&s| (s as f32 - 32768.0) / 32768.0)
                                    .collect();
                                if let Ok(mut guard) = proc_cb.lock() {
                                    guard.push_and_emit(
                                        &converted,
                                        rnn_cb.load(Ordering::Relaxed),
                                        &seq_cb,
                                        &tx_cb,
                                    );
                                }
                            },
                            err_cb,
                            None,
                        )
                        .map_err(|e| format!("failed to build u16 capture stream: {e}")),
                    other => Err(format!("unsupported host audio sample format: {other:?}")),
                };

                match build_res {
                    Ok(s) => {
                        opened = Some((s, src_rate, src_channels, is_loopback));
                        break;
                    }
                    Err(e) => {
                        last_err = e;
                    }
                }
            }

            let Some((stream, src_rate, src_channels, source_is_loopback)) = opened else {
                let _ = init_tx.send(Err(last_err));
                return;
            };
            let desired_channels: usize = if audio_bitrate_kbps >= 96 { 2 } else { 1 };

            if let Err(e) = stream.play() {
                let _ = init_tx.send(Err(format!("failed to start audio capture stream: {e}")));
                return;
            }

            info!(
                src_rate,
                src_channels, desired_channels, source_is_loopback, "host audio capture active"
            );
            let _ = init_tx.send(Ok(()));

            while !stop_worker.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            drop(stream);
        })
        .map_err(|e| format!("failed to spawn audio capture thread: {e}"))?;

    match init_rx.recv() {
        Ok(Ok(())) => Ok(AudioCaptureHandle {
            stop_flag,
            rnnoise_flag,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            stop_flag.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop_flag.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err("audio capture thread exited unexpectedly".to_string())
        }
    }
}

#[cfg(target_os = "windows")]
fn discord_root_pid() -> Result<u32, String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|err| err.to_string())?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut discord = Vec::new();
    let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
    while next.is_ok() {
        let len = entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "discord.exe" | "discordptb.exe" | "discordcanary.exe" | "discorddevelopment.exe"
        ) {
            discord.push((entry.th32ProcessID, entry.th32ParentProcessID));
        }
        next = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    let _ = unsafe { CloseHandle(snapshot) };
    let mut roots = discord
        .iter()
        .filter(|(_, parent)| !discord.iter().any(|(pid, _)| pid == parent));
    match (roots.next(), roots.next()) {
        (Some(&(pid, _)), None) => Ok(pid),
        (None, _) => {
            Err("Open the Discord desktop app before hosting with Discord exclusion.".into())
        }
        _ => Err(
            "Close extra Discord desktop instances before hosting with Discord exclusion.".into(),
        ),
    }
}

#[cfg(target_os = "windows")]
fn capture_process_audio(
    pid: u32,
    include: bool,
    tx: &mpsc::Sender<AudioPacket>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

    wasapi::initialize_mta()
        .ok()
        .map_err(|err| err.to_string())?;
    let result = (|| -> Result<(), wasapi::WasapiError> {
        let mut client = AudioClient::new_application_loopback_client(pid, include)?;
        let format = WaveFormat::new(
            32,
            32,
            SampleType::Float,
            RNNOISE_SAMPLE_RATE as usize,
            2,
            None,
        );
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        };
        client.initialize_client(&format, Direction::Capture, mode, None)?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        let mut bytes = VecDeque::new();
        let mut processor = HostAudioProcessor::new(RNNOISE_SAMPLE_RATE, 2, 2, true);
        let sequence = AtomicU32::new(1);
        client.start_stream()?;
        let _ = ready.send(Ok(()));
        let mut next_check = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if !include && pid != std::process::id() && std::time::Instant::now() >= next_check {
                if discord_root_pid() != Ok(pid) {
                    warn!("Discord process changed; stopped filtered audio until hosting restarts");
                    break;
                }
                next_check = std::time::Instant::now() + std::time::Duration::from_millis(100);
            }
            while !stop.load(Ordering::Relaxed) && capture.get_next_packet_size()?.unwrap_or(0) > 0
            {
                capture.read_from_device_to_deque(&mut bytes)?;
                processor.push_and_emit(
                    bytes_to_f32_vec(bytes.make_contiguous()).as_slice(),
                    false,
                    &sequence,
                    tx,
                );
                bytes.clear();
            }
            let _ = event.wait_for_event(25);
        }
        client.stop_stream()
    })();
    wasapi::deinitialize();
    result.map_err(|err| format!("Filtered audio unavailable for PID {pid}: {err}"))
}

pub fn start_client_audio_playback() -> Result<AudioPlaybackHandle, String> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ring = Arc::new(Mutex::new(PlaybackBuffer::new()));
    let stop_worker = Arc::clone(&stop_flag);
    let ring_worker = Arc::clone(&ring);

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(u32, u16), String>>();

    let thread = std::thread::Builder::new()
        .name("vmax-audio-playback".to_string())
        .spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_output_device() else {
                let _ = init_tx.send(Err("no audio output device found".to_string()));
                return;
            };
            let cfg = match device.default_output_config() {
                Ok(c) => c,
                Err(e) => {
                    let _ = init_tx.send(Err(format!("default output config failed: {e}")));
                    return;
                }
            };

            let target_rate = cfg.sample_rate();
            let target_channels = cfg.channels();
            let sample_format = cfg.sample_format();
            let stream_cfg: StreamConfig = cfg.into();
            let prefill =
                (target_rate as usize * PLAYBACK_PREFILL_MS / 1_000) * usize::from(target_channels);

            let err_cb = |err: cpal::Error| {
                debug!(error = %err, "client audio playback stream error");
            };

            macro_rules! output_stream {
                ($sample:ty) => {
                    device
                        .build_output_stream(
                            stream_cfg,
                            move |out: &mut [$sample], _| {
                                if let Ok(mut guard) = ring_worker.lock() {
                                    guard.render(out, prefill, usize::from(target_channels));
                                } else {
                                    out.fill(<$sample as cpal::Sample>::EQUILIBRIUM);
                                }
                            },
                            err_cb,
                            None,
                        )
                        .map_err(|e| format!("failed to open {sample_format:?} audio output: {e}"))
                };
            }
            let build_res: Result<Stream, String> = match sample_format {
                SampleFormat::F32 => output_stream!(f32),
                SampleFormat::I16 => output_stream!(i16),
                SampleFormat::U16 => output_stream!(u16),
                SampleFormat::I32 => output_stream!(i32),
                other => Err(format!("unsupported playback sample format: {other:?}")),
            };

            let stream = match build_res {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            if let Err(e) = stream.play() {
                let _ = init_tx.send(Err(format!("failed to start audio playback: {e}")));
                return;
            }

            let _ = init_tx.send(Ok((target_rate, target_channels)));

            while !stop_worker.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            drop(stream);
        })
        .map_err(|e| format!("failed to spawn audio playback thread: {e}"))?;

    match init_rx.recv() {
        Ok(Ok((target_rate, target_channels))) => Ok(AudioPlaybackHandle {
            stop_flag,
            ring,
            thread: Some(thread),
            target_rate,
            target_channels,
        }),
        Ok(Err(e)) => {
            stop_flag.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop_flag.store(true, Ordering::Relaxed);
            let _ = thread.join();
            Err("audio playback thread exited unexpectedly".to_string())
        }
    }
}

struct HostAudioProcessor {
    src_rate: u32,
    src_channels: usize,
    dst_channels: usize,
    source_is_loopback: bool,
    denoisers: Vec<Box<DenoiseState<'static>>>,
    denoised_first: Vec<bool>,
    channel_queues: Vec<VecDeque<f32>>,
    in_scratch: Vec<f32>,
    out_scratch: Vec<f32>,
}

impl HostAudioProcessor {
    fn new(
        src_rate: u32,
        src_channels: usize,
        dst_channels: usize,
        source_is_loopback: bool,
    ) -> Self {
        let ch = dst_channels.max(1);
        Self {
            src_rate,
            src_channels: src_channels.max(1),
            dst_channels: ch,
            source_is_loopback,
            denoisers: (0..ch).map(|_| DenoiseState::new()).collect(),
            denoised_first: vec![true; ch],
            channel_queues: (0..ch).map(|_| VecDeque::with_capacity(4096)).collect(),
            in_scratch: vec![0.0; RNNOISE_FRAME_LEN],
            out_scratch: vec![0.0; RNNOISE_FRAME_LEN],
        }
    }

    fn push_and_emit(
        &mut self,
        interleaved: &[f32],
        rnnoise_enabled: bool,
        seq_counter: &AtomicU32,
        audio_tx: &mpsc::Sender<AudioPacket>,
    ) {
        let remixed = remix_channels(interleaved, self.src_channels, self.dst_channels);
        let at_48k = resample_cubic_interleaved(
            &remixed,
            self.src_rate,
            RNNOISE_SAMPLE_RATE,
            self.dst_channels,
        );
        let frames = at_48k.len() / self.dst_channels;
        for f in 0..frames {
            for ch in 0..self.dst_channels {
                self.channel_queues[ch].push_back(at_48k[f * self.dst_channels + ch]);
            }
        }

        let pkt_frames = if self.dst_channels == 1 {
            PACKET_FRAME_COUNT_MONO
        } else {
            PACKET_FRAME_COUNT_STEREO
        };

        let apply_rnnoise = rnnoise_enabled && !self.source_is_loopback;

        while self
            .channel_queues
            .iter()
            .all(|q| q.len() >= RNNOISE_FRAME_LEN)
        {
            let mut denoised_channels: Vec<Vec<f32>> =
                vec![Vec::with_capacity(RNNOISE_FRAME_LEN); self.dst_channels];

            for (ch, channel_out) in denoised_channels
                .iter_mut()
                .enumerate()
                .take(self.dst_channels)
            {
                for sample_slot in &mut self.in_scratch[..RNNOISE_FRAME_LEN] {
                    let s = self.channel_queues[ch].pop_front().unwrap_or(0.0);
                    *sample_slot = (s * 32767.0).clamp(-32768.0, 32767.0);
                }
                if apply_rnnoise {
                    self.denoisers[ch].process_frame(&mut self.out_scratch, &self.in_scratch);
                    if self.denoised_first[ch] {
                        self.denoised_first[ch] = false;
                        channel_out.resize(RNNOISE_FRAME_LEN, 0.0);
                    } else {
                        for &s in &self.out_scratch {
                            channel_out.push((s / 32767.0).clamp(-1.0, 1.0));
                        }
                    }
                } else {
                    for &s in &self.in_scratch {
                        channel_out.push((s / 32767.0).clamp(-1.0, 1.0));
                    }
                }
            }

            let mut offset = 0;
            while offset + pkt_frames <= RNNOISE_FRAME_LEN {
                let mut interleaved_out = Vec::with_capacity(pkt_frames * self.dst_channels);
                for i in offset..offset + pkt_frames {
                    for ch_vec in &denoised_channels {
                        interleaved_out.push(ch_vec[i]);
                    }
                }
                let mut flags = if self.source_is_loopback {
                    audio_flags::SYSTEM_TRACK
                } else {
                    audio_flags::VOICE_TRACK
                };
                if apply_rnnoise {
                    flags |= audio_flags::DENOISED;
                }
                let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                let pkt = AudioPacket {
                    header: AudioPacketHeader {
                        codec: AudioCodec::PcmI16 as u8,
                        flags,
                        channels: self.dst_channels as u8,
                        seq,
                        timestamp_us: now_unix_us(),
                    },
                    payload: f32_slice_to_i16_bytes(&interleaved_out),
                };
                let _ = audio_tx.try_send(pkt);
                offset += pkt_frames;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_primes_and_recovers_after_a_gap() {
        let mut buffer = PlaybackBuffer::new();
        let mut out = [0.0f32; 4]; // 2 frames * 2 channels
        buffer.push(u32::MAX, vec![0.5; 4], 24);
        buffer.render(&mut out, 4, 2);
        assert_eq!(out, [0.0; 4]);
        buffer.push(0, vec![0.5; 4], 24);
        buffer.render(&mut out, 4, 2);
        assert_eq!(out, [0.5; 4]);
        buffer.push(u32::MAX, vec![-0.5; 4], 24);
        buffer.push(2, vec![0.25; 4], 24);
        buffer.render(&mut out, 4, 2);
        assert!(out.iter().all(|sample| (sample - 0.5).abs() < 0.001));
        buffer.render(&mut out, 4, 2);
        // After gap concealment, output blends toward next packet (0.25)
        assert!(out.iter().all(|sample| sample.abs() < 0.6));
        // Drain remaining and fade to silence
        for _ in 0..200 {
            buffer.render(&mut out, 4, 2);
        }
        assert!(out.iter().all(|sample| sample.abs() < 0.001));
        buffer.push(30, vec![1.0; 4], 24);
        buffer.render(&mut out, 4, 2);
        assert!(out.iter().all(|s| s.abs() < 0.001));
        buffer.push(31, vec![1.0; 4], 24);
        buffer.render(&mut out, 4, 2);
        assert_eq!(out, [1.0; 4]);
    }

    #[test]
    fn reordered_audio_is_played_in_sequence_without_inserting_silence() {
        let mut buffer = PlaybackBuffer::new();
        buffer.push(100, vec![1.0; 4], 24);
        buffer.push(102, vec![3.0; 4], 24);
        buffer.push(101, vec![2.0; 4], 24);
        assert_eq!(
            buffer.samples.into_iter().collect::<Vec<_>>(),
            [1.0; 4]
                .into_iter()
                .chain([2.0; 4])
                .chain([3.0; 4])
                .collect::<Vec<_>>()
        );
    }
}
