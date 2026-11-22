import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  AudioBitrateKbps,
  AudioSettings,
  CustomizationSettings,
  EncoderMode,
  FramerateOption,
  GraphicsBackend,
  NetworkSettings,
  NvencSettings,
  ResolutionOption,
  StreamVideoSettings,
  TransportProtocol,
  UpscalingMode,
  VmaXConfig,
} from "../types";
import {
  type SettingsPreset,
  MAX_CUSTOM_PRESETS,
  applyPresetToConfig,
  builtinPresets,
  loadCustomPresets,
  persistCustomPresets,
  presetMatches,
  snapshotPreset,
} from "./presets";

const STORAGE_KEY = "vmax.config.v1";

export const MAX_STREAM_BITRATE_MBPS = 200;
export const STREAM_FRAMERATES = [30, 60, 90, 120, 144] as const;

const DEFAULT_NVENC: NvencSettings = {
  preset: 4,
  vbr: false,
  vbvFrames: 1,
  aqStrength: 8,
  minQp: 5,
  quarterPass: true,
  intraRefresh: true,
  refreshPeriod: 120,
  refreshFrames: 1,
};

export const DEFAULT_VMAX_CONFIG: VmaXConfig = {
  video: {
    resolution: "1080p",
    framerate: 60,
    bitrateMbps: 24,
    hostUploadMbps: 30,
    strictQuality: false,
    encoder: "hardware",
    codec: "hevc",
    chroma: "420",
    framePacing: "motion",
    sharpness: 30,
    viewerSharpness: 45,
    upscaling: "off",
    renderer: "auto",
    lockResolution: true,
    constantFps: true,
    unlimitedBitrate: false,
    nvenc: DEFAULT_NVENC,
  },
  audio: {
    streamComputerAudio: true,
    outputDeviceId: "",
    gameOnly: false,
    excludeDiscord: false,
    gamePid: "",
    aiNoiseSuppression: false,
    bitrateKbps: 192,
  },
  network: {
    protocol: "auto",
    // Parity shards are cheap insurance on the relayed path, where one lost
    // packet is a TCP retransmit stall rather than an isolated datagram.
    fecRedundancyPct: 5,
    hardwareCursor: true,
    // Complete frames bypass the repair window.
    packetWindowMs: 16,
    congestionSensitivity: "low_latency",
  },
  customization: {
    statsOverlay: true,
    theme: "neon",
    controllerOpacity: 85,
    controllerSize: 100,
    hapticsEnabled: true,
    hapticsIntensity: 100,
  },
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function asResolution(val: unknown, fallback: ResolutionOption): ResolutionOption {
  return val === "native" || val === "720p" || val === "1080p" || val === "1440p" || val === "4K"
    ? val
    : fallback;
}

function asFramerate(val: unknown, fallback: FramerateOption): FramerateOption {
  return val === 30 || val === 60 || val === 90 || val === 120 || val === 144 ? val : fallback;
}

function asEncoder(val: unknown, fallback: EncoderMode): EncoderMode {
  return val === "hardware" || val === "software" ? val : fallback;
}

function asAudioBitrate(val: unknown, fallback: AudioBitrateKbps): AudioBitrateKbps {
  return val === 96 || val === 128 || val === 192 || val === 320 ? val : fallback;
}

function asProtocol(val: unknown, fallback: TransportProtocol): TransportProtocol {
  return val === "auto" || val === "quic" || val === "raw_udp" ? val : fallback;
}

function asGraphicsBackend(val: unknown, fallback: GraphicsBackend): GraphicsBackend {
  return val === "auto" || val === "d3d11" || val === "d3d12" || val === "vulkan" ? val : fallback;
}

function asUpscaling(val: unknown, fallback: UpscalingMode): UpscalingMode {
  return val === "off" || val === "auto" || val === "ultra_quality" || val === "quality"
    || val === "balanced" || val === "performance"
    ? val
    : fallback;
}

function asClampedNumber(val: unknown, min: number, max: number, fallback: number): number {
  if (typeof val !== "number" || Number.isNaN(val)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, val));
}

function asBoolean(val: unknown, fallback: boolean): boolean {
  return typeof val === "boolean" ? val : fallback;
}

// Preserve the saved slider value. New profiles use the default.
function migrateViewerSharpness(value: unknown): number {
  return Math.round(
    asClampedNumber(value, 0, 100, DEFAULT_VMAX_CONFIG.video.viewerSharpness),
  );
}

/**
 * Settings written before the intra-refresh retune carry the 60/5 pair that
 * produced a visible quality pulse. Untouched values are moved to the thin
 * continuous band; anything the user picked themselves is left alone.
 */
