/**
 * Full settings presets.
 *
 * A preset is a complete, named snapshot of everything that decides how a
 * stream feels — video, network and audio — not a loose pile of clips. The
 * point is that "make it smooth" is one click, not a tour through six tabs
 * where changing one control quietly undoes another.
 *
 * Appearance and controller ergonomics (theme, touch control size, haptics)
 * are deliberately *not* part of a preset: they describe this device, not the
 * stream, and swapping them when the network changes would be surprising.
 */
import type {
  AudioSettings,
  NetworkSettings,
  NvencSettings,
  StreamVideoSettings,
  VmaXConfig,
} from "../types";

/** Everything a preset owns. */
export interface PresetSettings {
  video: StreamVideoSettings;
  network: NetworkSettings;
  audio: AudioSettings;
  /** Diagnostic overlay state; part of how a session is set up, not a device preference. */
  statsOverlay: boolean;
}

export interface SettingsPreset extends PresetSettings {
  id: string;
  name: string;
  /** One line shown while this preset is the active one. */
  summary: string;
  builtin: boolean;
}

/** Video overrides where `nvenc` may itself be partial. */
type PresetVideoOverride = Omit<Partial<StreamVideoSettings>, "nvenc"> & {
  nvenc?: Partial<NvencSettings>;
};

const NVENC_KEYS = [
  "preset",
  "vbr",
  "vbvFrames",
  "aqStrength",
  "minQp",
  "quarterPass",
  "intraRefresh",
  "refreshPeriod",
  "refreshFrames",
] as const satisfies readonly (keyof NvencSettings)[];

function cloneAudio(audio: AudioSettings): AudioSettings {
  // The output device id names hardware on *this* host, so it must never travel
  // inside a preset.
  return { ...audio, outputDeviceId: "" };
}

function makePreset(
  baseConfig: VmaXConfig,
  id: string,
  name: string,
  summary: string,
  video: PresetVideoOverride,
  network: Partial<NetworkSettings>,
  audio: Partial<AudioSettings> = {},
): SettingsPreset {
  return {
    id,
    name,
    summary,
    builtin: true,
    video: {
      ...baseConfig.video,
      ...video,
      nvenc: { ...baseConfig.video.nvenc, ...(video.nvenc ?? {}) },
    },
    network: { ...baseConfig.network, ...network },
    audio: { ...cloneAudio(baseConfig.audio), ...audio },
    statsOverlay: true,
  };
}

/**
 * The built-in preset list.
 *
 * `baseConfig` is passed in rather than imported so this module never depends
 * on the settings loader: a preset is a delta against the shipped defaults, not
 * a second copy of them.
 */
