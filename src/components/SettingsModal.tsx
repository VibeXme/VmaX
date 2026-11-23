import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ChevronDown,
  Cpu,
  Gamepad2,
  LayoutGrid,
  Monitor,
  MousePointer,
  Network,
  RotateCcw,
  Settings,
  SlidersHorizontal,
  ShieldCheck,
  Sparkles,
  Volume2,
  X,
} from "lucide-react";
import type {
  AudioBitrateKbps,
  AudioSettings,
  CustomizationSettings,
  FramePacing,
  NetworkSettings,
  NvencSettings,
  ResolutionOption,
  StreamVideoSettings,
  VmaXConfig,
} from "../types";
import { SpeedometerSvg } from "./BitrateGauge";
import { MAX_STREAM_BITRATE_MBPS, STREAM_FRAMERATES } from "../config/defaults";
import type { SettingsPreset } from "../config/presets";

type SettingsTab = "general" | "video" | "network_audio" | "customization" | "connections" | "advanced" | "controller";

interface SettingsModalProps {
  initialTab: SettingsTab;
  open: boolean;
  onClose: () => void;
  config: VmaXConfig;
  onUpdateVideo: (patch: Partial<StreamVideoSettings>) => void;
  onUpdateAudio: (patch: Partial<AudioSettings>) => void;
  onUpdateNetwork: (patch: Partial<NetworkSettings>) => void;
  onUpdateCustomization: (patch: Partial<CustomizationSettings>) => void;
  onReset: () => void;
  onAutomaticConfig: () => void;
  onLongDistanceConfig: () => void;
  buildVersion: string | null;
  /** Built-in presets followed by the user's own, in display order. */
  presets: readonly SettingsPreset[];
  /** Id of the preset that exactly matches the live configuration, if any. */
  activePresetId: string | null;
  onApplyPreset: (id: string) => void;
  onSavePreset: (name: string) => void;
  onDeletePreset: (id: string) => void;
  savedConnections: React.ReactNode;
}

const NVENC_NUMBER_OPTIONS = [
  { key: "minQp", label: "Minimum QP", hint: "Bounds encoder quality and bitrate on simple content.", min: 0, max: 51 },
  { key: "vbvFrames", label: "VBV buffer in frames", hint: "One frame minimizes rate-control buffering.", min: 1, max: 2 },
  { key: "aqStrength", label: "Adaptive quantization strength", hint: "Shift bits toward detail. Set to 0 to disable.", min: 0, max: 15 },
  { key: "refreshPeriod", label: "Intra refresh period in frames", hint: "Frames between refresh waves.", min: 2, max: 600 },
  { key: "refreshFrames", label: "Intra refresh length in frames", hint: "Frames over which each refresh wave is spread.", min: 1, max: 60 },
] as const;

const CATEGORIES: readonly {
  id: SettingsTab;
  label: string;
  icon: typeof Monitor;
}[] = [
  { id: "general", label: "General", icon: Settings },
  {
    id: "video",
    label: "Streaming",
    icon: Monitor,
  },
  {
    id: "network_audio",
    label: "Network & Audio",
    icon: Network,
  },
  {
    id: "customization",
    label: "Appearance",
    icon: LayoutGrid,
  },
  {
    id: "connections",
    label: "Friends",
    icon: Network,
  },
  {
    id: "controller",
    label: "Controller",
    icon: Gamepad2,
  },
  {
    id: "advanced",
    label: "Advanced",
    icon: SlidersHorizontal,
  },
];

const RESOLUTION_OPTIONS: readonly { value: ResolutionOption; label: string }[] = [
  { value: "native", label: "Native · host display" },
  { value: "720p", label: "720p · HD" },
  { value: "1080p", label: "1080p · FHD" },
  { value: "1440p", label: "1440p · QHD" },
  { value: "4K", label: "4K · UHD" },
];

const AUDIO_BITRATE_OPTIONS: readonly { value: AudioBitrateKbps; label: string }[] = [
  { value: 96, label: "96 kbps" },
  { value: 128, label: "128 kbps" },
  { value: 192, label: "192 kbps" },
  { value: 320, label: "320 kbps" },
];

interface SettingRowProps {
  title: string;
  hint: string;
  icon?: ReactNode;
  children: ReactNode;
}