function migrateRefreshPeriod(value: unknown): number {
  const period = Math.round(asClampedNumber(value, 2, 600, DEFAULT_NVENC.refreshPeriod));
  return period === 60 ? DEFAULT_NVENC.refreshPeriod : period;
}

function migrateRefreshFrames(period: unknown, frames: unknown): number {
  const storedPeriod = Math.round(asClampedNumber(period, 2, 600, DEFAULT_NVENC.refreshPeriod));
  const clamped = Math.round(asClampedNumber(frames, 1, 60, DEFAULT_NVENC.refreshFrames));
  return storedPeriod === 60 && clamped === 5 ? DEFAULT_NVENC.refreshFrames : clamped;
}

/**
 * The shipped 5.0.0 baseline was 1080p/60 at 20 Mbps, balanced pacing, 5% FEC
 * and a 32 ms window. A stored config that still matches that exact shape was
 * never customised, so moving it to the native/60/low-latency baseline is the
 * same as shipping a new default. Anything the user changed stays untouched.
 */
function migrateToNativeBaseline(config: VmaXConfig): VmaXConfig {
  const v = config.video;
  const n = config.network;
  const untouchedShippedBaseline =
    v.resolution === "1080p" &&
    v.framerate === 60 &&
    v.bitrateMbps === 20 &&
    v.framePacing === "balanced" &&
    v.sharpness === 25 &&
    v.viewerSharpness === 55 &&
    n.fecRedundancyPct === 5 &&
    n.packetWindowMs === 32 &&
    n.congestionSensitivity === "balanced";
  if (!untouchedShippedBaseline) {
    return migrateToGamingBaseline(config);
  }
  // Only picture policy moves here. The 20 Mbps / 5% FEC / 32 ms budget is
  // already what a relayed internet session needs, so it is left alone.
  return migrateToGamingBaseline({
    ...config,
    video: {
      ...v,
      resolution: "native",
      framePacing: "motion",
      sharpness: 20,
      viewerSharpness: 30,
    },
    network: {
      ...n,
      congestionSensitivity: "low_latency",
    },
  });
}

/** Upgrade the previously shipped picture profiles when their settings still match. */
function migrateToGamingBaseline(config: VmaXConfig): VmaXConfig {
  const v = config.video;
  const n = config.network;
  const untouchedNativeBaseline =
    v.resolution === "native" &&
    v.framerate === 60 &&
    v.bitrateMbps === 20 &&
    v.framePacing === "motion" &&
    v.sharpness === 20 &&
    v.viewerSharpness === 30 &&
    v.upscaling === "off" &&
    v.lockResolution &&
    v.constantFps &&
    !v.unlimitedBitrate &&
    n.fecRedundancyPct === 5 &&
    n.packetWindowMs === 32 &&
    n.congestionSensitivity === "low_latency";
  const previousGamingBaseline =
    v.resolution === "native" && v.framerate === 120 && v.bitrateMbps === 60 &&
    v.codec === "auto" && v.encoder === "hardware" && v.chroma === "420" &&
    v.framePacing === "motion" && v.upscaling === "off" && v.renderer === "auto" &&
    v.sharpness === 30 && v.viewerSharpness === 55 &&
    v.lockResolution && v.constantFps && !v.unlimitedBitrate &&
    n.fecRedundancyPct === 5 && n.packetWindowMs === 16 &&
    n.congestionSensitivity === "low_latency";
  if (!untouchedNativeBaseline && !previousGamingBaseline) {
    return config;
  }
  return {
    ...config,
    video: {
      ...v,
      resolution: "1080p",
      codec: "hevc",
      framerate: 60,
      bitrateMbps: 24,
      sharpness: 30,
      viewerSharpness: 55,
    },
    audio: {
      ...config.audio,
      // Only lift audio if it was never touched either.
      bitrateKbps: untouchedNativeBaseline && config.audio.bitrateKbps === 128 ? 192 : config.audio.bitrateKbps,
    },
    network: {
      ...n,
      packetWindowMs: 16,
    },
  };
}