export function builtinPresets(baseConfig: VmaXConfig): SettingsPreset[] {
  const make = (
    id: string,
    name: string,
    summary: string,
    video: PresetVideoOverride,
    network: Partial<NetworkSettings>,
    audio: Partial<AudioSettings> = {},
  ) => makePreset(baseConfig, id, name, summary, video, network, audio);

  return [
    make(
      "smooth",
      "Smooth · recommended",
      "The shipped tuning, one click away: 1080p60 at an adaptive 20 Mbps that softens before it drops a frame, a 32 ms repair window to absorb Wi-Fi bursts, and a cursor composited by the host.",
      {
        resolution: "1080p",
        framerate: 60,
        bitrateMbps: 20,
        framePacing: "balanced",
        sharpness: 25,
        viewerSharpness: 55,
        upscaling: "off",
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: false,
      },
      { protocol: "quic", fecRedundancyPct: 5, hardwareCursor: true, packetWindowMs: 32, congestionSensitivity: "balanced" },
    ),
    make(
      "latency",
      "Lowest latency",
      "Glass-to-glass priority: no FEC overhead, the shortest reassembly window, and minimal post-processing so the picture reaches the panel as early as possible.",
      {
        resolution: "1080p",
        framerate: 60,
        bitrateMbps: 28,
        framePacing: "motion",
        sharpness: 10,
        viewerSharpness: 25,
        upscaling: "off",
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: false,
      },
      { protocol: "quic", fecRedundancyPct: 0, hardwareCursor: true, packetWindowMs: 20, congestionSensitivity: "low_latency" },
    ),
    make(
      "esports",
      "Esports · 120 FPS",
      "For a high-refresh panel: motion-first pacing, a 16 ms packet window and a 120 FPS cadence that keeps frame time under 8.3 ms without starving the encoder. The 35 Mbps ceiling still backs off when the link does — lower bitrate before added lag.",
      {
        resolution: "1080p",
        framerate: 120,
        bitrateMbps: 35,
        framePacing: "motion",
        sharpness: 15,
        viewerSharpness: 25,
        upscaling: "off",
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: false,
        nvenc: { preset: 3, minQp: 4, aqStrength: 4 },
      },
      { protocol: "quic", fecRedundancyPct: 0, hardwareCursor: true, packetWindowMs: 16, congestionSensitivity: "low_latency" },
    ),
    make(
      "quality",
      "Sharp 1440p",
      "Detail-first: 1440p at a high bitrate per pixel, stronger reconstruction and sharper text. Needs a link that can hold 45 Mbps.",
      {
        resolution: "1440p",
        framerate: 60,
        bitrateMbps: 45,
        framePacing: "detail",
        sharpness: 35,
        viewerSharpness: 60,
        upscaling: "off",
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: false,
        nvenc: { preset: 4, minQp: 2, aqStrength: 8 },
      },
      { protocol: "quic", fecRedundancyPct: 5, hardwareCursor: true, packetWindowMs: 32, congestionSensitivity: "quality" },
    ),
    make(
      "lan",
      "LAN showcase · 4K",
      "Local network only: 4K60 with headroom to spare, no FEC tax, and resolution locked so the picture never softens mid-session.",
      {
        resolution: "4K",
        framerate: 60,
        bitrateMbps: 120,
        framePacing: "detail",
        sharpness: 30,
        viewerSharpness: 45,
        upscaling: "off",
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: true,
        nvenc: { preset: 4, minQp: 2, aqStrength: 10 },
      },
      { protocol: "quic", fecRedundancyPct: 0, hardwareCursor: true, packetWindowMs: 32, congestionSensitivity: "quality" },
    ),
    make(
      "wifi",
      "Weak Wi-Fi / relay",
      "Small frames, more parity and a longer repair window. The picture holds together instead of freezing on every lost datagram.",
      {
        resolution: "720p",
        framerate: 60,
        bitrateMbps: 10,
        framePacing: "balanced",
        sharpness: 15,
        viewerSharpness: 40,
        upscaling: "off",
        lockResolution: true,
        constantFps: false,
        unlimitedBitrate: false,
      },
      { protocol: "quic", fecRedundancyPct: 12, hardwareCursor: true, packetWindowMs: 48, congestionSensitivity: "balanced" },
    ),
    make(
      "desktop",
      "Remote desktop",
      "Static-screen friendly: no wasted frames while nothing moves, maximum text sharpness, and a local pointer that never trails the hand.",
      {
        resolution: "1080p",
        framerate: 60,
        bitrateMbps: 20,
        framePacing: "detail",
        sharpness: 40,
        viewerSharpness: 70,
        upscaling: "off",
        lockResolution: true,
        constantFps: false,
        unlimitedBitrate: false,
      },
      { protocol: "quic", fecRedundancyPct: 5, hardwareCursor: false, packetWindowMs: 32, congestionSensitivity: "balanced" },
    ),
  ];
}

const STORAGE_KEY = "vmax.presets.v1";
/** Enough room for a handful of setups without turning the chip row into a wall. */
export const MAX_CUSTOM_PRESETS = 12;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Restores user presets, dropping anything that no longer fits the settings schema. */
export function loadCustomPresets(): SettingsPreset[] {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((entry): entry is SettingsPreset => {
        if (!isRecord(entry)) return false;
        return (
          typeof entry.id === "string" &&
          typeof entry.name === "string" &&
          isRecord(entry.video) &&
          isRecord(entry.network) &&
          isRecord(entry.audio) &&
          isRecord(entry.video.nvenc)
        );
      })
      .map((entry) => ({
        ...entry,
        builtin: false,
        summary: typeof entry.summary === "string" ? entry.summary : "",
        statsOverlay: entry.statsOverlay !== false,
        audio: cloneAudio(entry.audio),
      }))
      .slice(-MAX_CUSTOM_PRESETS);
  } catch {
    return [];
  }
}

