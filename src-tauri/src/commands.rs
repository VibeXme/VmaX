use crate::audio::{
    AudioCaptureHandle, AudioPlaybackHandle, start_client_audio_playback, start_host_audio_capture,
};
use crate::bitrate::FramePacing;
#[cfg(target_os = "windows")]
use crate::capture::fit_resolution;
use crate::capture::{
    HardwareTelemetryPayload, latest_decode_latency_ms, parse_resolution_ceiling,
    record_decode_latency_us, record_encode_latency_us, record_network_rtt_us,
    sample_cpu_load_pct, set_constant_fps,
};
use crate::connections::{
    SavedConnections, SavedDevice, normalize_endpoint, normalize_name, read_json, write_json,
};
use crate::encoder::EncoderModePreference;
#[cfg(target_os = "windows")]
use crate::input::HeldRemoteInputs;
#[cfg(target_os = "windows")]
use crate::input::gamepad::HostGamepad;
use crate::input::gamepad_client::ClientGamepad;
use crate::input::touch_gamepad::{TouchGamepad, TouchGamepadInput};
use crate::input::{FrontendInputPayload, build_input_packet};
use crate::network::{
    ConnectionPhase, NetworkConfig, NetworkEvent, NetworkTelemetry, OutboundVideoFrame,
    SessionHandle, SessionRole, SessionTx, TransportProtocol,
    start_client_session as net_start_client, start_host_session as net_start_host,
    start_saved_client_session as net_start_saved_client,
};
use crate::protocol::{
    AudioCodec, AudioPacket, AudioPacketHeader, ControlMessage, InputEvent, InputPacket,
    VideoCodec, audio_flags,
};
use crate::signaling::{DiscoveredHost, LocalSignalingServer, RoomSummary};
use crate::stun::discover_primary_lan_ip;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Manager;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;
use url::Url;

fn default_protocol() -> String {
    "quic".to_string()
}

/// Smallest fraction of the configured pixel count the smoothness guard may
/// use. Half resolution is the floor: below that the picture stops reading as
/// "native" for any resolution worth streaming.
const MIN_ENCODE_FPS_SCALE: f32 = 0.5;
/// Coarse step per adjustment, so the guard converges in a few steps instead
/// of oscillating around the budget.
const ENCODE_FPS_SCALE_STEP: f32 = 0.15;

fn default_resolution() -> String {
    "1440p".to_string()
}

fn default_framerate() -> u16 {
    60
}

fn default_encoder() -> String {
    "nvenc".to_string()
}

fn default_true() -> bool {
    true
}

fn default_audio_bitrate() -> u16 {
    192
}

fn default_fec_pct() -> u8 {
    10
}

fn default_sharpness() -> u8 {
    35
}