function SettingRow({ title, hint, icon, children }: SettingRowProps) {
  return (
    <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 rounded-xl border border-zinc-800 bg-zinc-950/80 px-5 py-4">
      <div className="flex items-start gap-3">
        {icon && (
          <div className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-zinc-800 bg-zinc-900 text-zinc-400">
            {icon}
          </div>
        )}
        <div>
          <p className="text-sm font-medium text-white">{title}</p>
          <p className="text-xs text-zinc-500 mt-0.5">{hint}</p>
        </div>
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

interface ToggleSwitchProps {
  checked: boolean;
  onToggle: () => void;
  ariaLabel: string;
}

function ToggleSwitch({ checked, onToggle, ariaLabel }: ToggleSwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      onClick={onToggle}
      className={`relative inline-flex h-8 w-14 sm:h-6 sm:w-11 shrink-0 items-center rounded-full border transition-colors cursor-pointer touch-manipulation ${
        checked ? "border-white bg-white" : "border-zinc-700 bg-zinc-800"
      }`}
    >
      <span
        className={`inline-block h-6 w-6 sm:h-4 sm:w-4 rounded-full transition-transform ${
          checked
            ? "translate-x-7 sm:translate-x-5.5 bg-zinc-950"
            : "translate-x-1 bg-zinc-400"
        }`}
      />
    </button>
  );
}

interface BinaryToggleProps {
  checked: boolean;
  offLabel: string;
  onLabel: string;
  onToggle: () => void;
  ariaLabel: string;
}

function BinaryToggle({
  checked,
  offLabel,
  onLabel,
  onToggle,
  ariaLabel,
}: BinaryToggleProps) {
  return (
    <div className="inline-flex items-center gap-3 min-h-[44px] sm:min-h-0">
      <button
        type="button"
        onClick={() => {
          if (checked) onToggle();
        }}
        className={`font-mono text-xs sm:text-[11px] py-1.5 px-1 transition-colors cursor-pointer touch-manipulation ${
          !checked ? "text-white font-semibold" : "text-zinc-500"
        }`}
      >
        {offLabel}
      </button>
      <ToggleSwitch checked={checked} onToggle={onToggle} ariaLabel={ariaLabel} />
      <button
        type="button"
        onClick={() => {
          if (!checked) onToggle();
        }}
        className={`font-mono text-xs sm:text-[11px] py-1.5 px-1 transition-colors cursor-pointer touch-manipulation ${
          checked ? "text-white font-semibold" : "text-zinc-500"
        }`}
      >
        {onLabel}
      </button>
    </div>
  );
}

interface SelectControlProps {
  id: string;
  ariaLabel: string;
  value: string;
  onChangeValue: (nextValue: string) => void;
  options: readonly { value: string; label: string }[];
}

function SelectControl({
  id,
  ariaLabel,
  value,
  onChangeValue,
  options,
}: SelectControlProps) {
  return (
    <div className="relative w-full sm:w-auto sm:min-w-[180px]">
      <select
        id={id}
        aria-label={ariaLabel}
        value={value}
        onChange={(e) => onChangeValue(e.target.value)}
        className="w-full min-h-[46px] sm:min-h-0 appearance-none rounded-lg border border-zinc-700 bg-zinc-900 pl-3.5 pr-10 py-2.5 sm:py-2 text-sm sm:text-xs font-medium text-white focus:outline-none focus:border-zinc-400 transition-colors cursor-pointer touch-manipulation"
      >
        {options.map((opt) => (
          <option key={opt.value} value={opt.value} className="bg-zinc-900 text-white">
            {opt.label}
          </option>
        ))}
      </select>
      <ChevronDown className="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 h-4 w-4 sm:h-3.5 sm:w-3.5 text-zinc-400" />
    </div>
  );
}

function hasTauriRuntime(): boolean {
  return (
    typeof window !== "undefined" &&
    ("__TAURI_INTERNALS__" in window || "__TAURI__" in window)
  );
}

function NvencControls({ settings, onChange }: { settings: NvencSettings; onChange: (settings: NvencSettings) => void }) {
  return (
    <details className="rounded-xl border border-zinc-800 p-3">
      <summary className="cursor-pointer text-sm font-semibold text-white">NVIDIA NVENC tuning</summary>
      <p className="my-2 text-xs text-zinc-400">Applies when the next hosted stream starts. Supported settings vary by GPU and driver.</p>
      <div className="space-y-2">
        <SettingRow title="NVENC preset" hint="P3 is the default. P4 increases quality and P2 lowers encoder load.">
          <SelectControl id="vmax-nvenc-preset" ariaLabel="NVENC preset" value={String(settings.preset)}
            options={[{ value: "3", label: "P3" }, { value: "4", label: "P4" }, { value: "2", label: "P2" }]}
            onChangeValue={(value) => onChange({ ...settings, preset: Number(value) })} />
        </SettingRow>
        <SettingRow title="Encoder rate control" hint="CBR without filler data."><span>CBR</span></SettingRow>
        <SettingRow title="Quarter-resolution multipass" hint="Run an additional low-resolution pass to improve bit allocation.">
          <ToggleSwitch ariaLabel="Quarter-resolution multipass" checked={settings.quarterPass}
            onToggle={() => onChange({ ...settings, quarterPass: !settings.quarterPass })} />
        </SettingRow>
        <SettingRow title="Intra refresh" hint="Refresh the picture gradually to help recover from packet loss.">
          <span className="text-xs text-zinc-400">On supported hardware</span>
        </SettingRow>
        {NVENC_NUMBER_OPTIONS.map(({ key, label, hint, min, max }) => {
          const upper = key === "refreshFrames" ? Math.min(max, settings.refreshPeriod - 1) : max;
          const value = Math.min(upper, settings[key]);
          return (
            <SettingRow key={key} title={label} hint={hint}>
              <label className="flex items-center gap-2 text-xs text-white">
                <input type="range" aria-label={label} min={min} max={upper} value={value} className="w-24 accent-white"
                  onChange={(event) => onChange({ ...settings, [key]: Number(event.target.value) })} />
                <span className="min-w-8 tabular-nums">{value}</span>
              </label>
            </SettingRow>
          );
        })}
      </div>
    </details>
  );
}

export function SettingsModal({
  initialTab,
  open,
  onClose,
  config,
  onUpdateVideo,
  onUpdateAudio,
  onUpdateNetwork,
  onUpdateCustomization,
  onReset,
  presets,
  activePresetId,
  onApplyPreset,
  onSavePreset,
  onDeletePreset,
  onAutomaticConfig,
  onLongDistanceConfig,
  buildVersion,
  savedConnections,
}: SettingsModalProps) {
  const desktop = !/Android|iPhone|iPad/i.test(navigator.userAgent);
  const [presetName, setPresetName] = useState("");
  const activePreset = presets.find((preset) => preset.id === activePresetId) ?? null;
  const [autostart, setAutostart] = useState(false);
  const [autostartError, setAutostartError] = useState("");
  useEffect(() => {
    if (open && desktop) void invoke<boolean>("autostart_enabled").then(setAutostart).catch(() => {});
  }, [open, desktop]);
  const [activeTab, setActiveTab] = useState<SettingsTab>("video");
  const [audioOutputs, setAudioOutputs] = useState<{ id: string; name: string }[]>([]);
  const [graphicsCaps, setGraphicsCaps] = useState<{
    d3d11: boolean;
    d3d12: boolean;
    vulkan: boolean;
    effective: string;
    note: string;
  } | null>(null);
  useEffect(() => {
    if (!open || !hasTauriRuntime()) return;
    void invoke<{ d3d11: boolean; d3d12: boolean; vulkan: boolean; effective: string; note: string }>(
      "graphics_backends",
    )
      .then(setGraphicsCaps)
      .catch(() => setGraphicsCaps(null));
  }, [open]);
  useEffect(() => {
    if (!open || activeTab !== "network_audio" || !/Windows/i.test(navigator.userAgent) || !hasTauriRuntime()) return;
    void invoke<{ id: string; name: string }[]>("list_host_audio_outputs").then(setAudioOutputs).catch(() => setAudioOutputs([]));
  }, [open, activeTab]);
  useEffect(() => { if (open) setActiveTab(initialTab); }, [open, initialTab]);
  const syncTimerRef = useRef<number | null>(null);
  const latestConfigRef = useRef<VmaXConfig>(config);
  latestConfigRef.current = config;
  const { video, audio, network, customization: { statsOverlay } } = config;

  // Sync runtime settings to Rust backend immediately on mount and with short debounce on rapid slider changes.
  useEffect(() => {
    if (!hasTauriRuntime()) {
      return;
    }
    if (syncTimerRef.current !== null) {
      window.clearTimeout(syncTimerRef.current);
    }
    syncTimerRef.current = window.setTimeout(() => {
      syncTimerRef.current = null;
      void invoke("update_settings", { settings: { video, audio, network, customization: { statsOverlay } } }).catch(() => {
        // Backend ignores transient startup/shutdown races cleanly.
      });
    }, 60);
    return () => {
      if (syncTimerRef.current !== null) {
        window.clearTimeout(syncTimerRef.current);
        syncTimerRef.current = null;
      }
    };
  }, [video, audio, network, statsOverlay]);

  const flushAndClose = () => {
    if (syncTimerRef.current !== null) {
      window.clearTimeout(syncTimerRef.current);
      syncTimerRef.current = null;
      if (hasTauriRuntime()) {
        void invoke("update_settings", { settings: latestConfigRef.current }).catch(
          () => {},
        );
      }
    }
    onClose();
  };

  useEffect(() => {
    if (!open) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        flushAndClose();
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [open, onClose]);

  if (!open) {
    return null;
  }

  return (
    <div
      className="vmax-settings-overlay fixed inset-0 z-50 flex items-center justify-center bg-zinc-950/85 backdrop-blur-sm p-2.5 sm:p-8"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) {
          flushAndClose();
        }
      }}
      role="dialog"
      aria-modal="true"
      aria-labelledby="vmax-settings-title"
    >
      <div className="vmax-settings-panel w-full max-w-4xl rounded-2xl border border-zinc-800 bg-zinc-900 shadow-2xl shadow-black/60 overflow-hidden flex flex-col max-h-[92dvh]">
        {/* Modal Header */}
        <div className="vmax-settings-header shrink-0 flex items-center justify-between border-b border-zinc-800 px-4 sm:px-6 py-4 sm:py-5">
          <div className="flex items-center gap-3 min-w-0">
            <div className="flex h-10 w-10 sm:h-9 sm:w-9 shrink-0 items-center justify-center rounded-xl border border-zinc-700 bg-zinc-800 text-white">
              <Settings className="h-4 w-4" />
            </div>
            <div className="min-w-0">
              <h2
                id="vmax-settings-title"
                className="text-sm font-semibold tracking-wide text-white"
              >
                Settings
              </h2>
              {buildVersion && (
                <p className="mt-0.5 text-[11px] leading-tight text-zinc-500">
                  VmaX {buildVersion}
                </p>
              )}
            </div>
          </div>

          <button
            type="button"
            onClick={flushAndClose}
            aria-label="Close settings"
            className="flex h-11 w-11 sm:h-8 sm:w-8 shrink-0 items-center justify-center rounded-xl sm:rounded-lg border border-zinc-800 bg-zinc-950 text-zinc-400 hover:border-zinc-700 hover:text-white active:bg-zinc-800 transition-colors cursor-pointer touch-manipulation"
          >
            <X className="h-5 w-5 sm:h-4 sm:w-4" />
          </button>
        </div>

        {/* Body: Sidebar + Active Category Panel */}
        <div className="vmax-settings-body flex flex-col md:flex-row flex-1 min-h-0">
          {/* Category Sidebar */}
          <aside className="vmax-settings-nav md:w-56 shrink-0 border-b md:border-b-0 md:border-r border-zinc-800 bg-zinc-950/40 p-3 sm:p-4">
            <nav
              className="grid grid-cols-2 sm:grid-cols-4 md:grid-cols-1 gap-1.5"
              aria-label="Settings categories"
            >
              {CATEGORIES.map((cat) => {
                const Icon = cat.icon;
                const active = activeTab === cat.id;
                return (
                  <button
                    key={cat.id}
                    type="button"
                    onClick={() => setActiveTab(cat.id)}
                    aria-pressed={active}
                    className={`flex min-w-0 items-center gap-2 rounded-xl min-h-[44px] px-2.5 py-2 text-left transition-colors cursor-pointer touch-manipulation ${
                      active
                        ? "vmax-active-tab"
                        : "text-zinc-200 border border-zinc-700 hover:bg-zinc-800"
                    }`}
                  >
                    <Icon
                      className={`h-4 w-4 shrink-0 ${active ? "text-white" : "text-zinc-500"}`}
                    />
                    <div className="min-w-0">
                      <p className="text-xs font-medium leading-tight">
                        {cat.label}
                      </p>
                    </div>
                  </button>
                );
              })}
            </nav>
          </aside>

          {/* Content Area */}
          <div className="vmax-settings-content flex-1 min-w-0 min-h-0 overflow-y-auto overscroll-contain p-4 sm:p-8 space-y-5">
            {activeTab === "general" && (
              <>
                <div className="flex items-center justify-between gap-2">
                  <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
                    <Settings className="h-3.5 w-3.5 text-zinc-400" />
                    <span>General</span>
                  </div>
                </div>

                <section className="vmax-preset-panel" aria-label="Settings presets">
                  <div className="flex flex-wrap items-start justify-between gap-3">
                    <div className="min-w-0">
                      <p className="text-sm font-semibold text-white">Presets</p>
                      <p className="text-xs text-zinc-500">
                        One click sets resolution, FPS, bitrate, pacing, FEC, packet window, audio and the overlay together.
                      </p>
                    </div>
                    <div className="flex flex-wrap items-center gap-2">
                      <input
                        aria-label="New preset name"
                        placeholder="Name this setup"
                        value={presetName}
                        maxLength={40}
                        onChange={(event) => setPresetName(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key !== "Enter") return;
                          event.preventDefault();
                          onSavePreset(presetName);
                          setPresetName("");
                        }}
                        className="min-w-0 rounded-lg border border-zinc-700 bg-zinc-950 px-2.5 py-2 text-sm text-white"
                      />
                      <button
                        type="button"
                        className="vmax-secondary"
                        onClick={() => {
                          onSavePreset(presetName);
                          setPresetName("");
                        }}
                      >
                        Save current
                      </button>
                    </div>
                  </div>
                  <div className="vmax-chip-row vmax-preset-row mt-3">
                    {presets.map((preset) => (
                      <span
                        key={preset.id}
                        className="vmax-preset-chip"
                      >
                        <button
                          type="button"
                          aria-pressed={preset.id === activePresetId}
                          title={preset.summary}
                          onClick={() => onApplyPreset(preset.id)}
                        >
                          {preset.name}
                        </button>
                        {!preset.builtin && (
                          <button
                            type="button"
                            className="vmax-preset-remove"
                            aria-label={`Delete preset ${preset.name}`}
                            title="Delete this preset"
                            onClick={() => onDeletePreset(preset.id)}
                          >
                            <X className="h-3.5 w-3.5" />
                          </button>
                        )}
                      </span>
                    ))}
                  </div>
                  {activePreset ? (
                    <p className="vmax-status mt-3" role="status">
                      {activePreset.summary}
                    </p>
                  ) : (
                    <p className="vmax-status mt-3" role="status">
                      Custom settings — save them as a preset to come back to them later.
                    </p>
                  )}
                </section>
                <div className="space-y-3.5">
                  {desktop ? (
                    <SettingRow title="Start minimized to tray on boot" hint="Keep VmaX available from the Windows or desktop tray.">
                      <input type="checkbox" aria-label="Start minimized to tray on boot" checked={autostart}
                        onChange={(event) => {
                          const enabled = event.target.checked;
                          setAutostartError("");
                          void invoke("set_autostart", { enabled }).then(() => setAutostart(enabled))
                            .catch((error) => setAutostartError(String(error)));
                        }} />
                      {autostartError && <span role="alert">{autostartError}</span>}
                    </SettingRow>
                  ) : (
                    <p>Autostart is available on desktop.</p>
                  )}
                </div>
              </>
            )}
            {activeTab === "video" && (
              <>
                <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
                  <Monitor className="h-3.5 w-3.5 text-zinc-400" />
                  <span>Video &amp; Display</span>
                </div>

                <div className="space-y-3.5">
                  <SettingRow
                    title="Resolution"
                    hint="Target capture and display canvas size"
                  >
                    <SelectControl
                      id="vmax-resolution"
                      ariaLabel="Stream resolution"
                      value={config.video.resolution}
                      options={RESOLUTION_OPTIONS}
                      onChangeValue={(val) => {
                        const match = RESOLUTION_OPTIONS.find((o) => o.value === val);
                        if (match) onUpdateVideo({ resolution: match.value });
                      }}
                    />
                  </SettingRow>

                  <SettingRow
                    title="Automatic configuration"
                    hint="Negotiate the codec, adapt bandwidth, and keep your selected FPS and resolution ceiling."
                  >
                    <div className="flex flex-wrap gap-2">
                      <button type="button" className="vmax-secondary" onClick={onAutomaticConfig}>Auto configure</button>
                      <button type="button" className="vmax-secondary" onClick={onLongDistanceConfig}>Unstable or long-distance link</button>
                    </div>
                  </SettingRow>

                  <SettingRow
                    title="Target Framerate"
                    hint="Maximum frames encoded per second"
                  >
                    <SelectControl
                      id="vmax-framerate"
                      ariaLabel="Target framerate"
                      value={String(config.video.framerate)}
                      options={STREAM_FRAMERATES.map((fps) => ({ value: String(fps), label: `${fps} FPS` }))}
                      onChangeValue={(val) => {
                        const fps = STREAM_FRAMERATES.find((candidate) => String(candidate) === val);
                        if (fps) onUpdateVideo({ framerate: fps });
                      }}
                    />
                  </SettingRow>

                  <SettingRow
                    title="Constant FPS"
                    hint={`Keep sending at the selected ${config.video.framerate} FPS, even when the desktop is static.`}
                  >
                    {config.network.protocol === "auto" ? (
                      <span className="text-sm text-zinc-400">On in Auto</span>
                    ) : (
                      <ToggleSwitch
                        checked={config.video.constantFps}
                        onToggle={() => onUpdateVideo({ constantFps: !config.video.constantFps })}
                        ariaLabel="Constant FPS"
                      />
                    )}
                  </SettingRow>

                  {/* Target Bitrate Slider + Synchronized Speedometer */}
                  <div className="rounded-xl border border-zinc-800 bg-zinc-950/80 p-5">
                    <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-6">
                      <div className="flex-1 space-y-4">
                        <div className="flex items-center justify-between gap-3">
                          <div>
                            <p className="text-sm font-medium text-white">{config.video.unlimitedBitrate ? "Fixed bitrate target" : "Adaptive bitrate ceiling"}</p>
                            <p className="text-xs text-zinc-500 mt-0.5">
                              Video bitrate before audio and packet recovery overhead
                            </p>
                          </div>
                          <span className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1 font-mono text-xs font-semibold text-white tabular-nums">
                            {config.video.bitrateMbps} Mbps
                          </span>
                        </div>

                        <div className="py-1">
                          <input
                            id="vmax-bitrate-slider"
                            type="range"
                            min={1}
                            max={MAX_STREAM_BITRATE_MBPS}
                            step={1}
                            value={config.video.bitrateMbps}
                            aria-label="Target bitrate in Mbps"
                            onChange={(e) =>
                              onUpdateVideo({ bitrateMbps: Number(e.target.value) })
                            }
                            className="w-full h-2.5 sm:h-1.5 rounded-lg bg-zinc-800 accent-white cursor-pointer touch-manipulation"
                          />
                          <div className="flex justify-between text-[10px] font-mono text-zinc-500 mt-2">
                            <span>1 Mbps</span>
                            <span>100 Mbps</span>
                            <span>{MAX_STREAM_BITRATE_MBPS} Mbps</span>
                          </div>
                        </div>
                      </div>

                      <div className="flex items-center justify-center rounded-xl border border-zinc-800 bg-zinc-900/70 px-4 py-2.5 shrink-0">
                        <SpeedometerSvg
                          bitrateMbps={config.video.bitrateMbps}
                          minMbps={1}
                          maxMbps={MAX_STREAM_BITRATE_MBPS}
                          decimals={0}
                          className="w-36 overflow-visible"
                        />
                      </div>
                    </div>
                  </div>

                  <SettingRow title="Chroma sampling" hint="4:4:4 preserves colored text and one-pixel edges. The next session uses 4:2:0 if either device lacks HEVC 4:4:4 support.">
                    <SelectControl id="vmax-chroma" ariaLabel="Chroma sampling" value={config.video.chroma}
                      options={[
                        { value: "420", label: "4:2:0 · wider compatibility" },
                        { value: "444", label: "4:4:4 · sharper colored text" },
                      ]}
                      onChangeValue={(value) => onUpdateVideo({ chroma: value === "444" ? "444" : "420" })} />
                  </SettingRow>

                  <SettingRow title="Preferred codec" hint="The next hosted stream tries this codec first when both devices support it.">
                    <SelectControl id="vmax-stream-codec" ariaLabel="Preferred codec" value={config.video.codec}
                      options={[
                        { value: "auto", label: "Auto" },
                        { value: "h264", label: "H.264" },
                        { value: "hevc", label: "H.265" },
                        { value: "hevc_main10", label: "H.265 Main 10" },
                        { value: "av1", label: "AV1 (hardware only)" },
                      ]}
                      onChangeValue={(value) => onUpdateVideo({ codec: value as StreamVideoSettings["codec"] })} />
                  </SettingRow>

                  {/* Hardware Encoder Toggle: NVENC / CPU Fallback */}
                  <SettingRow
                    title="Hardware Encoder"
                    hint="Zero-copy GPU pipeline vs software CPU fallback"
                    icon={<Cpu className="h-4 w-4" />}
                  >
                    <BinaryToggle
                      checked={config.video.encoder === "hardware"}
                      offLabel="CPU Fallback"
                      onLabel="NVENC"
                      ariaLabel="Toggle encoder between NVENC and CPU fallback"
                      onToggle={() =>
                        onUpdateVideo({
                          encoder:
                            config.video.encoder === "hardware"
                              ? "software"
                              : "hardware",
                        })
                      }
                    />
                  </SettingRow>
                </div>
              </>
            )}

            {activeTab === "network_audio" && (
              <>
                <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
                  <Network className="h-3.5 w-3.5 text-zinc-400" />
                  <span>Network &amp; Audio</span>
                </div>

                <div className="space-y-3.5">
                  <SettingRow
                    title="Protocol"
                    hint="Auto uses Raw UDP when a direct probe succeeds and WebRTC otherwise."
                  >
                    <SelectControl
                      id="vmax-transport"
                      ariaLabel="Transport protocol"
                      value={config.network.protocol}
                      options={[
                        { value: "auto", label: "Auto (Best Connection)" },
                        { value: "quic", label: "Legacy QUIC" },
                        { value: "raw_udp", label: "Legacy Raw UDP" },
                      ]}
                      onChangeValue={(protocol) => {
                        if (protocol === "auto" || protocol === "quic" || protocol === "raw_udp")
                          onUpdateNetwork({ protocol });
                      }}
                    />
                  </SettingRow>

                  <SettingRow
                    title="Audio Bitrate"
                    hint="Opus stereo frame bandwidth"
                    icon={<Volume2 className="h-4 w-4" />}
                  >
                    <SelectControl
                      id="vmax-audio-bitrate"
                      ariaLabel="Audio bitrate"
                      value={String(config.audio.bitrateKbps)}
                      options={AUDIO_BITRATE_OPTIONS.map((o) => ({
                        value: String(o.value),
                        label: o.label,
                      }))}
                      onChangeValue={(val) => {
                        const num = Number(val);
                        const match = AUDIO_BITRATE_OPTIONS.find((o) => o.value === num);
                        if (match) onUpdateAudio({ bitrateKbps: match.value });
                      }}
                    />
                  </SettingRow>

                  <div className="rounded-xl border border-zinc-800 bg-zinc-950/80 p-5 space-y-4">
                    <div className="flex items-center justify-between gap-3">
                      <div className="flex items-start gap-3">
                        <div className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-zinc-800 bg-zinc-900 text-zinc-400">
                          <ShieldCheck className="h-4 w-4" />
                        </div>
                        <div>
                          <p className="text-sm font-medium text-white">
                            Forward Error Correction (FEC)
                          </p>
                          <p className="text-xs text-zinc-500 mt-0.5">
                            Starting parity budget. Enabled QUIC sessions adapt it to packet loss.
                          </p>
                        </div>
                      </div>
                      <span className="rounded-lg border border-zinc-700 bg-zinc-900 px-3 py-1 font-mono text-xs font-semibold text-white tabular-nums">
                        {config.network.fecRedundancyPct}%
                      </span>
                    </div>

                    <div className="py-1">
                      <input
                        id="vmax-fec-slider"
                        type="range"
                        min={0}
                        max={20}
                        step={1}
                        value={config.network.fecRedundancyPct}
                        aria-label="Forward Error Correction redundancy percentage"
                        onChange={(e) =>
                          onUpdateNetwork({
                            fecRedundancyPct: Number(e.target.value),
                          })
                        }
                        className="w-full h-2.5 sm:h-1.5 rounded-lg bg-zinc-800 accent-white cursor-pointer touch-manipulation"
                      />
                      <div className="flex justify-between text-[10px] font-mono text-zinc-500 mt-2">
                        <span>0% · Zero Overhead</span>
                        <span>10%</span>
                        <span>20%</span>
                      </div>
                    </div>
                  </div>

                  <SettingRow title="Stream computer audio" hint="Capture playback in the next hosted session.">
                    <ToggleSwitch
                      checked={config.audio.streamComputerAudio}
                      onToggle={() => onUpdateAudio({ streamComputerAudio: !config.audio.streamComputerAudio })}
                      ariaLabel="Stream computer audio"
                    />
                  </SettingRow>
                  {/Windows/i.test(navigator.userAgent) && (
                    <SettingRow title="Exclude Discord audio" hint="Open Discord desktop before hosting. VmaX shares other apps across playback devices.">
                      <ToggleSwitch checked={config.audio.excludeDiscord} ariaLabel="Exclude Discord audio"
                        onToggle={() => onUpdateAudio({ excludeDiscord: !config.audio.excludeDiscord, gameOnly: false })} />
                    </SettingRow>
                  )}
                  {/Windows/i.test(navigator.userAgent) && !config.audio.excludeDiscord && (
                    <SettingRow title="Game audio only" hint="Share one Windows process and its child processes while keeping Discord private. Applies the next time you host.">
                      <ToggleSwitch checked={config.audio.gameOnly} ariaLabel="Game audio only"
                        onToggle={() => onUpdateAudio({ gameOnly: !config.audio.gameOnly })} />
                    </SettingRow>
                  )}
                  {/Windows/i.test(navigator.userAgent) && config.audio.gameOnly && !config.audio.excludeDiscord && (
                    <SettingRow title="Game process ID" hint="Open Task Manager → Details and enter the PID of the running game. Update it after restarting the game.">
                      <input aria-label="Game process ID" type="text" inputMode="numeric" maxLength={10}
                        className="rounded-lg border border-zinc-700 bg-zinc-950 p-2 text-sm text-white"
                        value={config.audio.gamePid} onChange={(event) => onUpdateAudio({ gamePid: event.target.value.replace(/\D/g, "") })} />
                    </SettingRow>
                  )}
                  {/Windows/i.test(navigator.userAgent) && !config.audio.gameOnly && !config.audio.excludeDiscord && (
                    <SettingRow title="Host audio output" hint="Shares everything played through this device. Applies the next time you host.">
                      <select aria-label="Host audio output" className="vmax-secondary max-w-full" value={config.audio.outputDeviceId}
                        onChange={(event) => onUpdateAudio({ outputDeviceId: event.target.value })}>
                        <option value="">Windows default playback</option>
                        {config.audio.outputDeviceId && !audioOutputs.some((device) => device.id === config.audio.outputDeviceId) && (
                          <option value={config.audio.outputDeviceId}>Saved device (unavailable)</option>
                        )}
                        {audioOutputs.map((device) => <option key={device.id} value={device.id}>{device.name}</option>)}
                      </select>
                    </SettingRow>
                  )}

                  <SettingRow
                    title="AI Noise Suppression"
                    hint="Real-time RNNoise voice isolation on capture"
                    icon={<Sparkles className="h-4 w-4" />}
                  >
                    <ToggleSwitch
                      checked={config.audio.aiNoiseSuppression}
                      onToggle={() =>
                        onUpdateAudio({
                          aiNoiseSuppression: !config.audio.aiNoiseSuppression,
                        })
                      }
                      ariaLabel="Toggle AI noise suppression"
                    />
                  </SettingRow>

                  <SettingRow
                    title="Hardware Cursor"
                    hint="Host draws the pointer into the stream. Off keeps this device's pointer (instant, but only follows local input)"
                    icon={<MousePointer className="h-4 w-4" />}
                  >
                    <ToggleSwitch
                      checked={config.network.hardwareCursor}
                      onToggle={() =>
                        onUpdateNetwork({
                          hardwareCursor: !config.network.hardwareCursor,
                        })
                      }
                      ariaLabel="Toggle hardware cursor"
                    />
                  </SettingRow>
                </div>
              </>
            )}

            {activeTab === "controller" && (
              <div className="space-y-3.5">
                <div className="space-y-2 rounded-xl border border-zinc-500/30 p-3">
                  <label className="flex items-center justify-between gap-3 text-sm font-semibold">
                    Controller haptic feedback
                    <input type="checkbox" role="switch" checked={config.customization.hapticsEnabled}
                      onChange={(event) => onUpdateCustomization({ hapticsEnabled: event.target.checked })} />
                  </label>
                  <label className="flex flex-wrap items-center justify-between gap-2 text-sm">
                    Vibration intensity
                    <input type="range" min={0} max={100} value={config.customization.hapticsIntensity}
                      disabled={!config.customization.hapticsEnabled}
                      onChange={(event) => onUpdateCustomization({ hapticsIntensity: Number(event.target.value) })} />
                    <span>{config.customization.hapticsIntensity}%</span>
                  </label>
                </div>
                <SettingRow title="Touch control opacity" hint="Applies to the on-screen gamepad.">
                  <input type="range" aria-label="Touch control opacity" min={35} max={100} value={config.customization.controllerOpacity} onChange={(event) => onUpdateCustomization({ controllerOpacity: Number(event.target.value) })} />
                </SettingRow>
                <SettingRow title="Touch control size" hint="Adjust the size for your grip.">
                  <input type="range" aria-label="Touch control size" min={75} max={115} value={config.customization.controllerSize} onChange={(event) => onUpdateCustomization({ controllerSize: Number(event.target.value) })} />
                </SettingRow>
              </div>
            )}
            {activeTab === "advanced" && (
              <div className="space-y-3.5">
                <SettingRow title="Frame pacing" hint="Choose what the next hosted stream prioritizes as bandwidth falls.">
                  <select aria-label="Frame pacing" value={config.video.framePacing}
                    className="rounded-lg border border-zinc-700 bg-zinc-950 p-2 text-sm text-white"
                    onChange={(event) => onUpdateVideo({ framePacing: event.target.value as FramePacing })}>
                    <option value="balanced">Balanced</option>
                    <option value="motion">Smoother motion</option>
                    <option value="detail">Sharper frames</option>
                  </select>
                </SettingRow>
                <SettingRow
                  title="Graphics backend"
                  hint={graphicsCaps?.note ?? "Detecting graphics backend…"}
                >
                  <span className="text-sm text-zinc-200">{graphicsCaps?.effective ?? "—"}</span>
                </SettingRow>
                <SettingRow
                  title="Sharpness"
                  hint="Hardware sharpening on the scaling pass. 0 is off; 40-60 restores detail lost to downscaling."
                >
                  <div className="flex items-center gap-2 text-xs text-white">
                    <input
                      type="range"
                      aria-label="Sharpness"
                      min={0}
                      max={100}
                      step={5}
                      value={config.video.sharpness}
                      className="w-24 accent-white"
                      onChange={(event) => onUpdateVideo({ sharpness: Number(event.target.value) })}
                    />
                    <span className="min-w-11 tabular-nums">{config.video.sharpness}%</span>
                  </div>
                </SettingRow>
                <SettingRow
                  title="Resolution and reconstruction"
                  hint="The host fits the capture to the selected resolution. The client scales and sharpens the picture for its display."
                >
                  <span className="font-mono text-xs text-zinc-500">Client-side</span>
                </SettingRow>
                <SettingRow
                  title="Lock resolution"
                  hint="Keep the selected frame size within the host display’s limits."
                >
                  <ToggleSwitch
                    checked={config.video.lockResolution}
                    onToggle={() =>
                      onUpdateVideo({ lockResolution: !config.video.lockResolution })
                    }
                    ariaLabel="Lock stream resolution"
                  />
                </SettingRow>
                <SettingRow title="Host upload speed" hint="Mbps available to the host. Video uses up to 80%.">
                  <input aria-label="Host upload speed in Mbps" type="number" min={1} max={1000} value={config.video.hostUploadMbps}
                    onChange={e => onUpdateVideo({ hostUploadMbps: Math.max(1, Math.min(1000, Math.round(Number(e.target.value)))) })} className="w-20 rounded bg-zinc-800 p-2" />
                </SettingRow>
                <SettingRow title="Enforce Minimum Quality (Limit Players)" hint="Reserve 10 Mbps for HEVC or 12 Mbps for H.264 per viewer.">
                  <ToggleSwitch ariaLabel="Enforce minimum quality" checked={config.video.strictQuality}
                    onToggle={() => onUpdateVideo({ strictQuality: !config.video.strictQuality })} />
                </SettingRow>
                <SettingRow
                  title="Bitrate mode"
                  hint="Fixed holds your requested target. Adaptive uses congestion feedback to stay within the link’s capacity."
                >
                  <SelectControl
                    id="vmax-bitrate-mode"
                    ariaLabel="Bitrate mode"
                    value={config.video.unlimitedBitrate ? "fixed" : "adaptive"}
                    options={[
                      { value: "fixed", label: "Fixed · prioritize quality" },
                      { value: "adaptive", label: "Adaptive · use available bandwidth" },
                    ]}
                    onChangeValue={(value) => onUpdateVideo({ unlimitedBitrate: value === "fixed" })}
                  />
                </SettingRow>
                <SettingRow title="Viewer sharpening" hint="Contrast-adaptive sharpening for the WebCodecs viewer. 0% disables the filter.">
                  <div className="flex items-center gap-2 text-xs text-white">
                    <input type="range" aria-label="Viewer sharpening" min={0} max={100} step={5}
                      value={config.video.viewerSharpness} className="w-24 accent-white"
                      onChange={(event) => onUpdateVideo({ viewerSharpness: Number(event.target.value) })} />
                    <span className="min-w-11 tabular-nums">{config.video.viewerSharpness}%</span>
                  </div>
                </SettingRow>
                <SettingRow title="Packet recovery window" hint="Allow extra time for late packets and short send bursts on the next connection. Complete frames play immediately.">
                  <div className="flex items-center gap-2 text-xs text-white">
                    <input type="range" aria-label="Packet recovery window" min={8} max={80} step={1}
                      value={config.network.packetWindowMs} className="w-24 accent-white"
                      onChange={(event) => onUpdateNetwork({ packetWindowMs: Number(event.target.value) })} />
                    <span className="min-w-11 tabular-nums">{config.network.packetWindowMs} ms</span>
                  </div>
                </SettingRow>
                {config.video.encoder === "hardware" && (
                  <NvencControls settings={config.video.nvenc} onChange={(nvenc) => onUpdateVideo({ nvenc })} />
                )}
              </div>
            )}
            {activeTab === "connections" && savedConnections}
            {activeTab === "customization" && (
              <>
                <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
                  <LayoutGrid className="h-3.5 w-3.5 text-zinc-400" />
                  <span>Customization</span>
                </div>

                <div className="space-y-3.5">
                  <fieldset className="rounded-xl border border-zinc-700 bg-zinc-950 p-4">
                    <legend className="px-2 text-sm font-semibold text-white">Theme</legend>
                    <div className="vmax-theme-picker">
                      {([["neon", "Neon"], ["blue", "Midnight Blue"], ["mono", "Graphite"]] as const).map(([value, label]) => (
                        <button key={value} type="button" data-theme-choice={value} className="vmax-secondary"
                          aria-pressed={config.customization.theme === value} onClick={() => onUpdateCustomization({ theme: value })}>
                          <span className="vmax-theme-swatch" aria-hidden="true" />{label}
                        </button>
                      ))}
                    </div>
                  </fieldset>
                  <SettingRow
                    title="Stats Overlay"
                    hint="Show live ping, bitrate, packet loss, and hardware telemetry during active streams"
                  >
                    <ToggleSwitch
                      checked={config.customization.statsOverlay}
                      onToggle={() =>
                        onUpdateCustomization({
                          statsOverlay: !config.customization.statsOverlay,
                        })
                      }
                      ariaLabel="Toggle stats overlay"
                    />
                  </SettingRow>
                </div>
              </>
            )}
          </div>
        </div>

        {/* Footer */}
        <div className="vmax-settings-footer shrink-0 flex items-center justify-between gap-3 border-t border-zinc-800 bg-zinc-950 px-4 sm:px-6 py-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
          <button
            type="button"
            onClick={onReset}
            className="inline-flex items-center justify-center gap-2 min-h-[46px] sm:min-h-0 rounded-xl sm:rounded-lg border border-zinc-800 bg-zinc-900 px-4 sm:px-3.5 py-2.5 sm:py-2 text-sm sm:text-xs font-medium text-zinc-300 hover:border-zinc-700 hover:text-white active:bg-zinc-800 transition-colors cursor-pointer touch-manipulation"
          >
            <RotateCcw className="h-4 w-4 sm:h-3.5 sm:w-3.5" />
            <span>Reset</span>
          </button>

          <button
            type="button"
            onClick={flushAndClose}
            className="vmax-primary"
          >
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