function loadSavedConfig(): VmaXConfig {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return DEFAULT_VMAX_CONFIG;
    }
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) {
      return DEFAULT_VMAX_CONFIG;
    }

    const v = isRecord(parsed.video) ? parsed.video : {};
    const nvenc = isRecord(v.nvenc) ? v.nvenc : {};
    const a = isRecord(parsed.audio) ? parsed.audio : {};
    const n = isRecord(parsed.network) ? parsed.network : {};
    const c = isRecord(parsed.customization) ? parsed.customization : {};

    const config: VmaXConfig = {
      video: {
        resolution: asResolution(v.resolution, DEFAULT_VMAX_CONFIG.video.resolution),
        framerate: asFramerate(v.framerate, DEFAULT_VMAX_CONFIG.video.framerate),
        hostUploadMbps: Math.round(asClampedNumber(v.hostUploadMbps, 1, 1000, 30)),
        strictQuality: asBoolean(v.strictQuality, false),
        bitrateMbps: asClampedNumber(v.bitrateMbps, 1, MAX_STREAM_BITRATE_MBPS, DEFAULT_VMAX_CONFIG.video.bitrateMbps),
        encoder: asEncoder(v.encoder, DEFAULT_VMAX_CONFIG.video.encoder),
        chroma: v.chroma === "444" ? "444" : "420",
        codec:
          v.codec === "h264" || v.codec === "hevc" || v.codec === "hevc_main10" || v.codec === "av1"
            ? v.codec
            : "auto",
        framePacing: v.framePacing === "motion" || v.framePacing === "detail" ? v.framePacing : "balanced",
        sharpness: Math.round(asClampedNumber(v.sharpness, 0, 100, DEFAULT_VMAX_CONFIG.video.sharpness)),
        viewerSharpness: migrateViewerSharpness(v.viewerSharpness),
        upscaling: asUpscaling(v.upscaling, DEFAULT_VMAX_CONFIG.video.upscaling),
        renderer: asGraphicsBackend(v.renderer, DEFAULT_VMAX_CONFIG.video.renderer),
        lockResolution: asBoolean(v.lockResolution, DEFAULT_VMAX_CONFIG.video.lockResolution),
        constantFps: asBoolean(v.constantFps, DEFAULT_VMAX_CONFIG.video.constantFps),
        unlimitedBitrate: asBoolean(v.unlimitedBitrate, DEFAULT_VMAX_CONFIG.video.unlimitedBitrate),
        nvenc: {
          preset: nvenc.preset === 3 ? 3 : 4,
          vbr: false,
          vbvFrames: 1,
          aqStrength: Math.round(asClampedNumber(nvenc.aqStrength, 0, 15, DEFAULT_NVENC.aqStrength)),
          minQp: 5,
          quarterPass: asBoolean(nvenc.quarterPass, DEFAULT_NVENC.quarterPass),
          intraRefresh: asBoolean(nvenc.intraRefresh, DEFAULT_NVENC.intraRefresh),
          refreshPeriod: migrateRefreshPeriod(nvenc.refreshPeriod),
          refreshFrames: migrateRefreshFrames(nvenc.refreshPeriod, nvenc.refreshFrames),
        },
      },
      audio: {
        streamComputerAudio: asBoolean(a.streamComputerAudio, true),
        outputDeviceId: typeof a.outputDeviceId === "string" ? a.outputDeviceId : "",
        gameOnly: asBoolean(a.gameOnly, false),
        excludeDiscord: asBoolean(a.excludeDiscord, false),
        gamePid: "",
        aiNoiseSuppression: asBoolean(
          a.aiNoiseSuppression,
          DEFAULT_VMAX_CONFIG.audio.aiNoiseSuppression,
        ),
        bitrateKbps: asAudioBitrate(a.bitrateKbps, DEFAULT_VMAX_CONFIG.audio.bitrateKbps),
      },
      network: {
        protocol: asProtocol(n.protocol, DEFAULT_VMAX_CONFIG.network.protocol),
        fecRedundancyPct: asClampedNumber(
          n.fecRedundancyPct,
          0,
          20,
          DEFAULT_VMAX_CONFIG.network.fecRedundancyPct,
        ),
        hardwareCursor: asBoolean(n.hardwareCursor, DEFAULT_VMAX_CONFIG.network.hardwareCursor),
        packetWindowMs: Math.round(asClampedNumber(n.packetWindowMs, 8, 80, DEFAULT_VMAX_CONFIG.network.packetWindowMs)),
        congestionSensitivity: (n.congestionSensitivity === "low_latency" || n.congestionSensitivity === "quality" || n.congestionSensitivity === "balanced")
          ? n.congestionSensitivity
          : DEFAULT_VMAX_CONFIG.network.congestionSensitivity,
      },
      customization: {
        statsOverlay: asBoolean(c.statsOverlay, DEFAULT_VMAX_CONFIG.customization.statsOverlay),
        theme: c.theme === "blue" || c.theme === "mono" ? c.theme : "neon",
        controllerOpacity: asClampedNumber(c.controllerOpacity, 35, 100, 85),
        controllerSize: asClampedNumber(c.controllerSize, 75, 115, 100),
        hapticsEnabled: asBoolean(c.hapticsEnabled, true),
        hapticsIntensity: Math.round(asClampedNumber(c.hapticsIntensity, 0, 100, 100)),
      },
    };
    return v.hostUploadMbps === undefined ? migrateToNativeBaseline(config) : config;
  } catch {
    return DEFAULT_VMAX_CONFIG;
  }
}