fn default_jitter_window() -> u64 {
    NetworkConfig::default().jitter_window_ms
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSessionParams {
    pub stream_computer_audio: Option<bool>,
    pub audio_output_id: Option<String>,
    pub audio_process_id: Option<u32>,
    #[serde(default)]
    pub exclude_discord_audio: bool,
    pub capture_display: Option<String>,
    #[serde(default)]
    pub host_only_input: bool,
    #[serde(default)]
    pub guest_controller_only: bool,
    pub signaling_url: String,
    pub enable_stun: bool,
    #[serde(default)]
    pub relay_only: bool,
    pub max_bitrate_mbps: u32,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_resolution")]
    pub resolution: String,
    #[serde(default = "default_framerate")]
    pub framerate: u16,
    #[serde(default)]
    pub frame_pacing: FramePacing,
    #[serde(default = "default_jitter_window")]
    pub jitter_window_ms: u64,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub chroma: String,
    #[serde(default = "default_true")]
    pub ai_noise_suppression: bool,
    #[serde(default = "default_audio_bitrate")]
    pub audio_bitrate_kbps: u16,
    #[serde(default = "default_fec_pct")]
    pub fec_redundancy_pct: u8,
    #[serde(default = "default_true")]
    pub hardware_cursor: bool,
    #[serde(default = "default_sharpness")]
    pub sharpness: u8,
    #[serde(default)]
    pub upscaling: String,
    /// Stream at the host's configured resolution regardless of bandwidth.
    #[serde(default = "default_true")]
    pub lock_resolution: bool,
    /// Keep emitting frames when the desktop is static.
    #[serde(default = "default_true")]
    pub constant_fps: bool,
    /// Ignore congestion-driven bitrate reductions and use the whole link.
    #[serde(default = "default_true")]
    pub unlimited_bitrate: bool,
    #[serde(default)]
    pub nvenc: crate::nvenc::NvencSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRelayStatus {
    pub running: bool,
    pub ws_url: Option<String>,
    pub lan_ws_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacketLogEntry {
    pub ts_ms: u64,
    pub channel: String,
    pub direction: String,
    pub summary: String,
}

/// Parsec-exact top bar telemetry payload emitted on `vmax://parsec-telemetry`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsecTelemetryPayload {
    pub decode_ms: f32,
    pub encode_ms: f32,
    pub network_ms: f32,
    pub bitrate_mbps: f32,
    pub pipeline_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSettingsPayload {
    pub resolution: String,
    pub framerate: u16,
    pub bitrate_mbps: u32,
    pub encoder: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub chroma: String,
    #[serde(default)]
    pub sharpness: u8,
    #[serde(default)]
    pub upscaling: String,
    #[serde(default)]
    pub renderer: String,
    #[serde(default)]
    pub frame_pacing: FramePacing,
    /// Never shrink the encoded resolution below what the host configured.
    #[serde(default = "default_true")]
    pub lock_resolution: bool,
    /// Constant FPS: keep sending frames even when the desktop is static.
    #[serde(default = "default_true")]
    pub constant_fps: bool,
    /// Use the full configured bitrate and ignore congestion back-off.
    #[serde(default = "default_true")]
    pub unlimited_bitrate: bool,
    #[serde(default)]
    pub nvenc: crate::nvenc::NvencSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSettingsPayload {
    pub ai_noise_suppression: bool,
    pub bitrate_kbps: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettingsPayload {
    pub protocol: String,
    pub fec_redundancy_pct: u8,
    pub hardware_cursor: bool,
    #[serde(default = "default_jitter_window")]
    pub packet_window_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomizationSettingsPayload {
    pub stats_overlay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmaXSettingsPayload {
    pub video: VideoSettingsPayload,
    pub audio: AudioSettingsPayload,
    pub network: NetworkSettingsPayload,
    pub customization: CustomizationSettingsPayload,
}

impl Default for VmaXSettingsPayload {
    fn default() -> Self {
        Self {
            video: VideoSettingsPayload {
                // Match the frontend defaults so a session started without a
                // settings payload still negotiates 1080p60 at a ceiling every
                // client class can carry, instead of 4K at 40 Mbps.
                resolution: "1080p".to_string(),
                framerate: 60,
                bitrate_mbps: 20,
                encoder: "hardware".to_string(),
                codec: "auto".to_string(),
                chroma: "420".to_string(),
                sharpness: default_sharpness(),
                upscaling: "off".to_string(),
                renderer: "auto".to_string(),
                frame_pacing: FramePacing::default(),
                lock_resolution: true,
                constant_fps: true,
                // Adaptive by default: the ceiling is still the user's value,
                // but congestion feedback may lower it so a weak link stutters
                // less instead of drowning in retransmission-free datagram loss.
                unlimited_bitrate: false,
                nvenc: Default::default(),
            },
            audio: AudioSettingsPayload {
                ai_noise_suppression: true,
                bitrate_kbps: 128,
            },
            network: NetworkSettingsPayload {
                protocol: "quic".to_string(),
                fec_redundancy_pct: 10,
                packet_window_ms: default_jitter_window(),
                hardware_cursor: true,
            },
            customization: CustomizationSettingsPayload {
                stats_overlay: true,
            },
        }
    }
}

#[derive(Clone, Default)]
struct HostRuntimeControls {
    guest_input_mask: Arc<AtomicU8>,
    host_only_input: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    desktop_bounds: Arc<StdMutex<Option<windows::Win32::Foundation::RECT>>>,
    connected: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    force_intra_refresh: Arc<AtomicU32>,
    target_bitrate_bps: Arc<AtomicU32>,
    desired_max_w: Arc<AtomicU32>,
    desired_max_h: Arc<AtomicU32>,
    desired_fps: Arc<AtomicU16>,
    desired_encoder_mode: Arc<AtomicU8>,
    sharpness: Arc<AtomicU8>,
    upscaling: Arc<AtomicU8>,
    pending_reconfigure: Arc<AtomicBool>,
    pending_codecs: Arc<StdMutex<Option<Vec<VideoCodec>>>>,
    client_display: Arc<StdMutex<Option<crate::handshake::ClientDisplay>>>,
}

const GUEST_KEYBOARD: u8 = 1;
const GUEST_MOUSE: u8 = 2;
const GUEST_CONTROLLER: u8 = 4;
const GUEST_ALL: u8 = GUEST_KEYBOARD | GUEST_MOUSE | GUEST_CONTROLLER;

fn guest_input_category(event: &InputEvent) -> u8 {
    match event {
        InputEvent::Key { .. } | InputEvent::Unicode { .. } => GUEST_KEYBOARD,
        InputEvent::GamepadState { .. } => GUEST_CONTROLLER,
        InputEvent::MouseMoveRel { .. }
        | InputEvent::MouseMoveAbs { .. }
        | InputEvent::MouseButton { .. }
        | InputEvent::MouseWheel { .. }
        | InputEvent::TouchPointer { .. } => GUEST_MOUSE,
    }
}

impl HostRuntimeControls {
    fn effective_guest_input(&self) -> u8 {
        if self.host_only_input.load(Ordering::Acquire) {
            0
        } else {
            self.guest_input_mask.load(Ordering::Acquire)
        }
    }
}

fn encoder_mode_to_u8(mode: EncoderModePreference) -> u8 {
    match mode {
        EncoderModePreference::Hardware => 0,
        EncoderModePreference::Software => 1,
    }
}

fn encoder_mode_from_u8(val: u8) -> EncoderModePreference {
    match val {
        1 => EncoderModePreference::Software,
        _ => EncoderModePreference::Hardware,
    }
}

fn protocol_label(protocol: TransportProtocol) -> &'static str {
    match protocol {
        TransportProtocol::Quic => "QUIC",
        TransportProtocol::RawUdp => "Raw UDP",
    }
}

#[inline]
fn round_2dp(val: f32) -> f32 {
    (val * 100.0).round() / 100.0
}

struct ActivePipelineHandles {
    stop_flag: Arc<AtomicBool>,
    capture_thread: Option<std::thread::JoinHandle<()>>,
    audio_capture: Option<AudioCaptureHandle>,
    audio_playback: Arc<StdMutex<Option<AudioPlaybackHandle>>>,
    host_controls: Option<HostRuntimeControls>,
}

impl ActivePipelineHandles {
    fn shutdown(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        if let Some(audio) = self.audio_capture.take() {
            audio.stop();
        }
        if let Ok(mut guard) = self.audio_playback.lock()
            && let Some(playback) = guard.take()
        {
            playback.stop();
        }
    }
}

#[derive(Clone)]
struct LastHostConfig {
    params: StartSessionParams,
    saved_only: Option<bool>,
    room_code: Option<String>,
}

pub struct AppState {
    haptics: Arc<crate::haptics::HapticsConfig>,
    #[cfg(target_os = "windows")]
    internet: Mutex<Option<crate::internet::InternetTunnel>>,
    pub active_tx: Arc<Mutex<Option<SessionTx>>>,
    pub telemetry: Arc<Mutex<Option<Arc<Mutex<NetworkTelemetry>>>>>,
    pub local_relay: Arc<Mutex<Option<LocalSignalingServer>>>,
    pub seq_counter: Arc<AtomicU64>,
    stream_input_paused: Arc<AtomicBool>,
    touch_gamepad: Arc<TouchGamepad>,
    session_lifecycle: Mutex<()>,
    active_forwarder: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    video_channel: Arc<StdMutex<Option<Channel<InvokeResponseBody>>>>,
    client_codecs: Arc<StdMutex<Vec<VideoCodec>>>,
    client_display: Arc<StdMutex<Option<crate::handshake::ClientDisplay>>>,
    audio_denoise_flag: Arc<AtomicBool>,
    runtime_settings: Arc<StdMutex<VmaXSettingsPayload>>,
    active_pipeline: Arc<StdMutex<Option<ActivePipelineHandles>>>,
    host_auto_rehost: Arc<AtomicBool>,
    last_host_config: Arc<StdMutex<Option<LastHostConfig>>>,
}

impl AppState {
    pub fn stop_internet_on_exit(&self) {
        self.host_auto_rehost.store(false, Ordering::Release);
        #[cfg(target_os = "windows")]
        if let Ok(mut tunnel) = self.internet.try_lock() {
            *tunnel = None;
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            #[cfg(target_os = "windows")]
            internet: Mutex::new(None),
            haptics: Arc::new(crate::haptics::HapticsConfig::default()),
            active_tx: Arc::new(Mutex::new(None)),
            telemetry: Arc::new(Mutex::new(None)),
            local_relay: Arc::new(Mutex::new(None)),
            seq_counter: Arc::new(AtomicU64::new(1)),
            stream_input_paused: Arc::new(AtomicBool::new(false)),
            touch_gamepad: Arc::new(TouchGamepad::default()),
            session_lifecycle: Mutex::new(()),
            active_forwarder: Mutex::new(None),
            video_channel: Arc::new(StdMutex::new(None)),
            client_codecs: Arc::new(StdMutex::new(vec![VideoCodec::H264])),
            client_display: Arc::default(),
            audio_denoise_flag: Arc::new(AtomicBool::new(true)),
            runtime_settings: Arc::new(StdMutex::new(VmaXSettingsPayload::default())),
            active_pipeline: Arc::new(StdMutex::new(None)),
            host_auto_rehost: Arc::new(AtomicBool::new(false)),
            last_host_config: Arc::new(StdMutex::new(None)),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn emit_packet_log(app: &AppHandle, channel: &str, direction: &str, summary: String) {
    let _ = app.emit(
        "vmax://packet-log",
        PacketLogEntry {
            ts_ms: now_ms(),
            channel: channel.to_string(),
            direction: direction.to_string(),
            summary,
        },
    );
}

fn parse_transport_protocol(raw: &str) -> TransportProtocol {
    match raw.trim().to_ascii_lowercase().as_str() {
        "raw_udp" | "raw-udp" | "udp" => TransportProtocol::RawUdp,
        _ => TransportProtocol::Quic,
    }
}

fn parse_encoder_mode(raw: &str) -> EncoderModePreference {
    match raw.trim().to_ascii_lowercase().as_str() {
        "software" | "cpu" => EncoderModePreference::Software,
        _ => EncoderModePreference::Hardware,
    }
}

fn map_fec_group_size(redundancy_pct: u8) -> u8 {
    if redundancy_pct == 0 {
        0
    } else {
        (100u16 / u16::from(redundancy_pct.max(1))).clamp(4, 24) as u8
    }
}

fn build_network_config(effective_signaling: String, params: &StartSessionParams) -> NetworkConfig {
    let max_bps = params.max_bitrate_mbps.clamp(1, 200) * 1_000_000;
    NetworkConfig {
        signaling_url: effective_signaling,
        enable_stun: params.enable_stun,
        protocol: parse_transport_protocol(&params.protocol),
        target_fps: params.framerate.clamp(30, 144),
        jitter_window_ms: params.jitter_window_ms.clamp(8, 80),
        max_bitrate_bps: max_bps,
        initial_bitrate_bps: if params.unlimited_bitrate {
            max_bps
        } else {
            (max_bps / 2).max(crate::bitrate::MIN_VIDEO_BITRATE_BPS)
        },
        fixed_bitrate: params.unlimited_bitrate,
        fec_group_size: map_fec_group_size(params.fec_redundancy_pct),
        ..Default::default()
    }
}

/// Serializes a reassembled video frame with a fixed 16-byte header for zero-overhead
/// binary IPC delivery into WebCodecs (`[u32 frame_id BE][u64 ts_us BE][u8 codec][u8 keyframe][u16 host_encode_us_div10 BE]`).
fn build_video_ipc_frame(
    frame_id: u32,
    timestamp_us: u64,
    codec: u8,
    is_keyframe: bool,
    payload: &[u8],
    host_encode_us: u32,
) -> Vec<u8> {
    let encode_us_div10 = (host_encode_us / 10).min(u32::from(u16::MAX)) as u16;
    let mut buf = Vec::with_capacity(16 + payload.len());
    buf.extend_from_slice(&frame_id.to_be_bytes());
    buf.extend_from_slice(&timestamp_us.to_be_bytes());
    buf.push(codec);
    buf.push(u8::from(is_keyframe));
    buf.extend_from_slice(&encode_us_div10.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Called by the WebCodecs / PC client renderer after each frame is decoded to record
/// the exact Parsec-style `Decode 5.50ms` stopwatch measurement.
#[tauri::command]
pub fn report_client_decode_latency(decode_us: u32) -> f32 {
    round_2dp(record_decode_latency_us(decode_us))
}

/// Toggles Parsec-style `Constant FPS` mode (when false, static desktops drop to `0.06Mbps`).
#[tauri::command]
pub fn set_constant_fps_mode(enabled: bool) {
    set_constant_fps(enabled);
}

#[cfg(target_os = "windows")]
struct HighResTimerGuard;

#[cfg(target_os = "windows")]
impl HighResTimerGuard {
    fn new() -> Self {
        unsafe {
            windows::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

#[cfg(target_os = "windows")]
impl Drop for HighResTimerGuard {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

#[cfg(target_os = "windows")]
struct HostPriorityGuard(windows::Win32::System::Threading::PROCESS_CREATION_FLAGS);

#[cfg(target_os = "windows")]
impl HostPriorityGuard {
    fn raise() -> Option<Self> {
        use windows::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess,
            GetPriorityClass, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
            PROCESS_CREATION_FLAGS, SetPriorityClass,
        };
        let process = unsafe { GetCurrentProcess() };
        let previous = PROCESS_CREATION_FLAGS(unsafe { GetPriorityClass(process) });
        if !matches!(
            previous,
            NORMAL_PRIORITY_CLASS
                | ABOVE_NORMAL_PRIORITY_CLASS
                | BELOW_NORMAL_PRIORITY_CLASS
                | IDLE_PRIORITY_CLASS
        ) {
            return None;
        }
        unsafe { SetPriorityClass(process, HIGH_PRIORITY_CLASS) }.ok()?;
        Some(Self(previous))
    }
}

#[cfg(target_os = "windows")]
impl Drop for HostPriorityGuard {
    fn drop(&mut self) {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetPriorityClass, HIGH_PRIORITY_CLASS, SetPriorityClass,
        };
        let process = unsafe { GetCurrentProcess() };
        if unsafe { GetPriorityClass(process) } == HIGH_PRIORITY_CLASS.0 {
            let _ = unsafe { SetPriorityClass(process, self.0) };
        }
    }
}

#[cfg(target_os = "windows")]
async fn spawn_host_capture_thread(
    app: AppHandle,
    params: StartSessionParams,
    video_tx: tokio::sync::mpsc::Sender<OutboundVideoFrame>,
    controls: HostRuntimeControls,
    stop_flag: Arc<AtomicBool>,
    wake_display: bool,
) -> Result<std::thread::JoinHandle<()>, String> {
    use crate::bitrate::CaptureCadence;
    use crate::capture::win::DxgiCapturer;
    use crate::encoder::win::create_encoder;
    use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_NOSOCKET, MFStartup};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
    let max_bps = controls.target_bitrate_bps.load(Ordering::Relaxed);
    let mut encoder_mode =
        encoder_mode_from_u8(controls.desired_encoder_mode.load(Ordering::Relaxed));
    let active_max_w = controls.desired_max_w.load(Ordering::Relaxed);
    let active_max_h = controls.desired_max_h.load(Ordering::Relaxed);
    let mut target_fps = controls.desired_fps.load(Ordering::Relaxed).clamp(30, 144);
    let frame_pacing = params.frame_pacing;
    let nvenc = params.nvenc;
    let preferred_codec = match params.codec.as_str() {
        "h264" => Some(VideoCodec::H264),
        "hevc" => Some(VideoCodec::Hevc),
        "hevc_main10" => Some(VideoCodec::HevcMain10),
        _ => None,
    };
    let full_chroma = params.chroma == "444";
    let prioritize_codec = move |codecs: &mut Vec<VideoCodec>| {
        if !full_chroma {
            codecs.retain(|codec| *codec != VideoCodec::Hevc444);
        } else if let Some(index) = codecs.iter().position(|codec| *codec == VideoCodec::Hevc444) {
            codecs[..=index].rotate_right(1);
            return;
        }
        if let Some(index) = codecs.iter().position(|&codec| Some(codec) == preferred_codec) {
            codecs[..=index].rotate_right(1);
        }
    };

    let app_thread = app.clone();
    let thread = std::thread::Builder::new()
        .name("vmax-host-capture".to_string())
        .spawn(move || {
            struct ComMfGuard {
                com: bool,
                mf: bool,
            }
            impl Drop for ComMfGuard {
                fn drop(&mut self) {
                    if self.mf {
                        let _ = unsafe { windows::Win32::Media::MediaFoundation::MFShutdown() };
                    }
                    if self.com {
                        unsafe { CoUninitialize() };
                    }
                }
            }

            let mut init_tx = Some(init_tx);
            let _timer_guard = HighResTimerGuard::new();
            let _priority_guard = HostPriorityGuard::raise();
            let _com_mf_guard = ComMfGuard {
                com: unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() },
                mf: unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).is_ok() },
            };

            let selected_display = params.capture_display.as_deref();
            let mut capturer = Some(
                match DxgiCapturer::new(active_max_w, active_max_h, selected_display) {
                    Ok(c) => c,
                    Err(err) => {
                        if let Some(tx) = init_tx.take() {
                            let _ = tx.send(Err(err));
                        }
                        return;
                    }
                },
            );
            if let (Some(cap), Ok(mut bounds)) = (capturer.as_ref(), controls.desktop_bounds.lock())
            {
                *bounds = Some(cap.desktop_bounds);
            }

            let mut preferred_codecs = vec![VideoCodec::H264];
            let mut current_bitrate = max_bps;
            let mut next_bitrate_retry = Instant::now();
            let mut forced_refresh_frames = 0;
            let mut encoder = {
                let cap_ref = match capturer.as_ref() {
                    Some(c) => c,
                    None => return,
                };
                Some(
                    match create_encoder(
                        cap_ref,
                        encoder_mode,
                        &preferred_codecs,
                        target_fps,
                        current_bitrate,
                        frame_pacing,
                        nvenc,
                    ) {
                        Ok(enc) => enc,
                        Err(err) => {
                            if let Some(tx) = init_tx.take() {
                                let _ = tx.send(Err(err));
                            }
                            return;
                        }
                    },
                )
            };

            if let Some(enc_ref) = encoder.as_ref() {
                let label = enc_ref.pipeline_label().to_string();
                if let Some(tx) = init_tx.take() {
                    let _ = tx.send(Ok(label));
                }
            }

            if wake_display {
                use windows::Win32::System::Power::{
                    ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
                };
                use windows::Win32::UI::Input::KeyboardAndMouse::{
                    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
                };
                let wake = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dwFlags: MOUSEEVENTF_MOVE,
                            ..Default::default()
                        },
                    },
                };
                unsafe {
                    SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
                    SendInput(&[wake], std::mem::size_of::<INPUT>() as i32);
                }
            }
            let start_time = Instant::now();
            let mut frame_id: u32 = 1;
            let mut cpu_sample = None;
            let mut encoded_frames: u32 = 0;
            let mut ema_encode_ms: f32 = 0.0;
            let mut last_telemetry = Instant::now();
            // EMA-smoothed values behind `vmax://telemetry-tick`.
            let mut telemetry = crate::telemetry::TelemetryCore::new();
            let mut pacing = crate::pacing::FrameCadence::new(Instant::now(), target_fps);
            let mut cadence = CaptureCadence::default();
            let mut adaptive_resolution = crate::bitrate::CaptureResolution::default();
            let mut active_ceiling = (active_max_w, active_max_h);
            let mut applied_sharpness = 0u8;
            // Smoothness guard state: encode-throughput scaling with hysteresis.
            let mut encode_fps_scale: f32 = 1.0;
            let mut last_scale_change = Instant::now();
            let mut over_budget_since: Option<Instant> = None;
            let mut under_budget_since: Option<Instant> = None;

            let mut capture_errors = 0;
            let mut capture_waiting_since: Option<Instant> = None;
            let mut encode_errors = 0;
            let mut encoder_waiting_since: Option<Instant> = None;
            let mut retry_capture = false;
            let mut retry_delay = Duration::from_millis(15);
            let mut next_retry = Instant::now();

            while !stop_flag.load(Ordering::Relaxed) {
                if retry_capture && Instant::now() < next_retry {
                    std::thread::sleep(
                        next_retry
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(15)),
                    );
                    continue;
                }
                let desired_bps = controls.target_bitrate_bps.load(Ordering::Relaxed);
                let max_w = controls.desired_max_w.load(Ordering::Relaxed);
                let max_h = controls.desired_max_h.load(Ordering::Relaxed);
                let max_fps = controls.desired_fps.load(Ordering::Relaxed).clamp(30, 144);
                let constant_fps = crate::capture::is_constant_fps();
                let (next_max_w, next_max_h, next_fps) = controls
                    .client_display
                    .lock()
                    .ok()
                    .and_then(|display| *display)
                    .map_or((max_w, max_h, max_fps), |display| {
                        let (width, height, display_fps) = display.constrain(max_w, max_h, max_fps);
                        // The client's refresh rate is the rate it can actually
                        // present. Encoding above it only wastes bandwidth and
                        // makes the presenter discard every second picture, so
                        // the cadence follows the panel in both modes.
                        (width, height, display_fps)
                    });
                let now = Instant::now();
                let pace_fps = if constant_fps {
                    // Constant FPS holds the configured rate; the bitrate
                    // budget never changes the frame cadence.
                    next_fps
                } else {
                    cadence.fps(now, desired_bps, next_fps, frame_pacing)
                };
                let (next_max_w, next_max_h) = if crate::capture::is_lock_resolution() {
                    // Resolution is fixed by settings: never downscale for
                    // bandwidth, keep encoding the host's configured size.
                    (next_max_w, next_max_h)
                } else {
                    adaptive_resolution.ceiling(
                        now,
                        desired_bps,
                        pace_fps,
                        next_max_w,
                        next_max_h,
                        frame_pacing,
                    )
                };
                // Smoothness guard: when this host cannot encode the configured
                // pixel count at the configured cadence, frame rate wins and
                // resolution gives way. Both settings paths (locked and
                // adaptive) end up here, because a stutter is never acceptable
                // on any device class. Steps are coarse and rate-limited so a
                // reconfigure can never turn into churn.
                let frame_budget_ms = 1000.0 / f32::from(pace_fps.max(1));
                let over_budget = ema_encode_ms > frame_budget_ms;
                let well_under = ema_encode_ms > 0.0 && ema_encode_ms < frame_budget_ms * 0.55;
                if over_budget {
                    over_budget_since.get_or_insert(now);
                    under_budget_since = None;
                } else if well_under {
                    over_budget_since = None;
                    under_budget_since.get_or_insert(now);
                } else {
                    over_budget_since = None;
                    under_budget_since = None;
                }
                let since_change = now.saturating_duration_since(last_scale_change);
                let shrank = over_budget_since.is_some_and(|since| {
                    now.saturating_duration_since(since) >= Duration::from_secs(2)
                }) && since_change >= Duration::from_secs(4)
                    && encode_fps_scale > MIN_ENCODE_FPS_SCALE;
                let grew = under_budget_since.is_some_and(|since| {
                    now.saturating_duration_since(since) >= Duration::from_secs(8)
                }) && since_change >= Duration::from_secs(6)
                    && encode_fps_scale < 1.0;
                if shrank || grew {
                    let previous = encode_fps_scale;
                    encode_fps_scale = if shrank {
                        (encode_fps_scale - ENCODE_FPS_SCALE_STEP).max(MIN_ENCODE_FPS_SCALE)
                    } else {
                        (encode_fps_scale + ENCODE_FPS_SCALE_STEP).min(1.0)
                    };
                    last_scale_change = now;
                    over_budget_since = None;
                    under_budget_since = None;
                    emit_packet_log(
                        &app_thread,
                        "VIDEO",
                        "PACING",
                        format!(
                            "Encoder budget guard {:.0}% -> {:.0}% pixels ({:.1} ms encode / {:.1} ms frame)",
                            previous * 100.0,
                            encode_fps_scale * 100.0,
                            ema_encode_ms,
                            frame_budget_ms
                        ),
                    );
                }
                let (next_max_w, next_max_h) = if encode_fps_scale < 0.999 {
                    (
                        ((next_max_w as f32 * encode_fps_scale) as u32 & !1).max(640),
                        ((next_max_h as f32 * encode_fps_scale) as u32 & !1).max(360),
                    )
                } else {
                    (next_max_w, next_max_h)
                };
                let upscaling = crate::quality::UpscalingMode::from_u8(
                    controls.upscaling.load(Ordering::Relaxed),
                );
                // The host encodes exactly the resolution selected in settings
                // (never scaled down by an upscaler). Any reconstruction for a
                // smaller or larger client panel happens on the client.
                let sharpness = crate::quality::effective_sharpness(
                    controls.sharpness.load(Ordering::Relaxed),
                    upscaling,
                );
                if sharpness != applied_sharpness {
                    if let Some(cap) = capturer.as_mut() {
                        cap.set_sharpness(sharpness);
                    }
                    applied_sharpness = sharpness;
                }
                let next_ceiling = (next_max_w, next_max_h);
                let reconfigure_requested = controls
                    .pending_reconfigure
                    .swap(false, Ordering::AcqRel)
                    || active_ceiling != next_ceiling;
                active_ceiling = next_ceiling;
                if retry_capture || reconfigure_requested {
                    let next_mode = encoder_mode_from_u8(
                        controls.desired_encoder_mode.load(Ordering::Relaxed),
                    );

                    let dims_changed = if retry_capture {
                        true
                    } else if let Some(cap_ref) = capturer.as_ref() {
                        let (new_w, new_h) = fit_resolution(
                            cap_ref.native_width,
                            cap_ref.native_height,
                            next_max_w,
                            next_max_h,
                        );
                        new_w != cap_ref.encode_width || new_h != cap_ref.encode_height
                    } else {
                        false
                    };
                    let fps_or_mode_changed = next_fps != target_fps || next_mode != encoder_mode;

                    if dims_changed {
                        drop(encoder.take());
                        drop(capturer.take());
                        match DxgiCapturer::new(next_max_w, next_max_h, selected_display) {
                            Ok(new_cap) => {
                                if let Ok(mut bounds) = controls.desktop_bounds.lock() {
                                    *bounds = Some(new_cap.desktop_bounds);
                                }
                                capturer = Some(new_cap);
                            }
                            Err(err) => {
                                emit_packet_log(
                                    &app_thread,
                                    "CAPTURE",
                                    "ERR",
                                    format!("Capture resize failed: {err}"),
                                );
                                retry_capture = true;
                                next_retry = Instant::now() + retry_delay;
                                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                                continue;
                            }
                        }
                    }

                    if dims_changed || fps_or_mode_changed || encoder.is_none() {
                        target_fps = next_fps;
                        encoder_mode = next_mode;
                        drop(encoder.take());
                        if let Some(cap_ref) = capturer.as_ref() {
                            match create_encoder(
                                cap_ref,
                                encoder_mode,
                                &preferred_codecs,
                                target_fps,
                                current_bitrate,
                                frame_pacing,
                                nvenc,
                            ) {
                                Ok(mut new_enc) => {
                                    new_enc.force_keyframe();
                                    let recovered = retry_capture;
                                    retry_capture = false;
                                    capture_errors = 0;
                                    capture_waiting_since = None;
                                    encode_errors = 0;
                                    encoder_waiting_since = None;
                                    retry_delay = Duration::from_millis(15);
                                    pacing.resync(Instant::now());
                                    emit_packet_log(
                                        &app_thread,
                                        if recovered { "CAPTURE" } else { "VIDEO" },
                                        if recovered { "RECOVER" } else { "RECONF" },
                                        format!(
                                            "{} ({}x{} @ {} FPS)",
                                            new_enc.pipeline_label(),
                                            cap_ref.encode_width,
                                            cap_ref.encode_height,
                                            target_fps
                                        ),
                                    );
                                    encoder = Some(new_enc);
                                }
                                Err(err) => {
                                    emit_packet_log(
                                        &app_thread,
                                        "VIDEO",
                                        "ERR",
                                        format!("Encoder reconfigure failed: {err}"),
                                    );
                                    retry_capture = true;
                                    next_retry = Instant::now() + retry_delay;
                                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                                    continue;
                                }
                            }
                        }
                    }
                }

                let Some(cap) = capturer.as_mut() else {
                    break;
                };
                let Some(enc) = encoder.as_mut() else {
                    break;
                };

                if let Ok(mut pending) = controls.pending_codecs.lock()
                    && let Some(mut new_prefs) = pending.take()
                    && !new_prefs.is_empty()
                {
                    prioritize_codec(&mut new_prefs);
                    let preference_changed = preferred_codecs != new_prefs;
                    preferred_codecs = new_prefs;
                    if preference_changed && preferred_codecs.first() != Some(&enc.codec()) {
                        match create_encoder(
                            cap,
                            encoder_mode,
                            &preferred_codecs,
                            target_fps,
                            current_bitrate,
                            frame_pacing,
                            nvenc,
                        ) {
                            Ok(mut new_enc) => {
                                new_enc.force_keyframe();
                                emit_packet_log(
                                    &app_thread,
                                    "VIDEO",
                                    "ENC",
                                    format!("Switched encoder -> {}", new_enc.pipeline_label()),
                                );
                                encoder = Some(new_enc);
                                continue;
                            }
                            Err(err) => {
                                emit_packet_log(
                                    &app_thread,
                                    "VIDEO",
                                    "WARN",
                                    format!("Codec switch skipped: {err}"),
                                );
                            }
                        }
                    }
                }

                if desired_bps != current_bitrate && now >= next_bitrate_retry {
                    match enc.set_bitrate_bps(desired_bps) {
                        Ok(()) => current_bitrate = desired_bps,
                        Err(error) => {
                            next_bitrate_retry = now + Duration::from_secs(1);
                            tracing::warn!(%error, desired_bps, "Encoder bitrate change rejected");
                        }
                    }
                }

                let force_keyframe = controls.force_idr.swap(false, Ordering::AcqRel);
                if force_keyframe {
                    controls.force_intra_refresh.store(0, Ordering::Release);
                    forced_refresh_frames = 0;
                    enc.force_keyframe();
                } else {
                    let refresh_frames = controls.force_intra_refresh.swap(0, Ordering::AcqRel);
                    if refresh_frames > 0 {
                        forced_refresh_frames = refresh_frames;
                        enc.request_intra_refresh(refresh_frames);
                    }
                }

                if !controls.connected.load(Ordering::Relaxed) {
                    capture_waiting_since = None;
                    encoder_waiting_since = None;
                    std::thread::sleep(Duration::from_millis(50));
                    if last_telemetry.elapsed() >= Duration::from_millis(200) {
                        let cpu = sample_cpu_load_pct(&mut cpu_sample);
                        let _ = app_thread.emit(
                            "vmax://hardware-telemetry",
                            HardwareTelemetryPayload {
                                nvenc_load_pct: 0.0,
                                vram_used_gb: 0.4,
                                vram_total_gb: cap.vram_total_gb,
                                cpu_load_pct: cpu,
                                encode_latency_ms: 0.0,
                                decode_latency_ms: 0.0,
                                fps: 0,
                                pipeline_label: enc.pipeline_label().to_string(),
                            },
                        );
                        last_telemetry = Instant::now();
                    }
                    pacing.resync(Instant::now());
                    continue;
                }

                // Backpressure: when the network worker cannot accept another
                // frame right now, skip this capture slot instead of encoding a
                // frame that can only be dropped. Skipping a slot keeps the
                // reference chain intact; dropping an encoded frame forces an
                // intra-refresh and is what turns a busy link into visible
                // pulsing. The viewer sees a brief lower frame rate instead of
                // an artifact plus a keyframe spike.
                if video_tx.capacity() == 0 {
                    pacing.set_fps(pace_fps);
                    pacing.wait_next_frame();
                    continue;
                }

                let capture_wait_ms = {
                    // Event-driven capture: block only until the next deadline
                    // minus the measured encode cost, so the acquire wakes as
                    // the desktop presents and a missed present still leaves
                    // room to emit the frame on the grid.
                    pacing.set_fps(pace_fps);
                    pacing.capture_wait_ms(Instant::now())
                };
                let valid_frame = match cap.capture_frame(capture_wait_ms, force_keyframe || forced_refresh_frames > 0) {
                    Ok(true) => {
                        capture_errors = 0;
                        capture_waiting_since = None;
                        true
                    }
                    Ok(false) if cap.waiting_for_desktop() => {
                        std::thread::sleep(Duration::from_millis(15));
                        continue;
                    }
                    Ok(false) => {
                        let waiting = capture_waiting_since.get_or_insert_with(Instant::now);
                        if waiting.elapsed() >= Duration::from_secs(2) {
                            drop(encoder.take());
                            drop(capturer.take());
                            retry_capture = true;
                            next_retry = Instant::now() + retry_delay;
                            continue;
                        }
                        false
                    }
                    Err(err) => {
                        capture_errors += 1;
                        if capture_errors == 1 {
                            emit_packet_log(&app_thread, "CAPTURE", "WARN", err);
                        }
                        if cap.needs_recovery() || capture_errors >= 3 {
                            drop(encoder.take());
                            drop(capturer.take());
                            retry_capture = true;
                            next_retry = Instant::now() + retry_delay;
                            let _ = app_thread.emit(
                                "vmax://hardware-telemetry",
                                HardwareTelemetryPayload {
                                    pipeline_label: "Waiting for Windows display".into(),
                                    ..Default::default()
                                },
                            );
                            continue;
                        }
                        false
                    }
                };
                if force_keyframe && !valid_frame {
                    controls.force_idr.store(true, Ordering::Release);
                }
                if valid_frame {
                    if cap.take_refinement_request() {
                        // Motion stopped: two near-lossless frames before the
                        // idle heartbeat thins the stream out.
                        enc.request_refinement(2);
                    }
                    let enc_started = Instant::now();
                    let ts_us = start_time.elapsed().as_micros() as u64;
                    match enc.encode(cap, ts_us) {
                        Ok(Some(au)) => {
                            encode_errors = 0;
                            encoder_waiting_since = None;
                            forced_refresh_frames = forced_refresh_frames.saturating_sub(1);
                            ema_encode_ms = enc.last_encode_latency_ms();
                            encoded_frames += 1;

                            let meta = {
                                let mut meta =
                                    crate::telemetry::FrameTelemetry::full_range_hardware(
                                        crate::nvenc::CodecId::for_codec(au.codec).wire_id(),
                                        cap.encode_width as u16,
                                        cap.encode_height as u16,
                                    );
                                if !enc.hardware_load_hint() {
                                    meta.chroma_and_range &= !crate::telemetry::FLAG_HARDWARE;
                                }
                                if au.codec == VideoCodec::Hevc444 {
                                    meta.chroma_and_range |= crate::telemetry::FLAG_444;
                                }
                                meta.host_encode_us = au.host_encode_us;
                                meta.capture_to_send_us =
                                    enc_started.elapsed().as_micros() as u32;
                                meta
                            };
                            telemetry.on_frame_meta(meta);
                            telemetry.bitrate.add(au.payload.len(), Instant::now());
                            let out = OutboundVideoFrame {
                                meta,
                                frame_id,
                                timestamp_us: ts_us,
                                captured_at: enc_started,
                                fps: pace_fps,
                                codec: au.codec,
                                is_keyframe: au.is_keyframe,
                                payload: Bytes::from(au.payload),
                            };
                            frame_id = frame_id.wrapping_add(1);
                            if video_tx.try_send(out).is_err() {
                                controls.force_intra_refresh.store(5, Ordering::Release);
                            }
                        }
                        Ok(None) => {
                            let waiting = encoder_waiting_since.get_or_insert(enc_started);
                            if waiting.elapsed() >= Duration::from_secs(1) {
                                emit_packet_log(
                                    &app_thread,
                                    "VIDEO",
                                    "WARN",
                                    "Encoder stopped producing frames. Restarting capture.".into(),
                                );
                                encode_errors = 3;
                            }
                        }
                        Err(err) => {
                            encode_errors += 1;
                            if encode_errors == 1 {
                                emit_packet_log(
                                    &app_thread,
                                    "VIDEO",
                                    "WARN",
                                    format!("Encode drop: {err}"),
                                );
                            }
                            enc.force_keyframe();
                        }
                    }
                    // Feed the measured capture+encode cost back into the
                    // cadence so the next capture starts early enough to land
                    // on the deadline on this specific host.
                    pacing.note_encode(enc_started.elapsed());
                    if encode_errors >= 3 {
                        drop(encoder.take());
                        drop(capturer.take());
                        retry_capture = true;
                        next_retry = Instant::now() + retry_delay;
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                        continue;
                    }
                }

                if last_telemetry.elapsed() >= Duration::from_millis(200) {
                    let elapsed_s = last_telemetry.elapsed().as_secs_f32().max(0.001);
                    let fps_now = (encoded_frames as f32 / elapsed_s).round() as u32;
                    encoded_frames = 0;
                    let cpu = sample_cpu_load_pct(&mut cpu_sample);
                    let hw_hint = enc.hardware_load_hint();
                    let gpu_pct = if hw_hint {
                        ((ema_encode_ms / (1000.0 / f32::from(target_fps))) * 48.0).clamp(4.0, 95.0)
                    } else {
                        0.0
                    };
                    if controls.connected.load(Ordering::Relaxed) {
                        let app = app_thread.clone();
                        let micros = (ema_encode_ms * 1000.0) as u32;
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<AppState>();
                            if let Some(tx) = state.active_tx.lock().await.as_ref() {
                                let _ = tx
                                    .control_tx
                                    .try_send(ControlMessage::EncoderLatency { micros });
                            }
                        });
                    }
                    let label = enc.pipeline_label().to_string();
                    let _ = app_thread.emit(
                        "vmax://hardware-telemetry",
                        HardwareTelemetryPayload {
                            nvenc_load_pct: (gpu_pct * 10.0).round() / 10.0,
                            vram_used_gb: if hw_hint { 1.1 } else { 0.4 },
                            vram_total_gb: cap.vram_total_gb,
                            cpu_load_pct: cpu,
                            encode_latency_ms: round_2dp(ema_encode_ms),
                            decode_latency_ms: round_2dp(latest_decode_latency_ms()),
                            fps: fps_now,
                            pipeline_label: label.clone(),
                        },
                    );
                    telemetry.bitrate.add(0, Instant::now());
                    let _ = app_thread.emit(
                        "vmax://parsec-telemetry",
                        ParsecTelemetryPayload {
                            decode_ms: round_2dp(latest_decode_latency_ms()),
                            encode_ms: round_2dp(ema_encode_ms),
                            network_ms: round_2dp(crate::capture::latest_network_latency_ms()),
                            bitrate_mbps: round_2dp(telemetry.bitrate.mbps()),
                            pipeline_label: label,
                        },
                    );
                    telemetry.on_decode((latest_decode_latency_ms() * 1000.0) as u32);
                    telemetry.on_rtt(crate::capture::latest_network_latency_ms() * 2.0);
                    telemetry.idle = cap.is_static_screen() && !crate::capture::is_constant_fps();
                    let _ = app_thread.emit("vmax://telemetry-tick", telemetry.tick());
                    last_telemetry = Instant::now();
                }

                pacing.set_fps(pace_fps);
                pacing.wait_next_frame();
            }
            drop(encoder.take());
            drop(capturer.take());
        })
        .map_err(|e| format!("failed to spawn host capture thread: {e}"))?;

    match init_rx.await {
        Ok(Ok(label)) => {
            emit_packet_log(&app, "CAPTURE", "INIT", format!("Initialized {label}"));
            Ok(thread)
        }
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(_) => {
            let _ = thread.join();
            Err("host capture thread exited before reporting initialization status".to_string())
        }
    }
}

#[cfg(not(target_os = "windows"))]
async fn spawn_host_capture_thread(
    _app: AppHandle,
    _params: StartSessionParams,
    _video_tx: tokio::sync::mpsc::Sender<OutboundVideoFrame>,
    _controls: HostRuntimeControls,
    _stop_flag: Arc<AtomicBool>,
    _wake_display: bool,
) -> Result<std::thread::JoinHandle<()>, String> {
    Err("Host screen hosting is only supported on Windows (DXGI + NVENC/MFT)".to_string())
}

struct EventForwarderContext {
    app: AppHandle,
    role: SessionRole,
    protocol: TransportProtocol,
    video_channel: Arc<StdMutex<Option<Channel<InvokeResponseBody>>>>,
    client_codecs: Arc<StdMutex<Vec<VideoCodec>>>,
    client_display: Arc<StdMutex<Option<crate::handshake::ClientDisplay>>>,
    host_controls: Option<HostRuntimeControls>,
    playback_sink: Option<Arc<StdMutex<Option<AudioPlaybackHandle>>>>,
}

#[derive(Clone, Copy, Serialize)]
pub struct NativeVideoState {
    pub active: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[tauri::command]
pub fn native_video_state() -> NativeVideoState {
    #[cfg(target_os = "android")]
    {
        let (width, height) = match crate::android_jni::video_size() {
            Some((width, height)) => (Some(width), Some(height)),
            None => (None, None),
        };
        NativeVideoState {
            active: crate::android_jni::native_route(),
            width,
            height,
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        NativeVideoState {
            active: false,
            width: None,
            height: None,
        }
    }
}

fn register_session<'a>(
    mut handle: SessionHandle,
    state: &'a AppState,
    ctx: EventForwarderContext,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let session_tx = handle.tx.clone();
        *state.active_tx.lock().await = Some(session_tx.clone());
        *state.telemetry.lock().await = Some(Arc::clone(&handle.telemetry));

        let haptics = state.haptics.clone();
        #[cfg(target_os = "android")]
        let touch = state.touch_gamepad.clone();
        let forwarder = tauri::async_runtime::spawn(async move {
            let mut video_rx_count: u64 = 0;
            let mut audio_rx_count: u64 = 0;
            let mut input_rx_count: u64 = 0;
            #[cfg(target_os = "windows")]
            let mut gamepad: Option<HostGamepad> = None;
            #[cfg(target_os = "windows")]
            let mut reported_port = None;
            #[cfg(target_os = "windows")]
            let mut last_pad_reconnect: Option<Instant> = None;
            #[cfg(target_os = "windows")]
            let mut held_inputs = HeldRemoteInputs::default();
            #[cfg(target_os = "windows")]
            let mut last_input_rejection: Option<Instant> = None;
            #[cfg(target_os = "windows")]
            let remote_taps = (ctx.role == SessionRole::Host).then(|| {
                crate::input::text_focus::watch_remote_taps(session_tx.control_tx.clone())
            });
            let mut client_gamepad: Option<ClientGamepad> = None;
            let mut client_fps_frames: u32 = 0;
            let mut last_client_telemetry = Instant::now();
            let mut client_cpu_sample = None;
            let mut client_metrics = crate::telemetry::TelemetryCore::new();
            if ctx.role == SessionRole::Client {
                crate::capture::reset_client_decode_latency();
            }
            let mut client_has_video = false;
            #[cfg(target_os = "android")]
            let mut native_video_route: Option<bool> = None;
            #[cfg(target_os = "android")]
            let mut last_native_render_count = crate::android_jni::rendered_frames();
            #[cfg(target_os = "android")]
            let mut native_video_size: Option<(u32, u32)> = None;

            use crate::haptics::{self, Rumble};
            let mut rumble_tick = tokio::time::interval(haptics::REFRESH);
            rumble_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut rumble_at: Option<Instant> = None;
            #[cfg(target_os = "windows")]
            let mut last_host_rumble = Rumble::default();

            loop {
                let ev = tokio::select! {
                    event = handle.event_rx.recv() => match event {
                        Some(event) => event,
                        None => break,
                    },
                    _ = rumble_tick.tick() => {
                        let settings = haptics.get();
                        #[cfg(target_os = "windows")]
                        if let Some(host) = ctx.host_controls.as_ref() {
                            let motors = if settings.enabled && host.effective_guest_input() & GUEST_CONTROLLER != 0 {
                                gamepad.as_ref().map_or(Rumble::default(), HostGamepad::rumble).scaled(settings.intensity)
                            } else { Rumble::default() };
                            if motors != Rumble::default() || last_host_rumble != motors {
                                session_tx.haptics_tx.send_replace(motors);
                                last_host_rumble = motors;
                            }
                        }
                        if rumble_at.is_some_and(|at| at.elapsed() >= haptics::LEASE || !settings.enabled || settings.intensity == 0) {
                            #[cfg(any(target_os = "windows", target_os = "linux"))]
                            if let Some(pad) = client_gamepad.as_mut() { pad.rumble(Rumble::default()); }
                            #[cfg(target_os = "android")]
                            haptics::deliver_android(Rumble::default(), false);
                            rumble_at = None;
                        }
                        continue;
                    }
                };
                match ev {
                    NetworkEvent::RumbleReceived(packet) if client_gamepad.is_some() => {
                        let settings = haptics.get();
                        let packet = if settings.enabled {
                            packet.scaled(settings.intensity)
                        } else {
                            Rumble::default()
                        };
                        #[cfg(any(target_os = "windows", target_os = "linux"))]
                        if let Some(pad) = client_gamepad.as_mut() {
                            pad.rumble(packet);
                        }
                        #[cfg(target_os = "android")]
                        haptics::deliver_android(packet, touch.reading().is_some());
                        rumble_at = (packet != Rumble::default()).then(Instant::now);
                    }
                    NetworkEvent::RumbleReceived(_) => {}
                    NetworkEvent::TelemetryUpdated(snapshot) => {
                        // Record one-way Network latency (RTT / 2) in microseconds for Parsec overlay
                        record_network_rtt_us((snapshot.rtt_ms.max(0.0) * 1000.0) as u32);

                        #[cfg(target_os = "windows")]
                        if let Some(host) = ctx.host_controls.as_ref() {
                            let allowed = host.effective_guest_input();
                            let port = if allowed & GUEST_CONTROLLER != 0 {
                                gamepad.as_mut().and_then(HostGamepad::windows_port)
                            } else {
                                None
                            };
                            if port != reported_port
                                && session_tx
                                    .control_tx
                                    .try_send(ControlMessage::GamepadPort { port })
                                    .is_ok()
                            {
                                reported_port = port;
                            }
                            if allowed & GUEST_KEYBOARD == 0 {
                                held_inputs.release_keyboard();
                            }
                            if allowed & GUEST_MOUSE == 0 {
                                held_inputs.release_mouse();
                            }
                            if allowed & GUEST_CONTROLLER == 0
                                && let Some(pad) = gamepad.as_mut()
                            {
                                let _ = pad.neutralize();
                            }
                        }
                        #[cfg(target_os = "windows")]
                        if let Some(gamepad) = gamepad.as_mut()
                            && let Err(err) = gamepad.expire()
                        {
                            emit_packet_log(&ctx.app, "INPUT", "ERROR", err);
                        }
                        if ctx.role == SessionRole::Client && client_has_video {
                            client_metrics.on_rtt(snapshot.rtt_ms as f32);
                            let mut tick = client_metrics.tick();
                            tick.decode_ms = latest_decode_latency_ms();
                            tick.bitrate_mbps = snapshot.media_bitrate_mbps;
                            if snapshot.audio_packets_rx == 0 {
                                tick.audio = "No audio received".into();
                            }
                            let device = if tick.hardware { "Hardware" } else { "Software" };
                            let label = format!(
                                "{device} {} {}x{} {} {} ({})",
                                tick.codec, tick.width, tick.height, tick.chroma, tick.range, tick.audio,
                            );
                            let _ = ctx.app.emit("vmax://parsec-telemetry", ParsecTelemetryPayload {
                                decode_ms: tick.decode_ms,
                                encode_ms: tick.encode_ms,
                                network_ms: tick.network_ms,
                                bitrate_mbps: tick.bitrate_mbps,
                                pipeline_label: label.clone(),
                            });
                            let elapsed = last_client_telemetry.elapsed().as_secs_f32().max(0.001);
                            #[allow(unused_mut)]
                            let mut displayed_frames = client_fps_frames;
                            #[cfg(target_os = "android")]
                            {
                                let count = crate::android_jni::rendered_frames();
                                if native_video_route == Some(true) {
                                    displayed_frames = count.saturating_sub(last_native_render_count)
                                        .min(u64::from(u32::MAX)) as u32;
                                }
                                last_native_render_count = count;
                            }
                            let _ = ctx.app.emit("vmax://hardware-telemetry", HardwareTelemetryPayload {
                                cpu_load_pct: sample_cpu_load_pct(&mut client_cpu_sample),
                                fps: (displayed_frames as f32 / elapsed).round() as u32,
                                decode_latency_ms: tick.decode_ms,
                                encode_latency_ms: tick.encode_ms,
                                pipeline_label: label,
                                ..Default::default()
                            });
                            client_fps_frames = 0;
                            last_client_telemetry = Instant::now();
                            let _ = ctx.app.emit("vmax://telemetry-tick", tick);
                        }
                        let _ = ctx.app.emit("vmax://telemetry", *snapshot);
                    }
                    NetworkEvent::InviteReady { .. } => {
                        emit_packet_log(&ctx.app, "SIGNAL", "LOCAL", "Invite ready".into());
                    }
                    NetworkEvent::PhaseChanged(phase) => {
                        emit_packet_log(
                            &ctx.app,
                            protocol_label(ctx.protocol),
                            "STATE",
                            format!("Phase -> {phase:?}"),
                        );
                        let is_live = phase == ConnectionPhase::Connected;
                        #[cfg(target_os = "windows")]
                        if ctx.role == SessionRole::Host {
                            gamepad = is_live.then(HostGamepad::default);
                            reported_port = None;
                            last_pad_reconnect = None;
                            if !is_live {
                                held_inputs = HeldRemoteInputs::default();
                                if let Some(ref taps) = remote_taps {
                                    taps.send_replace(None);
                                }
                            }
                        }
                        if ctx.role == SessionRole::Client {
                            if !is_live {
                                #[cfg(target_os = "android")]
                                haptics::deliver_android(Rumble::default(), false);
                                rumble_at = None;
                                client_gamepad = None;
                            } else if client_gamepad.is_none() {
                                #[cfg(target_os = "windows")]
                                let window_handle = ctx
                                    .app
                                    .get_webview_window("main")
                                    .and_then(|window| window.hwnd().ok())
                                    .map(|hwnd| hwnd.0 as isize);
                                #[cfg(not(target_os = "windows"))]
                                let window_handle = None;
                                let state = ctx.app.state::<AppState>();
                                client_gamepad = Some(ClientGamepad::start(
                                    window_handle,
                                    Arc::clone(&state.stream_input_paused),
                                    Arc::clone(&state.touch_gamepad),
                                    Arc::clone(&state.seq_counter),
                                    session_tx.input_tx.clone(),
                                ));
                            }
                        }
                        if let Some(ref host_ctrl) = ctx.host_controls {
                            host_ctrl.connected.store(is_live, Ordering::Relaxed);
                            if is_live {
                                host_ctrl.force_idr.store(true, Ordering::Release);
                            }
                        } else if is_live {
                            let codecs = ctx
                                .client_codecs
                                .lock()
                                .map(|g| g.clone())
                                .unwrap_or_else(|_| vec![VideoCodec::H264]);
                            let _ = session_tx.control_tx.try_send(
                                ControlMessage::ClientCapabilities {
                                    preferred_codecs: codecs,
                                    display: ctx
                                        .client_display
                                        .lock()
                                        .ok()
                                        .and_then(|display| *display),
                                },
                            );
                            let _ = session_tx.control_tx.try_send(
                                ControlMessage::RequestKeyframe {
                                    last_good_frame_id: None,
                                    reason: "client stream joined".to_string(),
                                },
                            );
                        }
                    }
                    NetworkEvent::VideoFrameReceived(frame) => {
                        video_rx_count += 1;
                        client_fps_frames += 1;
                        if !client_has_video {
                            last_client_telemetry = Instant::now();
                            client_has_video = true;
                        }
                        let mut metadata = frame.meta;
                        if let Some(codec) = VideoCodec::from_u8(frame.codec) {
                            metadata.codec_id = crate::nvenc::CodecId::for_codec(codec).wire_id();
                        }
                        client_metrics.on_frame_meta(metadata);

                        #[cfg(target_os = "android")]
                        if ctx.role == SessionRole::Client
                            && crate::android_jni::take_keyframe_request()
                        {
                            let _ = session_tx.control_tx.try_send(
                                ControlMessage::RequestKeyframe {
                                    last_good_frame_id: None,
                                    reason: "android decoder attached".to_string(),
                                },
                            );
                        }

                        let native_video = {
                            #[cfg(target_os = "android")]
                            {
                                if ctx.role == SessionRole::Client {
                                    let route = crate::android_jni::submit_video_frame(&frame);
                                    match route {
                                        crate::android_jni::FrameRoute::WebView => false,
                                        crate::android_jni::FrameRoute::Native => true,
                                        crate::android_jni::FrameRoute::NativeBacklog => {
                                            let _ = session_tx.control_tx.try_send(
                                                ControlMessage::RequestKeyframe {
                                                    last_good_frame_id: Some(frame.frame_id),
                                                    reason: "android decoder backlog".to_string(),
                                                },
                                            );
                                            true
                                        }
                                    }
                                } else {
                                    false
                                }
                            }
                            #[cfg(not(target_os = "android"))]
                            {
                                false
                            }
                        };

                        #[cfg(target_os = "android")]
                        {
                            crate::android_jni::set_native_route(native_video);
                            let size = crate::android_jni::video_size();
                            if native_video_route != Some(native_video) || native_video_size != size
                            {
                                native_video_route = Some(native_video);
                                native_video_size = size;
                                let (width, height) = match size {
                                    Some((width, height)) => (Some(width), Some(height)),
                                    None => (None, None),
                                };
                                let _ = ctx.app.emit(
                                    "vmax://native-video",
                                    NativeVideoState {
                                        active: native_video,
                                        width,
                                        height,
                                    },
                                );
                            }
                        }

                        if !native_video
                            && let Ok(guard) = ctx.video_channel.lock()
                            && let Some(ref ch) = *guard
                        {
                            let raw = build_video_ipc_frame(
                                frame.frame_id,
                                frame.timestamp_us,
                                frame.codec,
                                frame.is_keyframe,
                                &frame.payload,
                                frame.meta.host_encode_us,
                            );
                            let _ = ch.send(InvokeResponseBody::Raw(raw));
                        }

                        if frame.is_keyframe || video_rx_count % 60 == 1 {
                            emit_packet_log(
                                &ctx.app,
                                "VIDEO",
                                "RX",
                                format!(
                                    "Frame #{} ({} B, key={}, fec_recovered={})",
                                    frame.frame_id,
                                    frame.payload.len(),
                                    frame.is_keyframe,
                                    frame.fec_recovered_shards
                                ),
                            );
                        }

                    }
                    NetworkEvent::AudioPacketReceived(pkt) => {
                        audio_rx_count += 1;
                        if let Some(ref sink) = ctx.playback_sink
                            && let Ok(guard) = sink.lock()
                            && let Some(ref playback) = *guard
                        {
                            playback.push_packet(&pkt);
                        }
                        if audio_rx_count % 200 == 1 {
                            let denoised = (pkt.header.flags & audio_flags::DENOISED) != 0;
                            emit_packet_log(
                                &ctx.app,
                                "AUDIO",
                                "RX",
                                format!(
                                    "Audio seq #{} ({} B, denoised={denoised})",
                                    pkt.header.seq,
                                    pkt.payload.len()
                                ),
                            );
                        }
                    }
                    NetworkEvent::InputEventReceived(pkt) => {
                        if ctx.host_controls.as_ref().is_some_and(|host| {
                            host.effective_guest_input() & guest_input_category(&pkt.event) == 0
                        }) {
                            continue;
                        }
                        input_rx_count += 1;
                        if ctx.role == SessionRole::Host {
                            #[cfg(target_os = "windows")]
                            {
                                let bounds = ctx.host_controls.as_ref().and_then(|host| {
                                    host.desktop_bounds.lock().ok().and_then(|bounds| *bounds)
                                });
                                let accepted = held_inputs.inject(&pkt, bounds);
                                if !accepted
                                    && last_input_rejection
                                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
                                {
                                    last_input_rejection = Some(Instant::now());
                                    emit_packet_log(
                                        &ctx.app,
                                        "INPUT",
                                        "WARN",
                                        "Windows rejected remote keyboard or mouse input. Check whether the foreground game is running as administrator.".into(),
                                    );
                                }
                                if let (
                                    Some(taps),
                                    InputEvent::TouchPointer {
                                        x_norm,
                                        y_norm,
                                        tap_id,
                                        action: crate::protocol::TouchAction::Tap,
                                    },
                                ) = (&remote_taps, pkt.event)
                                    && accepted
                                {
                                    let point =
                                        crate::input::desktop_point(x_norm, y_norm, bounds);
                                    taps.send_replace(Some((tap_id, point)));
                                }
                            }
                            #[cfg(target_os = "windows")]
                            if matches!(pkt.event, InputEvent::GamepadState { .. }) {
                                let pad = gamepad.get_or_insert_with(HostGamepad::default);
                                if let Err(err) = pad.receive(&pkt).await {
                                    emit_packet_log(&ctx.app, "INPUT", "ERROR", err);
                                }
                            }
                        }
                        if input_rx_count % 60 == 1
                            && !matches!(pkt.event, InputEvent::Unicode { .. })
                        {
                            emit_packet_log(
                                &ctx.app,
                                "INPUT",
                                "RX",
                                format!("Received {input_rx_count} input events"),
                            );
                        }
                    }
                    NetworkEvent::BitrateUpdated(decision) => {
                        if let Some(ref host_ctrl) = ctx.host_controls {
                            host_ctrl.target_bitrate_bps.store(decision.target_bitrate_bps, Ordering::Relaxed);
                        }
                        let mbps = decision.target_bitrate_bps as f64 / 1_000_000.0;
                        emit_packet_log(
                            &ctx.app,
                            "RATE",
                            "CTRL",
                            format!("Target bitrate -> {mbps:.1} Mbps ({})", decision.reason),
                        );
                    }
                    NetworkEvent::KeyframeRequested {
                        last_good_frame_id,
                        reason,
                    } => {
                        if let Some(ref host_ctrl) = ctx.host_controls {
                            if last_good_frame_id.is_none() {
                                host_ctrl.force_idr.store(true, Ordering::Release);
                            } else {
                                host_ctrl.force_intra_refresh.store(5, Ordering::Release);
                            }
                        }
                        emit_packet_log(
                            &ctx.app,
                            "VIDEO",
                            "PLI",
                            format!("Keyframe requested: {reason}"),
                        );
                    }
                    NetworkEvent::IntraRefreshRequested { frames, reason } => {
                        if let Some(ref host_ctrl) = ctx.host_controls {
                            host_ctrl
                                .force_intra_refresh
                                .store(frames.max(1), Ordering::Release);
                        }
                        emit_packet_log(
                            &ctx.app,
                            "VIDEO",
                            "IR",
                            format!("Intra-refresh wave ({frames} frames): {reason}"),
                        );
                    }
                    NetworkEvent::CongestionFeedback(_) => {}
                    NetworkEvent::ControlReceived(msg) => {
                        #[cfg(target_os = "windows")]
                        if matches!(&msg, ControlMessage::ReconnectGamepad)
                            && let Some(host) = ctx.host_controls.as_ref()
                            && host.effective_guest_input() & GUEST_CONTROLLER != 0
                            && gamepad.is_some()
                            && last_pad_reconnect
                                .is_none_or(|last| last.elapsed() >= Duration::from_secs(3))
                        {
                            last_pad_reconnect = Some(Instant::now());
                            if let Some(pad) = gamepad.as_mut() {
                                pad.reconnect();
                            }
                            reported_port = None;
                            let _ = session_tx
                                .control_tx
                                .try_send(ControlMessage::GamepadPort { port: None });
                        }
                        if let ControlMessage::GamepadPort { port } = &msg
                            && ctx.role == SessionRole::Client
                        {
                            let _ = ctx.app.emit(
                                "vmax://gamepad-port",
                                port.filter(|port| (1..=4).contains(port)),
                            );
                        }
                        if let ControlMessage::EncoderLatency { micros } = &msg
                            && ctx.role == SessionRole::Client
                            && *micros <= 1_000_000
                        {
                            let smooth_ms = record_encode_latency_us(*micros);
                            let _ = ctx
                                .app
                                .emit("vmax://host-encode-ms", f64::from(round_2dp(smooth_ms)));
                        }
                        if let ControlMessage::ClientBitrateLimit { mbps } = &msg
                            && ctx.role == SessionRole::Host
                        {
                            session_tx.set_client_bitrate_limit((*mbps).clamp(1, 200) * 1_000_000);
                        }
                        if ctx.role == SessionRole::Client
                            && let ControlMessage::TextFocus { tap_id, editable } = &msg
                        {
                            let _ = ctx.app.emit(
                                "vmax://text-focus",
                                TextFocusPayload {
                                    tap_id: *tap_id,
                                    editable: *editable,
                                },
                            );
                        }
                        if let ControlMessage::ClientCapabilities {
                            mut preferred_codecs,
                            display,
                        } = msg
                            && let Some(ref host_ctrl) = ctx.host_controls
                        {
                            preferred_codecs.retain(|codec| !matches!(codec, VideoCodec::HevcMain10 | VideoCodec::Hevc444));
                            if display.is_some_and(|display| display.hevc_444) {
                                preferred_codecs.push(VideoCodec::Hevc444);
                            }
                            if display.is_some_and(|display| display.hevc_main10)
                                && preferred_codecs.contains(&VideoCodec::Hevc)
                            {
                                preferred_codecs.insert(0, VideoCodec::HevcMain10);
                            }
                            if let Ok(mut previous) = host_ctrl.client_display.lock() {
                                *previous = display;
                            }
                            host_ctrl.pending_reconfigure.store(true, Ordering::Release);
                            if let Ok(mut guard) = host_ctrl.pending_codecs.lock() {
                                *guard = Some(preferred_codecs.clone());
                            }
                            emit_packet_log(
                                &ctx.app,
                                "VIDEO",
                                "CAPS",
                                format!("Client codecs -> {preferred_codecs:?}"),
                            );
                        }
                    }
                }
            }

            #[cfg(target_os = "android")]
            if ctx.role == SessionRole::Client {
                haptics::deliver_android(Rumble::default(), false);
            }
            if ctx.role == SessionRole::Host {
                let app_rehost = ctx.app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_rehost.state::<AppState>();
                    if state.host_auto_rehost.load(Ordering::Acquire) {
                        let saved_cfg =
                            state.last_host_config.lock().ok().and_then(|g| g.clone());
                        if let Some(cfg) = saved_cfg {
                            emit_packet_log(
                                &app_rehost,
                                "HOST",
                                "RESET",
                                "Client disconnected - readying host for next session...".into(),
                            );
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            if state.host_auto_rehost.load(Ordering::Acquire) {
                                match start_host_session_internal(
                                    app_rehost.clone(),
                                    &state,
                                    cfg.params.clone(),
                                    cfg.saved_only,
                                    Some(cfg.room_code.clone()),
                                    None,
                                )
                                .await
                                {
                                    Ok(tel) => {
                                        let _ = app_rehost.emit("vmax://telemetry", tel);
                                    }
                                    Err(err) => {
                                        emit_packet_log(
                                            &app_rehost,
                                            "HOST",
                                            "ERR",
                                            format!("Auto-rehost failed: {err}"),
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });
        *state.active_forwarder.lock().await = Some(forwarder);
    })
}

#[tauri::command]
pub fn list_host_displays() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "windows")]
    {
        serde_json::to_value(crate::capture::win::list_displays()?).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    Ok(serde_json::json!([]))
}

#[tauri::command]
pub async fn prepare_internet(
    app: AppHandle,
    state: State<'_, AppState>,
    preferred_relay: Option<String>,
    additional_relays: Option<Vec<String>>,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let _lifecycle = state.session_lifecycle.lock().await;
        if let Some(endpoint) = crate::internet::preferred_public_relay(
            preferred_relay.as_deref(),
            additional_relays.as_deref().unwrap_or_default(),
        )
        .await
        {
            return Ok(endpoint);
        }
        let mut tunnel = state.internet.lock().await;
        if let Some(existing) = tunnel.as_mut() {
            if existing.alive() && existing.reachable().await {
                return Ok(existing.url.clone());
            }
        }
        *tunnel = None;
        let directory = app
            .path()
            .app_local_data_dir()
            .map_err(|e| e.to_string())?
            .join("internet");
        let started = crate::internet::InternetTunnel::start(&directory).await?;
        let url = started.url.clone();
        *tunnel = Some(started);
        Ok(url)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, state, preferred_relay, additional_relays);
        Err("Set up internet hosting on the Windows host, then use its full invite.".into())
    }
}

#[tauri::command]
pub async fn controller_driver_status() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        tokio::task::spawn_blocking(|| {
            vigem_client::Client::connect()
                .map(|_| "Xbox controller driver is available. Connect a controller on the client or enable touch gaming.".to_string())
                .map_err(|e| format!("Install or repair the host Xbox controller driver (ViGEmBus): {e}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok("The controller driver is needed only on the Windows host.".into())
    }
}

#[tauri::command]
pub async fn restart_as_administrator(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::UI::{
            Shell::{IsUserAnAdmin, ShellExecuteW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        };
        use windows::core::{PCWSTR, w};
        let _lifecycle = state.session_lifecycle.lock().await;
        if state.active_tx.lock().await.is_some() {
            return Err("End the current session before restarting VmaX.".into());
        }
        if unsafe { IsUserAnAdmin() }.as_bool() {
            return Err("VmaX is already running as administrator.".into());
        }
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let result = tokio::task::spawn_blocking(move || {
            let path: Vec<_> = exe.as_os_str().encode_wide().chain([0]).collect();
            let args: Vec<_> = format!("--wait-for-vmax={}", std::process::id())
                .encode_utf16()
                .chain([0])
                .collect();
            unsafe {
                ShellExecuteW(
                    None,
                    w!("runas"),
                    PCWSTR(path.as_ptr()),
                    PCWSTR(args.as_ptr()),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                )
            }
            .0 as isize
        })
        .await
        .map_err(|error| error.to_string())?;
        if result <= 32 {
            return Err(
                "Windows could not start VmaX as administrator, or approval was declined.".into(),
            );
        }
        app.exit(0);
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, state);
        Err("Administrator hosting is available on Windows.".into())
    }
}

#[tauri::command]
pub async fn install_controller_driver(app: AppHandle) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let directory = app
            .path()
            .app_local_data_dir()
            .map_err(|e| e.to_string())?
            .join("drivers");
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|e| e.to_string())?;
        let installer = directory.join("ViGEmBus-1.22.0.exe");
        if !installer.exists() {
            let dl = tokio::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command",
                    "Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/nefarius/ViGEmBus/releases/download/v1.22.0/ViGEmBus_1.22.0_x64_x86_arm64.exe' -OutFile $env:VMAX_DRIVER_INSTALLER"])
                .env("VMAX_DRIVER_INSTALLER", &installer)
                .creation_flags(0x08000000).output().await
                .map_err(|e| format!("Could not download ViGEmBus installer: {e}"))?;
            if !dl.status.success() {
                return Err("Could not download ViGEmBus v1.22.0 from GitHub. Check your internet connection or install ViGEmBus manually.".into());
            }
        }
        let result = tokio::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; $installerProcess = Start-Process -FilePath $env:VMAX_DRIVER_INSTALLER -Verb RunAs -Wait -PassThru; exit $installerProcess.ExitCode"])
            .env("VMAX_DRIVER_INSTALLER", &installer)
            .creation_flags(0x08000000).output().await
            .map_err(|e| format!("Could not open the controller driver installer: {e}"))?;
        if result.status.code() == Some(3010) {
            return Ok("Controller driver installed. Restart Windows to finish setup.".into());
        }
        if !result.status.success() {
            return Err("Driver setup did not finish. Approve the Windows installer, or install/repair ViGEmBus from its official GitHub release, then check again.".into());
        }
        controller_driver_status().await
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        Err("Install the driver on the Windows host.".into())
    }
}

async fn ensure_local_relay_if_local(
    app: &AppHandle,
    state: &AppState,
    signaling_url: &str,
) -> Result<String, String> {
    let Ok(parsed) = Url::parse(signaling_url) else {
        return Ok(signaling_url.to_string());
    };
    if parsed.scheme() != "ws" {
        return Ok(signaling_url.to_string());
    }
    let host = parsed.host_str().unwrap_or_default();
    let lan_ip = discover_primary_lan_ip().map(|ip| ip.to_string());
    let is_local_target =
        host == "127.0.0.1" || host == "localhost" || lan_ip.as_deref() == Some(host);
    if !is_local_target {
        return Ok(signaling_url.to_string());
    }

    let port = parsed.port().unwrap_or(80);
    let mut guard = state.local_relay.lock().await;
    if guard.is_none() {
        let srv = match LocalSignalingServer::start(&format!("0.0.0.0:{port}")).await {
            Ok(s) => s,
            Err(_) => LocalSignalingServer::start("0.0.0.0:0")
                .await
                .map_err(|e| e.to_string())?,
        };
        let bound_port = srv.local_addr.port();
        let status = LocalRelayStatus {
            running: true,
            ws_url: Some(format!("ws://127.0.0.1:{bound_port}")),
            lan_ws_url: discover_primary_lan_ip().map(|ip| format!("ws://{ip}:{bound_port}")),
        };
        *guard = Some(srv);
        let _ = app.emit("vmax://relay-status", &status);
        let effective_host = if host == "localhost" {
            "127.0.0.1"
        } else {
            host
        };
        return Ok(format!("ws://{effective_host}:{bound_port}"));
    }

    if let Some(existing) = guard.as_ref() {
        let bound_port = existing.local_addr.port();
        let effective_host = if host == "localhost" {
            "127.0.0.1"
        } else {
            host
        };
        return Ok(format!("ws://{effective_host}:{bound_port}"));
    }
    Ok(signaling_url.to_string())
}

async fn start_host_session_internal(
    app: AppHandle,
    state: &AppState,
    params: StartSessionParams,
    saved_only: Option<bool>,
    rehost_room: Option<Option<String>>,
    invited_client: Option<String>,
) -> Result<NetworkTelemetry, String> {
    if !cfg!(target_os = "windows") {
        return Err("Hosting requires a Windows PC.".to_string());
    }
    let _lifecycle = state.session_lifecycle.lock().await;
    if invited_client.is_some()
        && let Some(telemetry) = state.telemetry.lock().await.as_ref()
        && !matches!(
            telemetry.lock().await.phase,
            ConnectionPhase::Idle | ConnectionPhase::Failed | ConnectionPhase::Disconnected
        )
    {
        return Err("Finish the current session before accepting a connection".into());
    }
    if let Some(expected_room) = &rehost_room
        && (!state.host_auto_rehost.load(Ordering::Acquire)
            || !state
                .last_host_config
                .lock()
                .ok()
                .and_then(|cfg| cfg.as_ref().map(|cfg| cfg.room_code == *expected_room))
                .unwrap_or(false))
    {
        return Err("Hosting was stopped or replaced".into());
    }
    let profile_endpoint = normalize_endpoint(&params.signaling_url)?;
    if params.relay_only && !profile_endpoint.starts_with("wss://") {
        return Err("Relay-only hosting requires a wss:// VmaX relay endpoint.".into());
    }
    let saved_clients = if saved_only.unwrap_or(false) {
        let clients = load_saved_connections(&app)?.allowed_clients(&profile_endpoint);
        if clients.is_empty() {
            return Err("Approve at least one saved client for this network first.".into());
        }
        Some(clients)
    } else {
        None
    };
    let identity_file = app_data_file(&app, "device-identity.json")?;
    stop_active_internal(state).await;

    // Session quality policy from Settings → Video.  All three default to on:
    // full host resolution, constant FPS, and the whole configured bitrate.
    crate::capture::set_lock_resolution(params.lock_resolution);
    crate::capture::set_constant_fps(params.constant_fps);

    #[cfg(target_os = "windows")]
    let tunnel_origin = state
        .internet
        .lock()
        .await
        .as_ref()
        .filter(|tunnel| normalize_endpoint(&tunnel.url).ok().as_ref() == Some(&profile_endpoint))
        .map(|tunnel| tunnel.origin_url().to_string());
    #[cfg(not(target_os = "windows"))]
    let tunnel_origin: Option<String> = None;
    let managed_tunnel = tunnel_origin.is_some();
    let effective_signaling = match tunnel_origin {
        Some(origin) => origin,
        None => ensure_local_relay_if_local(&app, state, &params.signaling_url).await?,
    };

    let mut cfg = build_network_config(effective_signaling, &params);
    cfg.relay_only = params.relay_only;
    cfg.identity_file = Some(identity_file);
    cfg.profile_endpoint = Some(profile_endpoint);
    cfg.saved_clients = saved_clients;
    cfg.invited_client = invited_client.clone();
    let room_code = if cfg.saved_clients.is_none() {
        Some(
            rehost_room
                .flatten()
                .unwrap_or_else(crate::signaling::generate_room_code),
        )
    } else {
        None
    };
    cfg.host_room_code = room_code.clone();
    if managed_tunnel
        || cfg.saved_clients.is_some()
        || cfg.invited_client.is_some()
        || cfg.relay_only
    {
        cfg.protocol = TransportProtocol::Quic;
    }
    if managed_tunnel {
        cfg.advertised_signaling_url = Some(params.signaling_url.clone());
    } else if let Ok(mut advertised) = Url::parse(&cfg.signaling_url)
        && advertised.scheme() == "ws"
        && matches!(advertised.host_str(), Some("localhost" | "127.0.0.1"))
        && let Some(lan_ip) = discover_primary_lan_ip().filter(|ip| !ip.is_loopback())
        && advertised.set_host(Some(&lan_ip.to_string())).is_ok()
    {
        cfg.advertised_signaling_url = Some(advertised.to_string());
    }
    let protocol = cfg.protocol;
    let initial_bps = cfg.initial_bitrate_bps;

    let handle = net_start_host(cfg).await.map_err(|e| e.to_string())?;
    let initial = handle.telemetry.lock().await.clone();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let (max_w, max_h) = parse_resolution_ceiling(&params.resolution);
    let controls = HostRuntimeControls {
        guest_input_mask: Arc::new(AtomicU8::new(if params.guest_controller_only {
            GUEST_CONTROLLER
        } else {
            GUEST_ALL
        })),
        host_only_input: Arc::new(AtomicBool::new(params.host_only_input)),
        #[cfg(target_os = "windows")]
        desktop_bounds: Arc::default(),
        connected: Arc::new(AtomicBool::new(false)),
        force_idr: Arc::new(AtomicBool::new(true)),
        force_intra_refresh: Arc::new(AtomicU32::new(0)),
        target_bitrate_bps: Arc::new(AtomicU32::new(initial_bps)),
        desired_max_w: Arc::new(AtomicU32::new(max_w)),
        desired_max_h: Arc::new(AtomicU32::new(max_h)),
        desired_fps: Arc::new(AtomicU16::new(params.framerate.clamp(30, 144))),
        desired_encoder_mode: Arc::new(AtomicU8::new(encoder_mode_to_u8(parse_encoder_mode(
            &params.encoder,
        )))),
        sharpness: Arc::new(AtomicU8::new(params.sharpness.min(100))),
        upscaling: Arc::new(AtomicU8::new(
            crate::quality::UpscalingMode::parse(&params.upscaling).to_u8(),
        )),
        pending_reconfigure: Arc::new(AtomicBool::new(false)),
        pending_codecs: Arc::new(StdMutex::new(None)),
        client_display: Arc::default(),
    };

    let capture_thread = match spawn_host_capture_thread(
        app.clone(),
        params.clone(),
        handle.tx.video_tx.clone(),
        controls.clone(),
        Arc::clone(&stop_flag),
        invited_client.is_some(),
    )
    .await
    {
        Ok(t) => Some(t),
        Err(err) => {
            handle.tx.close();
            return Err(err);
        }
    };

    state
        .audio_denoise_flag
        .store(params.ai_noise_suppression, Ordering::Relaxed);
    let audio_capture = if params.stream_computer_audio.unwrap_or(true) {
        match start_host_audio_capture(
            handle.tx.audio_tx.clone(),
            params.audio_bitrate_kbps,
            params.ai_noise_suppression,
            params.audio_output_id.clone(),
            params.audio_process_id,
            params.exclude_discord_audio,
        ) {
            Ok(ac) => {
                ac.set_rnnoise_enabled(params.ai_noise_suppression);
                Some(ac)
            }
            Err(err) => {
                emit_packet_log(&app, "AUDIO", "WARN", format!("Capture unavailable: {err}"));
                None
            }
        }
    } else {
        None
    };

    if let Ok(mut guard) = state.active_pipeline.lock() {
        *guard = Some(ActivePipelineHandles {
            stop_flag,
            capture_thread,
            audio_capture,
            audio_playback: Arc::new(StdMutex::new(None)),
            host_controls: Some(controls.clone()),
        });
    }

    if let Ok(mut last_cfg) = state.last_host_config.lock() {
        *last_cfg = Some(LastHostConfig {
            params,
            saved_only,
            room_code,
        });
    }
    state
        .host_auto_rehost
        .store(invited_client.is_none(), Ordering::Release);

    register_session(
        handle,
        state,
        EventForwarderContext {
            app,
            role: SessionRole::Host,
            protocol,
            video_channel: Arc::clone(&state.video_channel),
            client_codecs: Arc::clone(&state.client_codecs),
            client_display: state.client_display.clone(),
            host_controls: Some(controls),
            playback_sink: None,
        },
    )
    .await;
    Ok(initial)
}

#[tauri::command]
pub async fn start_host_session(
    app: AppHandle,
    state: State<'_, AppState>,
    params: StartSessionParams,
    saved_only: Option<bool>,
) -> Result<NetworkTelemetry, String> {
    start_host_session_internal(app, &state, params, saved_only, None, None).await
}

pub(crate) async fn expire_waiting_invite(app: &AppHandle, route: &str) {
    let state = app.state::<AppState>();
    let _lifecycle = state.session_lifecycle.lock().await;
    if let Ok(snapshot) = get_network_snapshot(app.state()).await
        && snapshot.phase == ConnectionPhase::WaitingForPeer
        && snapshot.invite_uri.as_deref() == Some(route)
    {
        stop_active_internal(&state).await;
    }
}

pub(crate) async fn host_approved_device(
    app: AppHandle,
    params: StartSessionParams,
    fingerprint: String,
) -> Result<String, String> {
    let state = app.state::<AppState>();
    start_host_session_internal(app.clone(), &state, params, None, None, Some(fingerprint))
        .await?;
    let route = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = get_network_snapshot(app.state::<AppState>()).await?;
            if let Some(invite) = snapshot.invite_uri {
                return Ok(invite);
            }
            if matches!(
                snapshot.phase,
                ConnectionPhase::Failed | ConnectionPhase::Disconnected
            ) {
                return Err(snapshot
                    .last_error
                    .unwrap_or_else(|| "Host session ended".into()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    match route {
        Ok(result) => result,
        Err(_) => {
            let _ = stop_session(app.clone(), app.state()).await;
            Err("Timed out starting the approved session".into())
        }
    }
}

#[tauri::command]
pub fn list_host_audio_outputs() -> Result<Vec<crate::audio::AudioOutput>, String> {
    crate::audio::list_audio_outputs()
}

#[tauri::command]
pub async fn join_session(
    app: AppHandle,
    state: State<'_, AppState>,
    invite_input: String,
    params: StartSessionParams,
    saved_host: Option<String>,
) -> Result<NetworkTelemetry, String> {
    state.host_auto_rehost.store(false, Ordering::Release);
    let _lifecycle = state.session_lifecycle.lock().await;
    let device = if let Some(fingerprint) = saved_host {
        let device = load_saved_connections(&app)?
            .devices
            .into_iter()
            .find(|peer| peer.role == SessionRole::Host && peer.fingerprint == fingerprint)
            .ok_or_else(|| "Saved host no longer exists.".to_string())?;
        let key = device
            .resume_key
            .ok_or_else(|| "This host needs a new invite before reconnecting.".to_string())?;
        Some((device.endpoint, device.fingerprint, key))
    } else {
        None
    };
    let identity_file = app_data_file(&app, "device-identity.json")?;
    stop_active_internal(&state).await;

    let endpoint = device
        .as_ref()
        .map_or_else(|| params.signaling_url.clone(), |device| device.0.clone());
    let mut cfg = build_network_config(endpoint, &params);
    cfg.identity_file = Some(identity_file);
    if device.is_some() {
        cfg.protocol = TransportProtocol::Quic;
    }
    let protocol = cfg.protocol;

    let handle = match device {
        Some((_, fingerprint, key)) => net_start_saved_client(cfg, fingerprint, key).await,
        None => net_start_client(cfg, invite_input).await,
    }
    .map_err(|e| e.to_string())?;
    let initial = handle.telemetry.lock().await.clone();

    let playback = match start_client_audio_playback() {
        Ok(pb) => Some(pb),
        Err(err) => {
            emit_packet_log(
                &app,
                "AUDIO",
                "WARN",
                format!("Playback unavailable: {err}"),
            );
            None
        }
    };

    let playback_sink = Arc::new(StdMutex::new(playback));
    if let Ok(mut guard) = state.active_pipeline.lock() {
        *guard = Some(ActivePipelineHandles {
            stop_flag: Arc::new(AtomicBool::new(false)),
            capture_thread: None,
            audio_capture: None,
            audio_playback: Arc::clone(&playback_sink),
            host_controls: None,
        });
    }

    register_session(
        handle,
        &state,
        EventForwarderContext {
            app,
            role: SessionRole::Client,
            protocol,
            video_channel: Arc::clone(&state.video_channel),
            client_codecs: Arc::clone(&state.client_codecs),
            client_display: state.client_display.clone(),
            host_controls: None,
            playback_sink: Some(playback_sink),
        },
    )
    .await;
    Ok(initial)
}

async fn stop_active_internal(state: &AppState) {
    state.touch_gamepad.set(None);
    #[cfg(target_os = "android")]
    {
        crate::android_jni::reset_route();
        crate::haptics::deliver_android(crate::haptics::Rumble::default(), false);
    }
    if let Some(forwarder) = state.active_forwarder.lock().await.take() {
        forwarder.abort();
        let _ = forwarder.await;
    }
    if let Some(tx) = state.active_tx.lock().await.take() {
        tx.close();
    }
    *state.telemetry.lock().await = None;
    let pending_pipeline = state
        .active_pipeline
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    if let Some(pipeline) = pending_pipeline {
        let _ = tokio::task::spawn_blocking(move || pipeline.shutdown()).await;
    }
}

#[tauri::command]
pub async fn stop_session(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.host_auto_rehost.store(false, Ordering::Release);
    let _lifecycle = state.session_lifecycle.lock().await;
    stop_active_internal(&state).await;
    let _ = app.emit(
        "vmax://hardware-telemetry",
        HardwareTelemetryPayload::default(),
    );
    Ok(())
}

#[tauri::command]
pub async fn get_network_snapshot(state: State<'_, AppState>) -> Result<NetworkTelemetry, String> {
    let tel_opt = state.telemetry.lock().await.clone();
    if let Some(tel) = tel_opt {
        Ok(tel.lock().await.clone())
    } else {
        Ok(NetworkTelemetry::default())
    }
}

pub(crate) fn app_data_file(app: &AppHandle, filename: &str) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|dir| dir.join(filename))
        .map_err(|err| err.to_string())
}

fn load_saved_connections(app: &AppHandle) -> Result<SavedConnections, String> {
    let path = app_data_file(app, "saved-connections.json")?;
    read_json(&path)
        .map(|saved| saved.unwrap_or_default())
        .map_err(|err| err.to_string())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedConnectionUpdate {
    SaveNetwork {
        name: String,
        endpoint: String,
    },
    RemoveNetwork {
        endpoint: String,
    },
    RememberPeer {
        name: String,
    },
    SetDeviceAutomatic {
        role: SessionRole,
        fingerprint: String,
        enabled: bool,
    },
    RemoveDevice {
        role: SessionRole,
        fingerprint: String,
    },
    SetAutoHost {
        endpoint: Option<String>,
    },
    AcceptFriendRequest {
        fingerprint: String,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendInbox {
    requests: Vec<String>,
    error: Option<String>,
}

#[tauri::command]
pub async fn friend_inbox(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<FriendInbox, String> {
    let saved = load_saved_connections(&app)?;
    let identity = app_data_file(&app, "device-identity.json")?;
    let mut endpoints = std::collections::BTreeMap::<String, Vec<String>>::new();
    for friend in saved
        .devices
        .iter()
        .filter(|peer| peer.role == SessionRole::Client)
    {
        let clients = endpoints.entry(friend.endpoint.clone()).or_default();
        if friend.automatic {
            clients.push(friend.fingerprint.clone());
        }
    }
    let results =
        futures_util::future::join_all(endpoints.into_iter().map(|(endpoint, clients)| {
            let identity = identity.clone();
            let app = &app;
            let state = &*state;
            async move {
                let effective = ensure_local_relay_if_local(app, state, &endpoint).await?;
                let (requests, _) = crate::network::friend_relay_command(
                    identity,
                    &effective,
                    crate::signaling::FriendCommand::Inbox { clients },
                )
                .await
                .map_err(|error| error.to_string())?;
                Ok::<_, String>((endpoint, requests))
            }
        }))
        .await;
    let mut inbox = FriendInbox {
        requests: Vec::new(),
        error: None,
    };
    for result in results {
        match result {
            Ok((endpoint, requests)) => {
                inbox
                    .requests
                    .extend(requests.into_iter().filter(|fingerprint| {
                        saved.devices.iter().any(|friend| {
                            friend.role == SessionRole::Client
                                && !friend.automatic
                                && friend.endpoint == endpoint
                                && &friend.fingerprint == fingerprint
                        })
                    }))
            }
            Err(error) => inbox.error = Some(error),
        }
    }
    Ok(inbox)
}

#[tauri::command]
pub async fn friend_access(
    app: AppHandle,
    fingerprint: String,
    request: bool,
) -> Result<bool, String> {
    let peer = load_saved_connections(&app)?
        .devices
        .into_iter()
        .find(|peer| {
            peer.role == SessionRole::Host
                && peer.fingerprint == fingerprint
                && peer.resume_key.is_some()
        })
        .ok_or_else(|| {
            "Join this friend's invite and save them before requesting access.".to_string()
        })?;
    let command = if request {
        crate::signaling::FriendCommand::Request {
            host: peer.fingerprint,
        }
    } else {
        crate::signaling::FriendCommand::Status {
            host: peer.fingerprint,
        }
    };
    crate::network::friend_relay_command(
        app_data_file(&app, "device-identity.json")?,
        &peer.endpoint,
        command,
    )
    .await
    .map(|(_, approved)| approved)
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn dismiss_friend_request(
    app: AppHandle,
    state: State<'_, AppState>,
    fingerprint: String,
) -> Result<(), String> {
    let peer = load_saved_connections(&app)?
        .devices
        .into_iter()
        .find(|peer| peer.role == SessionRole::Client && peer.fingerprint == fingerprint)
        .ok_or_else(|| "This friend is no longer saved.".to_string())?;
    let endpoint = ensure_local_relay_if_local(&app, &state, &peer.endpoint).await?;
    crate::network::friend_relay_command(
        app_data_file(&app, "device-identity.json")?,
        &endpoint,
        crate::signaling::FriendCommand::Resolve {
            client: peer.fingerprint,
        },
    )
    .await
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn get_saved_connections(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SavedConnections, String> {
    let _lifecycle = state.session_lifecycle.lock().await;
    Ok(load_saved_connections(&app)?.view())
}

#[tauri::command]
pub async fn update_saved_connections(
    app: AppHandle,
    state: State<'_, AppState>,
    update: SavedConnectionUpdate,
) -> Result<SavedConnections, String> {
    let _lifecycle = state.session_lifecycle.lock().await;
    let mut saved = load_saved_connections(&app)?;
    let original = saved.clone();
    let accepting_request = matches!(&update, SavedConnectionUpdate::AcceptFriendRequest { .. });
    let snapshot = if let Some(telemetry) = state.telemetry.lock().await.clone() {
        telemetry.lock().await.clone()
    } else {
        NetworkTelemetry::default()
    };

    match update {
        SavedConnectionUpdate::SaveNetwork { name, endpoint } => {
            saved.save_network(name, endpoint)?
        }
        SavedConnectionUpdate::RemoveNetwork { endpoint } => {
            saved
                .networks
                .retain(|network| network.endpoint != endpoint);
        }
        SavedConnectionUpdate::RememberPeer { name } => {
            if snapshot.phase != ConnectionPhase::Connected || !snapshot.peer_savable {
                return Err("Connect over authenticated QUIC before saving this device.".into());
            }
            let peer = snapshot
                .trusted_peer
                .as_ref()
                .ok_or_else(|| "Peer identity unavailable.".to_string())?;
            let role = match snapshot.role {
                Some(SessionRole::Host) => SessionRole::Client,
                Some(SessionRole::Client) => SessionRole::Host,
                None => return Err("No active peer.".into()),
            };
            saved.remember(SavedDevice {
                name: normalize_name(&name)?,
                role,
                fingerprint: peer.fingerprint.clone(),
                endpoint: normalize_endpoint(&peer.endpoint)?,
                automatic: false,
                resume_key: peer.resume_key.clone(),
            })?;
        }
        SavedConnectionUpdate::SetDeviceAutomatic {
            role,
            fingerprint,
            enabled,
        } => {
            saved.set_device_automatic(role, &fingerprint, enabled)?;
        }
        SavedConnectionUpdate::RemoveDevice { role, fingerprint } => {
            saved
                .devices
                .retain(|peer| peer.role != role || peer.fingerprint != fingerprint);
        }
        SavedConnectionUpdate::AcceptFriendRequest { fingerprint } => {
            if !cfg!(target_os = "windows") {
                return Err("Hosting requires a Windows PC.".into());
            }
            let endpoint = saved
                .devices
                .iter()
                .find(|peer| peer.role == SessionRole::Client && peer.fingerprint == fingerprint)
                .map(|peer| peer.endpoint.clone())
                .ok_or_else(|| "This friend is no longer saved.".to_string())?;
            saved.set_device_automatic(SessionRole::Client, &fingerprint, true)?;
            saved.auto_host_endpoint = Some(endpoint);
        }
        SavedConnectionUpdate::SetAutoHost { endpoint } => {
            if let Some(endpoint) = endpoint {
                if !cfg!(target_os = "windows") {
                    return Err("Automatic hosting requires Windows.".into());
                }
                let endpoint = normalize_endpoint(&endpoint)?;
                if saved.allowed_clients(&endpoint).is_empty() {
                    return Err("Approve a saved client for this network first.".into());
                }
                saved.auto_host_endpoint = Some(endpoint);
                for peer in saved
                    .devices
                    .iter_mut()
                    .filter(|peer| peer.role == SessionRole::Host)
                {
                    peer.automatic = false;
                }
            } else {
                saved.auto_host_endpoint = None;
            }
        }
    }
    if saved
        .auto_host_endpoint
        .as_ref()
        .is_some_and(|endpoint| saved.allowed_clients(endpoint).is_empty())
    {
        saved.auto_host_endpoint = None;
    }
    write_json(&app_data_file(&app, "saved-connections.json")?, &saved)
        .map_err(|err| err.to_string())?;
    if accepting_request
        && snapshot.role == Some(SessionRole::Host)
        && snapshot.phase == ConnectionPhase::Connected
        && let Some(endpoint) = saved.auto_host_endpoint.as_ref()
        && let Ok(mut config) = state.last_host_config.lock()
        && let Some(config) = config.as_mut()
    {
        config.params.signaling_url = endpoint.clone();
        config.params.relay_only &= endpoint.starts_with("wss://");
        config.saved_only = Some(true);
    }
    let approved = |profiles: &SavedConnections| {
        profiles
            .devices
            .iter()
            .filter(|peer| peer.role == SessionRole::Client && peer.automatic)
            .map(|peer| (peer.fingerprint.clone(), peer.endpoint.clone()))
            .collect::<Vec<_>>()
    };
    if snapshot.role == Some(SessionRole::Host)
        && (snapshot.saved_session || accepting_request)
        && (!accepting_request || snapshot.phase == ConnectionPhase::WaitingForPeer)
        && (approved(&saved) != approved(&original)
            || saved.auto_host_endpoint != original.auto_host_endpoint)
    {
        state.host_auto_rehost.store(false, Ordering::Release);
        stop_active_internal(&state).await;
        let _ = app.emit(
            "vmax://telemetry",
            NetworkTelemetry {
                phase: ConnectionPhase::Disconnected,
                role: Some(SessionRole::Host),
                saved_session: true,
                ..NetworkTelemetry::default()
            },
        );
        let _ = app.emit(
            "vmax://hardware-telemetry",
            HardwareTelemetryPayload::default(),
        );
    }
    Ok(saved.view())
}

#[tauri::command]
pub async fn toggle_local_signaling(
    state: State<'_, AppState>,
    enable: bool,
) -> Result<LocalRelayStatus, String> {
    let mut guard = state.local_relay.lock().await;
    if enable {
        if let Some(existing) = guard.as_ref() {
            let port = existing.local_addr.port();
            let lan_ws_url = discover_primary_lan_ip().map(|ip| format!("ws://{ip}:{port}"));
            return Ok(LocalRelayStatus {
                running: true,
                ws_url: Some(format!("ws://127.0.0.1:{port}")),
                lan_ws_url,
            });
        }
        let srv = LocalSignalingServer::start("0.0.0.0:8787")
            .await
            .or_else(|_| futures_executor_fallback());
        let srv = match srv {
            Ok(s) => s,
            Err(_) => LocalSignalingServer::start("0.0.0.0:0")
                .await
                .map_err(|e| e.to_string())?,
        };
        let port = srv.local_addr.port();
        let ws_url = format!("ws://127.0.0.1:{port}");
        let lan_ws_url = discover_primary_lan_ip().map(|ip| format!("ws://{ip}:{port}"));
        *guard = Some(srv);
        Ok(LocalRelayStatus {
            running: true,
            ws_url: Some(ws_url),
            lan_ws_url,
        })
    } else {
        if let Some(mut srv) = guard.take() {
            srv.stop();
        }
        Ok(LocalRelayStatus {
            running: false,
            ws_url: None,
            lan_ws_url: None,
        })
    }
}

fn futures_executor_fallback() -> Result<LocalSignalingServer, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "port 8787 unavailable",
    ))
}

#[tauri::command]
pub async fn scan_lan_hosts() -> Result<Vec<DiscoveredHost>, String> {
    crate::signaling::discover_lan_hosts(Duration::from_millis(1400))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn test_media_relay(endpoint: String) -> Result<u32, String> {
    let endpoint = crate::signaling::validate_signaling_url(endpoint.trim())
        .map_err(|error| error.to_string())?;
    crate::relay_probe::check(endpoint.as_str()).await
        .map(|elapsed| elapsed.as_millis() as u32)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn list_relay_rooms(endpoint: String) -> Result<Vec<RoomSummary>, String> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return Err("Choose a signaling endpoint before listing rooms.".to_string());
    }
    crate::signaling::list_relay_rooms(trimmed)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn subscribe_stream_frames(
    state: State<'_, AppState>,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    {
        let mut guard = state.video_channel.lock()
            .map_err(|_| "failed to register video frame channel".to_string())?;
        *guard = Some(on_frame);
    }
    // The connection can deliver its first IDR before the WebView opens this
    // IPC channel. An idle desktop may never send another frame by itself.
    request_keyframe(state, Some("viewer ready".into())).await
}

#[tauri::command]
pub async fn send_input_event(
    state: State<'_, AppState>,
    input: FrontendInputPayload,
) -> Result<(), String> {
    let tx_opt = state.active_tx.lock().await.clone();
    let Some(tx) = tx_opt else {
        return Ok(());
    };
    let seq = state.seq_counter.fetch_add(1, Ordering::Relaxed);
    let pkt = build_input_packet(seq, input);
    if pkt.is_high_frequency_motion()
        || matches!(
            pkt.event,
            InputEvent::TouchPointer {
                action: crate::protocol::TouchAction::Drag,
                ..
            }
        )
    {
        let _ = tx.input_tx.try_send(pkt);
    } else {
        let _ = tx.input_tx.send(pkt).await;
    }
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TextFocusPayload {
    tap_id: u32,
    editable: bool,
}

#[tauri::command]
pub async fn set_fullscreen_keyboard_capture(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    enabled: bool,
    shortcut_scan: u32,
    shortcut_modifiers: u8,
    capture_id: u32,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use crate::input::keyboard_capture::{CaptureEvent, start, stop};
        let app = window.app_handle().clone();
        if !enabled {
            return window
                .run_on_main_thread(move || stop(capture_id))
                .map_err(|error| error.to_string());
        }
        if !window.is_fullscreen().map_err(|error| error.to_string())? {
            return Err("Enter fullscreen before capturing system shortcuts".into());
        }
        let telemetry = state
            .telemetry
            .lock()
            .await
            .clone()
            .ok_or("No active stream")?;
        let snapshot = telemetry.lock().await;
        if snapshot.role != Some(SessionRole::Client)
            || snapshot.phase != ConnectionPhase::Connected
        {
            return Err("Join a stream before capturing system shortcuts".into());
        }
        drop(snapshot);
        let tx = state
            .active_tx
            .lock()
            .await
            .clone()
            .ok_or("No active stream")?;
        let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as usize;
        let (events, mut recv) = tokio::sync::mpsc::unbounded_channel();
        let (ready, result) = tokio::sync::oneshot::channel();
        window
            .run_on_main_thread(move || {
                let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut _);
                let _ = ready.send(start(
                    capture_id,
                    hwnd,
                    (shortcut_scan, shortcut_modifiers),
                    events,
                ));
            })
            .map_err(|error| error.to_string())?;
        result.await.map_err(|error| error.to_string())??;
        let seq_counter = Arc::clone(&state.seq_counter);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.input_tx.closed() => break,
                    event = recv.recv() => match event {
                        Some(CaptureEvent::Key(input)) => {
                            let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                            if tx.input_tx.send(build_input_packet(seq, input)).await.is_err() { break; }
                        }
                        _ => break,
                    }
                }
            }
            let _ = app.emit("vmax://keyboard-capture-released", capture_id);
        });
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (
            window,
            state,
            enabled,
            shortcut_scan,
            shortcut_modifiers,
            capture_id,
        );
        Err("System shortcut capture is available on Windows clients".into())
    }
}

#[tauri::command]
pub async fn send_remote_text(state: State<'_, AppState>, text: String) -> Result<(), String> {
    if text.len() > 4096 {
        return Err("Remote keyboard text exceeds 4096 bytes".into());
    }
    let Some(tx) = state.active_tx.lock().await.clone() else {
        return Ok(());
    };
    for character in text.chars() {
        let scan_code = match character {
            '\u{8}' => 0x0e,
            '\n' => 0x1c,
            _ => 0,
        };
        if scan_code != 0 {
            for pressed in [true, false] {
                let seq = state.seq_counter.fetch_add(1, Ordering::Relaxed);
                let pkt = build_input_packet(
                    seq,
                    FrontendInputPayload::Key {
                        scan_code,
                        pressed,
                        modifiers: 0,
                    },
                );
                tx.input_tx.send(pkt).await.map_err(|e| e.to_string())?;
            }
        } else if !character.is_control() {
            let packet = InputPacket {
                seq: state.seq_counter.fetch_add(1, Ordering::Relaxed),
                client_ts_us: crate::input::now_unix_us(),
                event: InputEvent::Unicode {
                    codepoint: character as u32,
                },
            };
            tx.input_tx.send(packet).await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn set_guest_input_permissions(
    state: State<'_, AppState>,
    keyboard: bool,
    mouse: bool,
    controller: bool,
) -> Result<(), String> {
    let pipeline = state
        .active_pipeline
        .lock()
        .map_err(|err| err.to_string())?;
    let controls = pipeline
        .as_ref()
        .and_then(|pipeline| pipeline.host_controls.as_ref())
        .ok_or_else(|| "Start hosting before changing guest permissions.".to_string())?;
    let mask = u8::from(keyboard) * GUEST_KEYBOARD
        | u8::from(mouse) * GUEST_MOUSE
        | u8::from(controller) * GUEST_CONTROLLER;
    controls.guest_input_mask.store(mask, Ordering::Release);
    Ok(())
}

#[tauri::command]
pub fn set_host_only_input(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let pipeline = state
        .active_pipeline
        .lock()
        .map_err(|err| err.to_string())?;
    let controls = pipeline
        .as_ref()
        .and_then(|pipeline| pipeline.host_controls.as_ref())
        .ok_or_else(|| "Start hosting before changing live input permissions.".to_string())?;
    controls.host_only_input.store(enabled, Ordering::Release);
    Ok(())
}

#[tauri::command]
pub fn set_stream_input_paused(state: State<'_, AppState>, paused: bool) {
    state.stream_input_paused.store(paused, Ordering::Release);
    if paused {
        state.touch_gamepad.set(None);
    }
}

#[tauri::command]
pub fn set_touch_gamepad(state: State<'_, AppState>, input: Option<TouchGamepadInput>) {
    if !state.stream_input_paused.load(Ordering::Acquire) {
        state.touch_gamepad.set(input);
    }
}

#[tauri::command]
pub async fn report_client_capabilities(
    state: State<'_, AppState>,
    codecs: Vec<VideoCodec>,
    display: Option<crate::handshake::ClientDisplay>,
    native_decoder: Option<bool>,
) -> Result<(), String> {
    let mut normalized = codecs;
    #[cfg(target_os = "android")]
    let display = display.map(|mut display| {
        display.max_refresh_hz = crate::haptics::android_refresh_hz();
        if native_decoder.unwrap_or(true) {
            if crate::haptics::android_hevc() && !normalized.contains(&VideoCodec::Hevc) {
                normalized.insert(0, VideoCodec::Hevc);
            }
            display.hevc_main10 |= crate::haptics::android_main10();
        }
        display
    });
    #[cfg(not(target_os = "android"))]
    let _ = native_decoder;
    if !normalized.contains(&VideoCodec::H264) {
        normalized.push(VideoCodec::H264);
    }
    if let Ok(mut guard) = state.client_codecs.lock() {
        *guard = normalized.clone();
    }
    if let Ok(mut guard) = state.client_display.lock() {
        *guard = display;
    }
    if let Some(tx) = state.active_tx.lock().await.clone() {
        let _ = tx.control_tx.try_send(ControlMessage::ClientCapabilities {
            preferred_codecs: normalized,
            display,
        });
    }
    Ok(())
}

#[tauri::command]
pub async fn set_stream_bitrate_limit(state: State<'_, AppState>, mbps: u32) -> Result<(), String> {
    let snapshot = get_network_snapshot(state.clone()).await?;
    if snapshot.role != Some(SessionRole::Client) || snapshot.phase != ConnectionPhase::Connected {
        return Err("Connect to a stream first".into());
    }
    let tx = state.active_tx.lock().await.clone().ok_or("Session ended")?;
    tx.control_tx
        .try_send(ControlMessage::ClientBitrateLimit {
            mbps: mbps.clamp(1, 200),
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn reconnect_stream_gamepad(state: State<'_, AppState>) -> Result<(), String> {
    let snapshot = get_network_snapshot(state.clone()).await?;
    if snapshot.role != Some(SessionRole::Client) || snapshot.phase != ConnectionPhase::Connected {
        return Err("Connect to a stream first".into());
    }
    let tx = state.active_tx.lock().await.clone().ok_or("Session ended")?;
    tx.control_tx
        .try_send(ControlMessage::ReconnectGamepad)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn request_keyframe(
    state: State<'_, AppState>,
    reason: Option<String>,
) -> Result<(), String> {
    if let Some(tx) = state.active_tx.lock().await.clone() {
        let _ = tx.control_tx.try_send(ControlMessage::RequestKeyframe {
            last_good_frame_id: None,
            reason: reason.unwrap_or_else(|| "decoder sync".to_string()),
        });
    }
    Ok(())
}

#[tauri::command]
pub async fn update_runtime_audio_denoise(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    state.audio_denoise_flag.store(enabled, Ordering::Relaxed);
    if let Ok(mut s) = state.runtime_settings.lock() {
        s.audio.ai_noise_suppression = enabled;
    }
    if let Ok(guard) = state.active_pipeline.lock()
        && let Some(ref pipeline) = *guard
        && let Some(ref ac) = pipeline.audio_capture
    {
        ac.set_rnnoise_enabled(enabled);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_haptics_enabled(
    state: State<'_, AppState>,
    enabled: bool,
    intensity: u8,
) -> Result<(), String> {
    state.haptics.set(enabled, intensity);
    if (!enabled || intensity == 0)
        && let Some(tx) = state.active_tx.lock().await.as_ref()
    {
        tx.haptics_tx.send_replace(crate::haptics::Rumble::default());
    }
    #[cfg(target_os = "android")]
    if !enabled || intensity == 0 {
        crate::haptics::deliver_android(crate::haptics::Rumble::default(), false);
    }
    Ok(())
}

#[tauri::command]
pub fn get_haptics_settings(state: State<'_, AppState>) -> crate::haptics::HapticsSettings {
    state.haptics.get()
}

#[tauri::command]
pub fn graphics_backends() -> crate::graphics::BackendCaps {
    crate::graphics::probe()
}

#[tauri::command]
pub async fn update_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: VmaXSettingsPayload,
) -> Result<(), String> {
    // Up to 200 Mbps: on a LAN or a fast link the stream is allowed to spend
    // the whole pipe rather than a conservative preset ceiling.
    let clamped_bps = settings.video.bitrate_mbps.clamp(1, 200) * 1_000_000;
    crate::capture::set_lock_resolution(settings.video.lock_resolution);
    crate::capture::set_constant_fps(settings.video.constant_fps);
    let clamped_fps = settings.video.framerate.clamp(30, 144);
    let (max_w, max_h) = parse_resolution_ceiling(&settings.video.resolution);
    let enc_mode = encoder_mode_to_u8(parse_encoder_mode(&settings.video.encoder));
    let fec_group = map_fec_group_size(settings.network.fec_redundancy_pct);

    if let Ok(mut guard) = state.runtime_settings.lock() {
        *guard = settings.clone();
    }

    state
        .audio_denoise_flag
        .store(settings.audio.ai_noise_suppression, Ordering::Relaxed);

    if let Some(tx) = state.active_tx.lock().await.clone() {
        tx.set_fec_group_size(fec_group);
        tx.set_bitrate_policy(clamped_bps, settings.video.unlimited_bitrate);
    }

    let mut applied_live = false;
    if let Ok(guard) = state.active_pipeline.lock()
        && let Some(ref pipeline) = *guard
    {
        if let Some(ref ac) = pipeline.audio_capture {
            ac.set_rnnoise_enabled(settings.audio.ai_noise_suppression);
        }
        if let Some(ref ctrl) = pipeline.host_controls {
            ctrl.desired_max_w.store(max_w, Ordering::Relaxed);
            ctrl.desired_max_h.store(max_h, Ordering::Relaxed);
            ctrl.desired_fps.store(clamped_fps, Ordering::Relaxed);
            ctrl.desired_encoder_mode.store(enc_mode, Ordering::Relaxed);
            ctrl.sharpness
                .store(settings.video.sharpness.min(100), Ordering::Relaxed);
            ctrl.upscaling.store(
                crate::quality::UpscalingMode::parse(&settings.video.upscaling).to_u8(),
                Ordering::Relaxed,
            );
            ctrl.pending_reconfigure.store(true, Ordering::Release);
            applied_live = true;
        }
    }

    if let Ok(mut last_cfg) = state.last_host_config.lock()
        && let Some(ref mut cfg) = *last_cfg
    {
        cfg.params.resolution = settings.video.resolution.clone();
        cfg.params.framerate = clamped_fps;
        cfg.params.max_bitrate_mbps = settings.video.bitrate_mbps.clamp(1, 200);
        cfg.params.encoder = settings.video.encoder.clone();
        cfg.params.codec = settings.video.codec.clone();
        cfg.params.chroma = settings.video.chroma.clone();
        cfg.params.lock_resolution = settings.video.lock_resolution;
        cfg.params.constant_fps = settings.video.constant_fps;
        cfg.params.unlimited_bitrate = settings.video.unlimited_bitrate;
        cfg.params.nvenc = settings.video.nvenc;
        cfg.params.sharpness = settings.video.sharpness;
        cfg.params.upscaling = settings.video.upscaling.clone();
        cfg.params.frame_pacing = settings.video.frame_pacing;
        cfg.params.jitter_window_ms = settings.network.packet_window_ms;
        cfg.params.ai_noise_suppression = settings.audio.ai_noise_suppression;
        cfg.params.audio_bitrate_kbps = settings.audio.bitrate_kbps;
        cfg.params.protocol = settings.network.protocol.clone();
        cfg.params.fec_redundancy_pct = settings.network.fec_redundancy_pct;
        cfg.params.hardware_cursor = settings.network.hardware_cursor;
    }

    if applied_live {
        emit_packet_log(
            &app,
            "SETTINGS",
            "SYNC",
            format!(
                "{} @ {} FPS · {} Mbps · FEC {}% · RNNoise {}",
                settings.video.resolution,
                clamped_fps,
                settings.video.bitrate_mbps.clamp(1, 200),
                settings.network.fec_redundancy_pct,
                if settings.audio.ai_noise_suppression {
                    "on"
                } else {
                    "off"
                }
            ),
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn send_probe_packet(
    app: AppHandle,
    state: State<'_, AppState>,
    channel: String,
) -> Result<(), String> {
    let tx = state
        .active_tx
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no active P2P session".to_string())?;
    let seq = state.seq_counter.fetch_add(1, Ordering::Relaxed);
    let ts_us = now_ms() * 1000;

    match channel.as_str() {
        "video" => {
            tx.video_tx
                .send(OutboundVideoFrame {
                    meta: crate::telemetry::FrameTelemetry::full_range_hardware(
                        1, 1920, 1080,
                    ),
                    frame_id: seq as u32,
                    timestamp_us: ts_us,
                    captured_at: Instant::now(),
                    fps: 60,
                    codec: VideoCodec::Hevc,
                    is_keyframe: seq % 30 == 1,
                    payload: Bytes::from(vec![0x5A; 16_384]),
                })
                .await
                .map_err(|e| e.to_string())?;
            emit_packet_log(
                &app,
                "VIDEO",
                "TX",
                format!("Test HEVC frame #{seq} (16,384 B)"),
            );
        }
        "audio" => {
            tx.audio_tx
                .send(AudioPacket {
                    header: AudioPacketHeader {
                        codec: AudioCodec::Opus as u8,
                        flags: audio_flags::DENOISED | audio_flags::VOICE_TRACK,
                        channels: 2,
                        seq: seq as u32,
                        timestamp_us: ts_us,
                    },
                    payload: Bytes::from_static(b"vmax-rnnoise-opus-10ms-frame"),
                })
                .await
                .map_err(|e| e.to_string())?;
            emit_packet_log(
                &app,
                "AUDIO",
                "TX",
                format!("Test Opus+RNNoise packet #{seq}"),
            );
        }
        "input" => {
            tx.input_tx
                .send(build_input_packet(
                    seq,
                    FrontendInputPayload::MouseMoveRel { dx: 14, dy: -6 },
                ))
                .await
                .map_err(|e| e.to_string())?;
            emit_packet_log(
                &app,
                "INPUT",
                "TX",
                format!("Test MouseMoveRel(14, -6) seq #{seq}"),
            );
        }
        other => return Err(format!("unknown channel '{other}'")),
    }
    Ok(())
}