export function persistCustomPresets(presets: SettingsPreset[]): void {
  try {
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify(presets.filter((preset) => !preset.builtin)),
    );
  } catch {
    // Storage can be unavailable in a locked-down WebView; presets still work
    // for the lifetime of the window.
  }
}

function clamp(value: number, min: number, max: number, fallback: number): number {
  if (typeof value !== "number" || Number.isNaN(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}

/** Human-readable description of any configuration, used for saved presets. */
export function describeConfig(config: VmaXConfig): string {
  const above = config.network.fecRedundancyPct > 0 ? ` · ${config.network.fecRedundancyPct}% FEC` : "";
  const pacing =
    config.video.framePacing === "motion"
      ? "motion-first"
      : config.video.framePacing === "detail"
        ? "detail-first"
        : "balanced";
  const rate = config.video.unlimitedBitrate ? "fixed" : "adaptive";
  return `${config.video.resolution} · ${config.video.framerate} FPS · ${config.video.bitrateMbps} Mbps ${rate} · ${pacing}${above}`;
}

/** Snapshots the current configuration under a new name. */
export function snapshotPreset(config: VmaXConfig, name: string): SettingsPreset {
  const trimmed = name.trim().slice(0, 40);
  return {
    id: `custom-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 7)}`,
    name: trimmed || "Custom preset",
    summary: describeConfig(config),
    builtin: false,
    video: { ...config.video, nvenc: { ...config.video.nvenc } },
    network: { ...config.network },
    audio: cloneAudio(config.audio),
    statsOverlay: config.customization.statsOverlay,
  };
}

/**
 * True when the live configuration is exactly what `preset` would apply.
 *
 * Used only to highlight the active chip. Comparing the fields a preset owns
 * means the highlight disappears the moment the user changes something, which
 * is the honest signal: the stream is no longer that preset.
 */
export function presetMatches(preset: SettingsPreset, config: VmaXConfig): boolean {
  const v = config.video;
  const p = preset.video;
  return (
    v.resolution === p.resolution &&
    v.framerate === p.framerate &&
    v.bitrateMbps === p.bitrateMbps &&
    v.encoder === p.encoder &&
    v.codec === p.codec &&
    v.chroma === p.chroma &&
    v.framePacing === p.framePacing &&
    v.sharpness === p.sharpness &&
    v.viewerSharpness === p.viewerSharpness &&
    v.upscaling === p.upscaling &&
    v.lockResolution === p.lockResolution &&
    v.constantFps === p.constantFps &&
    v.unlimitedBitrate === p.unlimitedBitrate &&
    NVENC_KEYS.every((key) => v.nvenc[key] === p.nvenc[key]) &&
    config.network.protocol === preset.network.protocol &&
    config.network.fecRedundancyPct === preset.network.fecRedundancyPct &&
    config.network.hardwareCursor === preset.network.hardwareCursor &&
    config.network.packetWindowMs === preset.network.packetWindowMs &&
    config.audio.bitrateKbps === preset.audio.bitrateKbps &&
    config.audio.aiNoiseSuppression === preset.audio.aiNoiseSuppression &&
    // A preset carries the whole audio configuration, so a change to what is
    // captured ends the preset just like a change to the codec does. The output
    // device is excluded because it names hardware on this host.
    config.audio.streamComputerAudio === preset.audio.streamComputerAudio &&
    config.audio.gameOnly === preset.audio.gameOnly &&
    config.audio.excludeDiscord === preset.audio.excludeDiscord &&
    config.customization.statsOverlay === preset.statsOverlay
  );
}

/**
 * Merges a preset into the live configuration.
 *
 * Device-local values survive: the audio output device id names hardware on
 * this host, and appearance/controller preferences belong to the person using
 * it, not to the network preset.
 */
export function applyPresetToConfig(config: VmaXConfig, preset: SettingsPreset): VmaXConfig {
  return {
    ...config,
    video: {
      ...preset.video,
      nvenc: { ...preset.video.nvenc },
      // A stored preset may predate a new option; keep the live value then.
      renderer: config.video.renderer,
      bitrateMbps: clamp(preset.video.bitrateMbps, 1, 200, config.video.bitrateMbps),
    },
    network: { ...preset.network },
    audio: {
      ...preset.audio,
      outputDeviceId: config.audio.outputDeviceId,
      gamePid: "",
    },
    customization: { ...config.customization, statsOverlay: preset.statsOverlay },
  };
}