export function useVmaXConfig() {
  const [config, setConfig] = useState<VmaXConfig>(() => loadSavedConfig());
  const [customPresets, setCustomPresets] = useState<SettingsPreset[]>(() => loadCustomPresets());
  const builtins = useMemo(() => builtinPresets(DEFAULT_VMAX_CONFIG), []);
  const presets = useMemo(() => [...builtins, ...customPresets], [builtins, customPresets]);
  // `savePreset` snapshots whatever is live right now, without making the
  // callback depend on the config object (which would rebuild every consumer).
  const configRef = useRef(config);
  configRef.current = config;
  // Presets are read from a ref inside `applyPreset` so the callback identity is
  // stable: the modal re-renders on every keystroke and must not rebuild it.
  const presetsRef = useRef(presets);
  presetsRef.current = presets;

  useEffect(() => {
    try {
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(config));
    } catch {
      // Ignore storage quota errors in restricted WebViews.
    }
  }, [config]);

  useEffect(() => {
    persistCustomPresets(customPresets);
  }, [customPresets]);

  /** Which preset, if any, exactly describes the live configuration. */
  const activePresetId = useMemo(
    () => presets.find((preset) => presetMatches(preset, config))?.id ?? null,
    [presets, config],
  );

  const updateVideo = useCallback((patch: Partial<StreamVideoSettings>) => {
    setConfig((prev) => ({
      ...prev,
      video: { ...prev.video, ...patch },
    }));
  }, []);

  const updateAudio = useCallback((patch: Partial<AudioSettings>) => {
    setConfig((prev) => ({
      ...prev,
      audio: { ...prev.audio, ...patch },
    }));
  }, []);

  const updateNetwork = useCallback((patch: Partial<NetworkSettings>) => {
    setConfig((prev) => ({
      ...prev,
      network: { ...prev.network, ...patch },
    }));
  }, []);

  const updateCustomization = useCallback((patch: Partial<CustomizationSettings>) => {
    setConfig((prev) => ({
      ...prev,
      customization: { ...prev.customization, ...patch },
    }));
  }, []);

  const resetConfig = useCallback(() => {
    setConfig((prev) => ({
      ...DEFAULT_VMAX_CONFIG,
      customization: { ...DEFAULT_VMAX_CONFIG.customization, theme: prev.customization.theme },
    }));
  }, []);

  /**
   * Switches the whole stream configuration to `preset`.
   *
   * Every field a preset owns is replaced in one state update, so the user
   * never sees an intermediate mix of two presets where, say, the FPS is new
   * but the bitrate is old and the pipeline has to re-plan twice.
   */
  const applyPreset = useCallback((id: string) => {
    setConfig((prev) => {
      const preset = presetsRef.current.find((entry) => entry.id === id);
      return preset ? applyPresetToConfig(prev, preset) : prev;
    });
  }, []);

  /** Saves the live configuration as a new custom preset. */
  const savePreset = useCallback((name: string) => {
    const preset = snapshotPreset(configRef.current, name);
    setCustomPresets((prev) => [...prev, preset].slice(-MAX_CUSTOM_PRESETS));
    return preset.id;
  }, []);

  const deletePreset = useCallback((id: string) => {
    setCustomPresets((prev) => prev.filter((preset) => preset.id !== id));
  }, []);

  const applyAutomaticConfig = useCallback(() => {
    setConfig((prev) => ({
      ...prev,
      video: {
        ...prev.video,
        encoder: "hardware",
        codec: "auto",
        constantFps: true,
        lockResolution: true,
        unlimitedBitrate: false,
        upscaling: "off",
        nvenc: DEFAULT_NVENC,
      },
      network: {
        ...prev.network,
        protocol: "quic",
        fecRedundancyPct: prev.network.fecRedundancyPct || 5,
      },
    }));
  }, []);

  const applyLongDistanceConfig = useCallback(() => {
    setConfig((prev) => ({
      ...prev,
      video: {
        ...prev.video,
        encoder: "hardware",
        codec: "auto",
        constantFps: true,
        lockResolution: true,
        unlimitedBitrate: false,
        upscaling: "off",
      },
      network: {
        ...prev.network,
        protocol: "quic",
        fecRedundancyPct: Math.max(12, prev.network.fecRedundancyPct),
        packetWindowMs: Math.max(45, prev.network.packetWindowMs),
      },
    }));
  }, []);

  return {
    config,
    presets,
    activePresetId,
    applyPreset,
    savePreset,
    deletePreset,
    updateVideo,
    updateAudio,
    updateNetwork,
    updateCustomization,
    resetConfig,
    applyAutomaticConfig,
    applyLongDistanceConfig,
  };
}

