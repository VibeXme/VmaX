export type SessionRole = "host" | "client";
export type ConnectionPath = "direct_lan" | "direct_internet" | "turn" | "relay";

export function connectionPathLabel(path: ConnectionPath | null | undefined, relayed: boolean) {
  if (path === "turn") return "UDP Relay";
  if (relayed || path === "relay") return "Relay";
  if (path === "direct_lan") return "Direct LAN";
  if (path === "direct_internet") return "Direct Internet";
  return "Direct";
}

export type ConnectionPhase =
  | "idle"
  | "discovering_nat"
  | "waiting_for_peer"
  | "hole_punching"
  | "quic_handshake"
  | "connected"
  | "disconnected"
  | "failed";

export type CongestionState =
  | "stable"
  | "ramping_up"
  | "mild_congestion"
  | "severe_congestion"
  | "cooldown";

export interface NetworkTelemetry {
  role: SessionRole | null;
  phase: ConnectionPhase;
  room_code: string | null;
  invite_uri: string | null;
  local_udp_addr: string | null;
  public_reflexive_addr: string | null;
  active_peer_addr: string | null;
  relayed: boolean;
  connection_path?: ConnectionPath | null;
  saved_session: boolean;
  peer_savable: boolean;
  cert_fingerprint: string | null;
  rtt_ms: number;
  min_rtt_ms: number;
  packet_loss_pct: number;
  peer_packet_loss_pct?: number | null;
  target_bitrate_bps: number;
  congestion_state: CongestionState;
  video_frames_tx: number;
  video_frames_rx: number;
  video_packets_rx?: number;
  video_frames_dropped: number;
  fec_recovered_shards: number;
  audio_packets_tx: number;
  audio_packets_rx: number;
  input_events_tx: number;
  input_events_rx: number;
  last_error: string | null;
}

export interface SavedNetwork {
  name: string;
  endpoint: string;
}

export interface SavedDevice {
  name: string;
  role: "host" | "client";
  fingerprint: string;
  endpoint: string;
  automatic: boolean;
}

export interface SavedConnections {
  networks: SavedNetwork[];
  devices: SavedDevice[];
  autoHostEndpoint: string | null;
}

/** A hosting peer found by scanning the local network. */
export interface DiscoveredHost {
  code: string;
  endpoint: string;
  name: string | null;
  addr: string;
}

/** A joinable room listed by a signaling relay. */
export interface RelayRoom {
  code: string;
  name?: string | null;
}

export type SavedConnectionUpdate =
  | { kind: "save_network"; name: string; endpoint: string }
  | { kind: "remove_network"; endpoint: string }
  | { kind: "remember_peer"; name: string }
  | { kind: "set_device_automatic"; role: SavedDevice["role"]; fingerprint: string; enabled: boolean }
  | { kind: "remove_device"; role: SavedDevice["role"]; fingerprint: string }
  | { kind: "accept_friend_request"; fingerprint: string }
  | { kind: "set_auto_host"; endpoint: string | null };

export interface LocalRelayStatus {
  running: boolean;
  ws_url: string | null;
  lan_ws_url: string | null;
}

export interface PacketLogEntry {
  ts_ms: number;
  channel: string;
  direction: string;
  summary: string;
}

export interface HardwareTelemetry {
  nvencLoadPct: number;
  vramUsedGb: number;
  vramTotalGb: number;
  cpuLoadPct: number;
  encodeLatencyMs: number;
  decodeLatencyMs: number;
  fps: number;
  pipelineLabel?: string;
}

export interface ParsecTelemetryPayload {
  decodeMs: number;
  encodeMs: number;
  networkMs: number;
  bitrateMbps: number;
  pipelineLabel: string;
}

export type TouchAction = "tap" | "down" | "up" | "context" | "drag";

export type RemoteInputPayload =
  | { kind: "mouse_move_rel"; dx: number; dy: number }
  | { kind: "mouse_move_abs"; xNorm: number; yNorm: number }
  | { kind: "touch_pointer"; xNorm: number; yNorm: number; action: TouchAction; tapId: number }
  | { kind: "mouse_button"; button: number; pressed: boolean }
  | { kind: "mouse_wheel"; deltaX: number; deltaY: number }
  | { kind: "key"; scanCode: number; pressed: boolean; modifiers: number };

/**
 * Stream size ceiling. `native` keeps the host's display resolution (up to
 * 8K); the others cap the encoded picture at a fixed height.
 */
export type ResolutionOption = "native" | "720p" | "1080p" | "1440p" | "4K";
export type FramerateOption = 30 | 60 | 90 | 120 | 144;
export type EncoderMode = "hardware" | "software";
export type FramePacing = "balanced" | "motion" | "detail";
export type AudioBitrateKbps = 96 | 128 | 192 | 320;
export type TransportProtocol = "quic" | "raw_udp";

export type UpscalingMode =
  | "off"
  | "auto"
  | "ultra_quality"
  | "quality"
  | "balanced"
  | "performance";

export interface NvencSettings {
  preset: number;
  vbr: boolean;
  vbvFrames: number;
  aqStrength: number;
  minQp: number;
  quarterPass: boolean;
  intraRefresh: boolean;
  refreshPeriod: number;
  refreshFrames: number;
}

export interface StreamVideoSettings {
  hostUploadMbps: number;
  strictQuality: boolean;
  resolution: ResolutionOption;
  framerate: FramerateOption;
  bitrateMbps: number;
  encoder: EncoderMode;
  framePacing: FramePacing;
  /** Hardware sharpening after scaling, 0-100. */
  sharpness: number;
  viewerSharpness: number;
  /** Spatial reconstruction tier (FSR render scale). */
  upscaling: UpscalingMode;
  /** Graphics backend request: auto | d3d11 | d3d12 | vulkan. */
  renderer: GraphicsBackend;
  /** Stream at the host's resolution; never downscale for bandwidth. */
  lockResolution: boolean;
  /** Keep the target frame rate even when the desktop is static. */
  constantFps: boolean;
  /** Use the whole configured bitrate; ignore congestion back-off. */
  unlimitedBitrate: boolean;
  nvenc: NvencSettings;
  codec: "auto" | "h264" | "hevc" | "hevc_main10" | "av1";
  chroma: "420" | "444";
}

export type GraphicsBackend = "auto" | "d3d11" | "d3d12" | "vulkan";

export interface AudioSettings {
  streamComputerAudio: boolean;
  outputDeviceId: string;
  gameOnly: boolean;
  excludeDiscord: boolean;
  gamePid: string;
  aiNoiseSuppression: boolean;
  bitrateKbps: AudioBitrateKbps;
}

export type CongestionSensitivity = "balanced" | "low_latency" | "quality";

export interface NetworkSettings {
  protocol: TransportProtocol;
  fecRedundancyPct: number;
  hardwareCursor: boolean;
  packetWindowMs: number;
  congestionSensitivity: CongestionSensitivity;
}

export interface CustomizationSettings {
  statsOverlay: boolean;
  theme: "neon" | "blue" | "mono";
  controllerOpacity: number;
  controllerSize: number;
  hapticsEnabled: boolean;
  hapticsIntensity: number;
}

/**
 * Central configuration object designed to synchronize directly with the Rust backend.
 */
export interface VmaXConfig {
  video: StreamVideoSettings;
  audio: AudioSettings;
  network: NetworkSettings;
  customization: CustomizationSettings;
}
