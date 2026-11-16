import { useCallback, useEffect, useState } from "react";
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

const STORAGE_KEY = "vmax.config.v1";

export const MAX_STREAM_BITRATE_MBPS = 200;
export const STREAM_FRAMERATES = [30, 60, 90, 120, 144] as const;

export const STREAM_PRESETS = {
  latency: { label: "Low latency", resolution: "1080p", framerate: 60, bitrateMbps: 25, fec: 0, framePacing: "motion" },
  balanced: { label: "Balanced", resolution: "1080p", framerate: 60, bitrateMbps: 20, fec: 5, framePacing: "balanced" },
  quality: { label: "High quality", resolution: "1080p", framerate: 60, bitrateMbps: 35, fec: 5, framePacing: "detail" },
  mobile: { label: "Unstable network", resolution: "720p", framerate: 60, bitrateMbps: 8, fec: 15, framePacing: "motion" },
} as const;

const DEFAULT_NVENC: NvencSettings = {
  preset: 4,
  vbr: true,
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
    resolution: STREAM_PRESETS.balanced.resolution,
    framerate: STREAM_PRESETS.balanced.framerate,
    bitrateMbps: STREAM_PRESETS.balanced.bitrateMbps,
    encoder: "hardware",
    codec: "auto",
    chroma: "420",
    framePacing: "balanced",
    sharpness: 25,
    viewerSharpness: 40,
    upscaling: "off",
    renderer: "auto",
    lockResolution: true,
    constantFps: true,
    // Adaptive by default. The ceiling is still the user's bitrate, but
    // congestion feedback may lower it: a link that cannot carry 20 Mbps
    // should soften the picture, not drop frames.
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
    bitrateKbps: 128,
  },
  network: {
    protocol: "quic",
    fecRedundancyPct: STREAM_PRESETS.balanced.fec,
    hardwareCursor: true,
    // ~2 frame intervals at 60 Hz. Complete frames never wait for it; only
    // incomplete ones get this long to be repaired by a late packet.
    packetWindowMs: 32,
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
  return val === "720p" || val === "1080p" || val === "1440p" || val === "4K" ? val : fallback;
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
  return val === "quic" || val === "raw_udp" ? val : fallback;
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

    return {
      video: {
        resolution: asResolution(v.resolution, DEFAULT_VMAX_CONFIG.video.resolution),
        framerate: asFramerate(v.framerate, DEFAULT_VMAX_CONFIG.video.framerate),
        bitrateMbps: asClampedNumber(v.bitrateMbps, 1, MAX_STREAM_BITRATE_MBPS, DEFAULT_VMAX_CONFIG.video.bitrateMbps),
        encoder: asEncoder(v.encoder, DEFAULT_VMAX_CONFIG.video.encoder),
        chroma: v.chroma === "444" ? "444" : "420",
        codec: v.codec === "h264" || v.codec === "hevc" || v.codec === "hevc_main10" ? v.codec : "auto",
        framePacing: v.framePacing === "motion" || v.framePacing === "detail" ? v.framePacing : "balanced",
        sharpness: Math.round(asClampedNumber(v.sharpness, 0, 100, DEFAULT_VMAX_CONFIG.video.sharpness)),
        viewerSharpness: Math.round(asClampedNumber(v.viewerSharpness, 0, 100, DEFAULT_VMAX_CONFIG.video.viewerSharpness)),
        upscaling: asUpscaling(v.upscaling, DEFAULT_VMAX_CONFIG.video.upscaling),
        renderer: asGraphicsBackend(v.renderer, DEFAULT_VMAX_CONFIG.video.renderer),
        lockResolution: asBoolean(v.lockResolution, DEFAULT_VMAX_CONFIG.video.lockResolution),
        constantFps: asBoolean(v.constantFps, DEFAULT_VMAX_CONFIG.video.constantFps),
        unlimitedBitrate: asBoolean(v.unlimitedBitrate, DEFAULT_VMAX_CONFIG.video.unlimitedBitrate),
        nvenc: {
          preset: nvenc.preset === 3 ? 3 : 4,
          vbr: asBoolean(nvenc.vbr, DEFAULT_NVENC.vbr),
          vbvFrames: Math.round(asClampedNumber(nvenc.vbvFrames, 1, 4, DEFAULT_NVENC.vbvFrames)),
          aqStrength: Math.round(asClampedNumber(nvenc.aqStrength, 0, 15, DEFAULT_NVENC.aqStrength)),
          minQp: Math.round(asClampedNumber(nvenc.minQp, 0, 51, DEFAULT_NVENC.minQp)),
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
  } catch {
    return DEFAULT_VMAX_CONFIG;
  }
}

export function useVmaXConfig() {
  const [config, setConfig] = useState<VmaXConfig>(() => loadSavedConfig());

  useEffect(() => {
    try {
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(config));
    } catch {
      // Ignore storage quota errors in restricted WebViews.
    }
  }, [config]);

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

  const applyStreamPreset = useCallback((key: keyof typeof STREAM_PRESETS) => {
    const { resolution, framerate, bitrateMbps, fec, framePacing } = STREAM_PRESETS[key];
    setConfig((prev) => ({
      ...prev,
      video: {
        ...prev.video, resolution, framerate, bitrateMbps, framePacing,
        lockResolution: true,
        constantFps: true,
        unlimitedBitrate: key === "quality",
      },
      network: { ...prev.network, protocol: "quic", fecRedundancyPct: fec },
    }));
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
        fecRedundancyPct: prev.network.fecRedundancyPct || STREAM_PRESETS.balanced.fec,
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
    updateVideo,
    updateAudio,
    updateNetwork,
    updateCustomization,
    resetConfig,
    applyStreamPreset,
    applyAutomaticConfig,
    applyLongDistanceConfig,
  };
}

