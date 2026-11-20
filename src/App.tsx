import { CloudAccounts } from "./components/CloudAccounts";
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  ArrowRight,
  ChevronDown,
  Globe,
  House,
  Link2,
  Monitor,
  ScanLine,
  Radio,
  RefreshCw,
  SlidersHorizontal,
  Square,
  Wifi,
  Users,
  UserRound,
} from "lucide-react";
import "./App.css";
import { HostingDashboard, type GuestPermissions } from "./components/HostingDashboard";
import { HostSetup } from "./components/HostSetup";
import { SettingsModal } from "./components/SettingsModal";
import { SavedConnectionsPanel } from "./components/SavedConnectionsPanel";
import { QrScanner } from "./components/QrScanner";
import { StreamViewer } from "./components/StreamViewer";
import { TitleBar } from "./components/TitleBar";
import { VmaXLogo } from "./components/VmaXLogo";
import { MAX_STREAM_BITRATE_MBPS, STREAM_FRAMERATES, useVmaXConfig } from "./config/defaults";
import type {
  DiscoveredHost,
  HardwareTelemetry,
  LocalRelayStatus,
  NetworkTelemetry,
  PacketLogEntry,
  RelayRoom,
  SavedConnections,
  SavedConnectionUpdate,
  SavedDevice,
} from "./types";

const EMPTY_TELEMETRY: NetworkTelemetry = {
  role: null,
  phase: "idle",
  room_code: null,
  invite_uri: null,
  local_udp_addr: null,
  public_reflexive_addr: null,
  active_peer_addr: null,
  relayed: false,
  saved_session: false,
  peer_savable: false,
  cert_fingerprint: null,
  rtt_ms: 0,
  min_rtt_ms: 0,
  packet_loss_pct: 0,
  target_bitrate_bps: 0,
  congestion_state: "stable",
  video_frames_tx: 0,
  video_frames_rx: 0,
  video_frames_dropped: 0,
  fec_recovered_shards: 0,
  audio_packets_tx: 0,
  audio_packets_rx: 0,
  input_events_tx: 0,
  input_events_rx: 0,
  last_error: null,
};

const IDLE_HARDWARE: HardwareTelemetry = {
  nvencLoadPct: 0,
  vramUsedGb: 0,
  vramTotalGb: 0,
  cpuLoadPct: 0,
  encodeLatencyMs: 0,
  decodeLatencyMs: 0,
  fps: 0,
};

type AppView = "lobby" | "hosting" | "streaming";

const DEFAULT_SIGNALING_URL = "ws://127.0.0.1:8787";
const SIGNALING_STORAGE_KEY = "vmax.signaling-url";
const EMPTY_SAVED: SavedConnections = { networks: [], devices: [], autoHostEndpoint: null };

function savedSignalingUrl(): string {
  try {
    return localStorage.getItem(SIGNALING_STORAGE_KEY) || DEFAULT_SIGNALING_URL;
  } catch {
    return DEFAULT_SIGNALING_URL;
  }
}

function hasTauriRuntime(): boolean {
  return (
    typeof window !== "undefined" &&
    ("__TAURI_INTERNALS__" in window || "__TAURI__" in window)
  );
}

export function App() {
  const isAndroid = /Android/i.test(navigator.userAgent);
  const canHost = /Windows/i.test(navigator.userAgent);
  const defaultMode = canHost ? "host" : "join";
  const canChangeStreamWindow = !isAndroid && hasTauriRuntime();
  const {
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
  } = useVmaXConfig();

  const { hapticsEnabled, hapticsIntensity } = config.customization;
  useEffect(() => {
    if (hasTauriRuntime()) {
      void invoke("set_haptics_enabled", { enabled: hapticsEnabled, intensity: hapticsIntensity }).catch(console.error);
    }
  }, [hapticsEnabled, hapticsIntensity]);

  const [view, setView] = useState<AppView>("lobby");
  const [windowPreference, setWindowPreference] = useState({ fullscreen: false });
  const [streamFullscreen, setStreamFullscreen] = useState(false);
  const [windowModeError, setWindowModeError] = useState<string | null>(null);
  const windowModeRequest = useRef(Promise.resolve());
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [mode, setMode] = useState<"host" | "join">(defaultMode);
  const [accountPanel, setAccountPanel] = useState<HTMLDivElement | null>(null);
  const [page, setPage] = useState<"home" | "friends" | "sessions" | "computers" | "accounts">("home");
  const [settingsTab, setSettingsTab] = useState<"video" | "controller" | "connections">("video");
  const [favorites, setFavorites] = useState<string[]>(() => {
    try {
      const value: unknown = JSON.parse(localStorage.getItem("vmax.favorites") ?? "[]");
      return Array.isArray(value) ? value.filter((id): id is string => typeof id === "string") : [];
    } catch { return []; }
  });
  const [favoritesOnly, setFavoritesOnly] = useState(false);
  const theme = config.customization.theme;
  useEffect(() => { document.documentElement.dataset.theme = theme; }, [theme]);
  const [signalingUrl, setSignalingUrl] = useState(savedSignalingUrl);
  const [enableStun, setEnableStun] = useState(true);
  const [relayOnly, setRelayOnly] = useState(false);
  const [relayTesting, setRelayTesting] = useState(false);
  const [relayTestResult, setRelayTestResult] = useState<string | null>(null);
  const [buildVersion, setBuildVersion] = useState<string | null>(null);
  useEffect(() => {
    if (hasTauriRuntime()) void getVersion().then(setBuildVersion).catch(() => {});
  }, []);

  async function testRelay() {
    setRelayTesting(true);
    setRelayTestResult(null);
    try {
      const elapsed = await invoke<number>("test_media_relay", { endpoint: signalingUrl });
      setRelayTestResult(`Packets forwarded in both directions · ${elapsed} ms round trip from this device`);
    } catch (error) {
      setRelayTestResult(`Relay test failed. ${String(error)}`);
    } finally {
      setRelayTesting(false);
    }
  }
  const [displays, setDisplays] = useState<{ id: string; width: number; height: number; x: number; y: number }[]>([]);
  const [captureDisplay, setCaptureDisplay] = useState("");
  const [hostOnlyInput, setHostOnlyInput] = useState(false);
  const [guestControllerOnly, setGuestControllerOnly] = useState(false);
  const [guestPermissions, setGuestPermissions] = useState<GuestPermissions>({ keyboard: true, mouse: true, controller: true });
  const [displayError, setDisplayError] = useState("");

  async function refreshDisplays() {
    if (!hasTauriRuntime()) return;
    try {
      setDisplays(await invoke<typeof displays>("list_host_displays"));
      setDisplayError("");
    } catch (err) {
      setDisplayError(String(err));
    }
  }
  useEffect(() => {
    if (mode === "host" && view === "lobby") void refreshDisplays();
  }, [mode, view]);
  const [inviteInput, setInviteInput] = useState("");
  const [qrOpen, setQrOpen] = useState(false);
  const [telemetry, setTelemetry] = useState<NetworkTelemetry>(EMPTY_TELEMETRY);
  const telemetryRevision = useRef(0);
  const [hardware, setHardware] = useState<HardwareTelemetry>(IDLE_HARDWARE);
  const [relayStatus, setRelayStatus] = useState<LocalRelayStatus>({
    running: false,
    ws_url: null,
    lan_ws_url: null,
  });
  const [pingHistory, setPingHistory] = useState<number[]>([]);
  const [logs, setLogs] = useState<PacketLogEntry[]>([]);
  const [playerNotices, setPlayerNotices] = useState<{ id: number; text: string; expires: number }[]>([]);
  const [uiError, setUiError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState<SavedConnections>(EMPTY_SAVED);
  const [lastFriend, setLastFriend] = useState<string | null>(null);
  const [lastJoin, setLastJoin] = useState<{ fingerprint: string } | { code: string; endpoint: string } | null>(null);
  const pendingJoin = useRef<typeof lastJoin>(null);
  const [friendRequests, setFriendRequests] = useState<string[]>([]);
  const [friendInboxError, setFriendInboxError] = useState<string | null>(null);
  const [friendAccess, setFriendAccess] = useState<Record<string, boolean>>({});
  const [requestedFriends, setRequestedFriends] = useState<string[]>([]);
  const [friendAction, setFriendAction] = useState<string | null>(null);
  const savedHosts = saved.devices.filter((device) => device.role === "host");
  const visibleHosts = favoritesOnly ? savedHosts.filter((device) => favorites.includes(device.fingerprint)) : savedHosts;
  const [autoSuspended, setAutoSuspended] = useState(false);
  const [joinTab, setJoinTab] = useState<"invite" | "discover">("invite");
  const [discoveredHosts, setDiscoveredHosts] = useState<DiscoveredHost[]>([]);
  const [relayRooms, setRelayRooms] = useState<RelayRoom[]>([]);
  const [relayListNote, setRelayListNote] = useState<string | null>(null);
  const [scanError, setScanError] = useState<string | null>(null);
  const [scanning, setScanning] = useState(false);
  const pendingSavedJoin = useRef(false);
  const [automaticAttempts, setAutomaticAttempts] = useState(0);
  const stopping = useRef(false);

  const isConnected = telemetry.phase === "connected";
  const wantsFullscreen = canChangeStreamWindow && view === "streaming" && isConnected && windowPreference.fullscreen;

  useEffect(() => {
    if (isAndroid && hasTauriRuntime()) {
      void invoke("set_stream_immersive", { active: view === "streaming" && isConnected && !settingsOpen }).catch(() => {});
    }
  }, [isAndroid, view, isConnected, settingsOpen]);

  // During Android playback MediaCodec renders into a SurfaceView composited
  // below the WebView. The page must stay transparent or the opaque lobby /
  // streaming backgrounds would cover the native picture.
  useEffect(() => {
    if (!isAndroid || !hasTauriRuntime()) return;
    const root = document.documentElement;
    root.classList.toggle("vmax-native-video", view === "streaming");
    return () => root.classList.remove("vmax-native-video");
  }, [isAndroid, view]);

  useEffect(() => {
    if (!canChangeStreamWindow) return;
    let superseded = false;
    windowModeRequest.current = windowModeRequest.current.then(async () => {
      if (superseded) return;
      try {
        const appWindow = getCurrentWindow();
        if (await appWindow.isFullscreen() !== wantsFullscreen) {
          await appWindow.setFullscreen(wantsFullscreen);
        }
        if (!superseded) {
          setStreamFullscreen(wantsFullscreen);
          setWindowModeError(null);
        }
      } catch {
        if (!superseded) setWindowModeError("Could not change the window mode. Try again.");
      }
    });
    return () => { superseded = true; };
  }, [canChangeStreamWindow, wantsFullscreen, windowPreference]);

  useEffect(() => {
    if (hasTauriRuntime()) {
      void invoke("set_stream_input_paused", { paused: settingsOpen }).catch(() => {});
    }
  }, [settingsOpen]);

  const isSessionActive =
    telemetry.phase !== "idle" &&
    telemetry.phase !== "disconnected" &&
    telemetry.phase !== "failed";

  useEffect(() => {
    if (!canHost || !hasTauriRuntime() || !saved.devices.some((friend) => friend.role === "client")) return;
    let active = true;
    let pending = false;
    const refresh = async () => {
      if (pending) return;
      pending = true;
      try {
        const inbox = await invoke<{ requests: string[]; error: string | null }>("friend_inbox");
        if (active) {
          setFriendRequests(inbox.requests);
          setFriendInboxError(inbox.error);
        }
      } catch (error) {
        if (active) setFriendInboxError(String(error));
      } finally {
        pending = false;
      }
    };
    void refresh();
    const timer = window.setInterval(refresh, 10_000);
    return () => { active = false; window.clearInterval(timer); };
  }, [canHost, saved]);

  useEffect(() => {
    if (!hasTauriRuntime() || view !== "lobby"
      || (page !== "friends" && !(isAndroid && page === "home") && !settingsOpen && requestedFriends.length === 0)) return;
    let active = true;
    let pending = false;
    const refresh = async () => {
      if (pending || document.hidden) return;
      pending = true;
      const results = await Promise.all(saved.devices.filter((friend) => friend.role === "host").map(async (friend) => {
        try {
          return [friend.fingerprint, await invoke<boolean>("friend_access", { fingerprint: friend.fingerprint, request: false })] as const;
        } catch {
          return [friend.fingerprint, null] as const;
        }
      }));
      if (active) {
        const next = Object.fromEntries(results.filter((entry) => entry[1] !== null)) as Record<string, boolean>;
        setFriendAccess((previous) => Object.keys(previous).length === Object.keys(next).length
          && Object.entries(next).every(([id, access]) => previous[id] === access) ? previous : next);
      }
      pending = false;
    };
    void refresh();
    const timer = window.setInterval(refresh, 10_000);
    return () => { active = false; window.clearInterval(timer); };
  }, [saved, page, view, isAndroid, settingsOpen, requestedFriends.length]);

  useEffect(() => {
    if (hasTauriRuntime()) {
      void invoke<SavedConnections>("get_saved_connections").then(setSaved).catch((error) => setUiError(String(error)));
    }
  }, []);

  useEffect(() => {
    if (!hasTauriRuntime()) {
      return;
    }

    let nextNotice = 0;
    const unlistenPresence = listen<{ slot: number; joined: boolean }>("vmax://player-presence", ({ payload }) => {
      const notice = {
        id: nextNotice++,
        text: `Player ${payload.slot + 1} ${payload.joined ? "joined" : "left"}`,
        expires: Date.now() + 4_000,
      };
      setPlayerNotices((previous) => [...previous.slice(-7), notice]);
    });

    const unlistenTel = listen<NetworkTelemetry>("vmax://telemetry", (event) => {
      if (stopping.current) return;
      telemetryRevision.current += 1;
      setTelemetry(event.payload);
      if (event.payload.phase === "connected") {
        if (event.payload.role === "client" && pendingJoin.current) setLastJoin(pendingJoin.current);
        setAutomaticAttempts(0);
        if (pendingSavedJoin.current && event.payload.role === "client" && event.payload.saved_session) {
          pendingSavedJoin.current = false;
          setRequestedFriends([]);
          setView("streaming");
        }
        setPingHistory((prev) => [...prev.slice(-23), event.payload.rtt_ms]);
      } else if (
        event.payload.phase === "idle" ||
        event.payload.phase === "disconnected"
      ) {
        setPingHistory([]);
      }
      if (event.payload.last_error && event.payload.phase === "failed") {
        setUiError(event.payload.last_error);
      }
      if (
        event.payload.role === "client" &&
        (event.payload.phase === "disconnected" || event.payload.phase === "failed")
      ) {
        pendingSavedJoin.current = false;
        setHardware(IDLE_HARDWARE);
        setView("lobby");
        void invoke("stop_session").catch(() => {});
      }
    });
    const revision = telemetryRevision.current;
    void unlistenTel.then(() => invoke<NetworkTelemetry>("get_network_snapshot"))
      .then((snapshot) => {
        if (revision === telemetryRevision.current) setTelemetry(snapshot);
      })
      .catch(() => {});

    const unlistenHw = listen<HardwareTelemetry>(
      "vmax://hardware-telemetry",
      (event) => {
        setHardware(event.payload);
      },
    );

    const unlistenLog = listen<PacketLogEntry>("vmax://packet-log", (event) => {
      setLogs((prev) => [event.payload, ...prev].slice(0, 40));
    });

    const unlistenRelay = listen<LocalRelayStatus>("vmax://relay-status", (event) => {
      setRelayStatus(event.payload);
      if (event.payload.running && event.payload.ws_url) {
        setSignalingUrl((cur) =>
          cur.includes("127.0.0.1") || cur.includes("localhost")
            ? (event.payload.ws_url ?? cur)
            : cur,
        );
      }
    });

    return () => {
      unlistenPresence.then((f) => f()).catch(() => {});
      unlistenTel.then((f) => f()).catch(() => {});
      unlistenHw.then((f) => f()).catch(() => {});
      unlistenLog.then((f) => f()).catch(() => {});
      unlistenRelay.then((f) => f()).catch(() => {});
    };
  }, []);

  const nextNoticeExpiry = playerNotices[0]?.expires;
  useEffect(() => {
    if (nextNoticeExpiry === undefined) return;
    const timer = window.setTimeout(() => {
      const now = Date.now();
      setPlayerNotices((previous) => previous.filter(({ expires }) => expires > now));
    }, Math.max(0, nextNoticeExpiry - Date.now()));
    return () => window.clearTimeout(timer);
  }, [nextNoticeExpiry]);

  const livePingMs = isConnected ? telemetry.rtt_ms : 0;

  const sparklineSamples =
    pingHistory.length >= 2
      ? pingHistory
      : pingHistory.length === 1
        ? [pingHistory[0], pingHistory[0]]
        : [0, 0];

  const useRelayOnly = mode === "host" && relayOnly;

  async function handleToggleRelay() {
    setUiError(null);
    if (!hasTauriRuntime()) {
      setUiError("Local signaling relay requires the Tauri desktop runtime.");
      return;
    }
    try {
      const next = await invoke<LocalRelayStatus>("toggle_local_signaling", {
        enable: !relayStatus.running,
      });
      setRelayStatus(next);
      if (next.running && next.ws_url) {
        setSignalingUrl(next.ws_url);
      }
    } catch (err) {
      setUiError(String(err));
    }
  }

  function buildStartSessionParams(endpoint = signalingUrl) {
    return {
      signaling_url: endpoint.trim() || DEFAULT_SIGNALING_URL,
      enable_stun: enableStun,
      max_bitrate_mbps: config.video.bitrateMbps,
      protocol: config.network.protocol,
      resolution: config.video.resolution,
      framerate: config.video.framerate,
      frame_pacing: config.video.framePacing,
      sharpness: config.video.sharpness,
      upscaling: config.video.upscaling,
      lock_resolution: config.video.lockResolution,
      constant_fps: config.video.constantFps,
      unlimited_bitrate: config.video.unlimitedBitrate,
      nvenc: config.video.nvenc,
      jitter_window_ms: config.network.packetWindowMs,
      encoder: config.video.encoder,
      codec: config.video.codec,
      chroma: config.video.chroma,
      ai_noise_suppression: config.audio.aiNoiseSuppression,
      stream_computer_audio: config.audio.streamComputerAudio,
      audio_output_id: config.audio.outputDeviceId || null,
      capture_display: captureDisplay || null,
      host_only_input: hostOnlyInput,
      guest_controller_only: guestControllerOnly,
      audio_process_id: config.audio.gameOnly ? Number(config.audio.gamePid) : null,
      exclude_discord_audio: config.audio.excludeDiscord,
      audio_bitrate_kbps: config.audio.bitrateKbps,
      fec_redundancy_pct: config.network.fecRedundancyPct,
      hardware_cursor: config.network.hardwareCursor,
    };
  }


  async function handleStartHost(savedEndpoint?: string, automatic = false) {
    setUiError(null);
    if (config.audio.streamComputerAudio && config.audio.gameOnly && !config.audio.excludeDiscord
      && (!/^[1-9]\d{0,9}$/.test(config.audio.gamePid) || Number(config.audio.gamePid) > 0xffffffff)) {
      setUiError("Enter your game's process ID in Settings → Network & Audio, or turn off computer audio.");
      return;
    }
    if (!hasTauriRuntime()) {
      setUiError("Hosting requires the Tauri desktop runtime.");
      return;
    }

    if (!automatic) setAutoSuspended(true);
    setBusy(true);
    if (savedEndpoint) setView("hosting");
    const revision = telemetryRevision.current;
    try {
      let endpoint = savedEndpoint;
      if (!endpoint) {
        endpoint = await invoke<string>("prepare_internet", {
          preferredRelay: /^wss:\/\//i.test(signalingUrl.trim()) ? signalingUrl.trim() : null,
          additionalRelays: saved.networks.map((network) => network.endpoint),
        }).catch(() => DEFAULT_SIGNALING_URL);
      }
      const params = buildStartSessionParams(endpoint);
      setGuestPermissions({ keyboard: !guestControllerOnly, mouse: !guestControllerOnly, controller: true });
      const snap = await invoke<NetworkTelemetry>("start_host_session", {
        params: {
          ...params,
          protocol: savedEndpoint ? params.protocol : "quic",
          relay_only: relayOnly && /^wss:\/\//i.test(params.signaling_url),
        },
        savedOnly: !!savedEndpoint,
      });
      if (revision === telemetryRevision.current) {
        setTelemetry(snap);
        if (savedEndpoint && (snap.phase === "failed" || snap.phase === "disconnected")) {
          setView("lobby");
          void invoke("stop_session").catch(() => {});
        }
      }
      if (!savedEndpoint) setView("hosting");
    } catch (err) {
      setUiError(String(err));
      setView("lobby");
    } finally {
      setBusy(false);
    }
  }

  async function handleJoin(device?: SavedDevice, automatic = false, override?: { code?: string; endpoint?: string }) {
    setUiError(null);
    const trimmed = (override?.code ?? inviteInput).trim();
    if (!device && !trimmed) {
      setUiError("Paste a VmaX invite URI or room code to join.");
      return;
    }
    if (!hasTauriRuntime()) {
      setUiError("Joining a stream requires the Tauri desktop runtime.");
      return;
    }

    if (!automatic) {
      setAutoSuspended(!device?.automatic);
      setAutomaticAttempts(0);
    }
    setBusy(true);
    pendingSavedJoin.current = !!device;
    if (device) {
      setLastFriend(device.fingerprint);
      setPage("friends");
      setMode("join");
    } else {
      setView("streaming");
    }
    const revision = telemetryRevision.current;
    try {
      const paramsEndpoint = override?.endpoint ?? device?.endpoint ?? signalingUrl;
      const route = trimmed.startsWith("vmax://join/cloud/")
        ? await invoke<string>("cloud_connect", { target: "", invite: trimmed })
        : trimmed;
      pendingJoin.current = device ? { fingerprint: device.fingerprint } : { code: trimmed, endpoint: paramsEndpoint };
      const snap = await invoke<NetworkTelemetry>("join_session", {
        inviteInput: device ? "" : route,
        savedHost: device?.fingerprint,
        params: buildStartSessionParams(paramsEndpoint),
      });
      if (revision === telemetryRevision.current) {
        setTelemetry(snap);
        if (snap.phase === "connected" && pendingJoin.current) setLastJoin(pendingJoin.current);
        if (device && snap.phase === "connected") {
          pendingSavedJoin.current = false;
          setRequestedFriends([]);
          setView("streaming");
        }
        if (snap.phase === "failed" || snap.phase === "disconnected") {
          pendingSavedJoin.current = false;
          setView("lobby");
          void invoke("stop_session").catch(() => {});
        }
      }
    } catch (err) {
      pendingSavedJoin.current = false;
      setUiError(String(err));
      setView("lobby");
    } finally {
      setBusy(false);
    }
  }

  function rejoinLastStream() {
    if (!lastJoin || busy || isSessionActive) return;
    if ("fingerprint" in lastJoin) {
      const host = saved.devices.find((device) => device.role === "host" && device.fingerprint === lastJoin.fingerprint);
      if (host) void handleJoin(host);
      return;
    }
    setPage("home");
    setMode("join");
    setJoinTab("invite");
    setInviteInput(lastJoin.code);
    void handleJoin(undefined, false, lastJoin);
  }

  async function requestFriendAccess(friend: SavedDevice) {
    setFriendAction(friend.fingerprint);
    setUiError(null);
    try {
      const approved = await invoke<boolean>("friend_access", { fingerprint: friend.fingerprint, request: true });
      setFriendAccess((previous) => ({ ...previous, [friend.fingerprint]: approved }));
      if (approved) await handleJoin(friend);
      else setRequestedFriends((previous) => [...new Set([...previous, friend.fingerprint])]);
    } catch (error) {
      setUiError(String(error));
    } finally {
      setFriendAction(null);
    }
  }

  async function resolveFriendRequest(fingerprint: string, accept: boolean) {
    setFriendAction(fingerprint);
    setFriendInboxError(null);
    try {
      if (accept) {
        await updateSavedConnections({ kind: "accept_friend_request", fingerprint });
        setSettingsOpen(false);
      } else {
        await invoke("dismiss_friend_request", { fingerprint });
      }
      setFriendRequests((previous) => previous.filter((request) => request !== fingerprint));
    } catch (error) {
      setFriendInboxError(String(error));
    } finally {
      setFriendAction(null);
    }
  }

  async function handleStop(keepApprovedHosting = false) {
    if (stopping.current) return;
    stopping.current = true;
    setRequestedFriends([]);
    setAutoSuspended(!keepApprovedHosting);
    pendingSavedJoin.current = false;
    pendingJoin.current = null;
    telemetryRevision.current += 1;
    setBusy(true);
    setUiError(null);
    setView("lobby");
    try {
      if (hasTauriRuntime()) {
        await invoke("stop_session").catch(() => {});
      }
      setTelemetry(EMPTY_TELEMETRY);
      setHardware(IDLE_HARDWARE);
      setPingHistory([]);
    } finally {
      stopping.current = false;
      setBusy(false);
    }
  }

  function chooseSignalingUrl(endpoint: string) {
    setSignalingUrl(endpoint);
    try {
      localStorage.setItem(SIGNALING_STORAGE_KEY, endpoint.trim());
    } catch {
      // This session can still use the selected endpoint.
    }
  }

  /** Scans the local network for hosting peers and lists rooms on the selected relay. */
  async function runDiscoveryScan() {
    if (!hasTauriRuntime() || scanning || isSessionActive || busy) return;
    setScanning(true);
    setScanError(null);
    try {
      const endpoint = signalingUrl.trim() || DEFAULT_SIGNALING_URL;
      const [lan, relay] = await Promise.allSettled([
        invoke<DiscoveredHost[]>("scan_lan_hosts"),
        invoke<RelayRoom[]>("list_relay_rooms", { endpoint }),
      ]);
      if (lan.status === "fulfilled") {
        setDiscoveredHosts(lan.value);
      } else {
        setScanError(String(lan.reason));
      }
      if (relay.status === "fulfilled") {
        setRelayRooms(relay.value);
        setRelayListNote(null);
      } else {
        setRelayRooms([]);
        setRelayListNote(`${endpoint} is not answering room listings.`);
      }
    } finally {
      setScanning(false);
    }
  }

  async function updateSavedConnections(update: SavedConnectionUpdate) {
    const next = await invoke<SavedConnections>("update_saved_connections", { update });
    setSaved(next);
    if (update.kind === "remember_peer" && telemetry.role === "client") {
      setPage("friends");
      setMode("join");
    }
    if (update.kind === "accept_friend_request" || (update.kind === "set_auto_host" && update.endpoint)
      || (update.kind === "set_device_automatic" && update.role === "host" && update.enabled)) {
      setAutomaticAttempts(0);
      setAutoSuspended(false);
    }
  }

  function openSettings(tab: typeof settingsTab = "video") {
    setSettingsTab(tab);
    setSettingsOpen(true);
  }

  function openInvite() {
    setPage("home");
    setMode("join");
    setJoinTab("invite");
    requestAnimationFrame(() => document.getElementById("vmax-invite")?.focus());
  }

  function openSessions() {
    setPage("sessions");
    setMode(defaultMode);
    setJoinTab("invite");
  }

  useEffect(() => {
    if (!hasTauriRuntime() || settingsOpen || qrOpen || busy || !!friendAction || isSessionActive || view !== "lobby") return;
    const approvedRequest = saved.devices.find((device) => device.role === "host"
      && requestedFriends.includes(device.fingerprint) && friendAccess[device.fingerprint]);
    if (autoSuspended && !approvedRequest) return;
    const host = approvedRequest ?? saved.devices.find((device) => device.role === "host" && device.automatic);
    const endpoint = canHost && !approvedRequest ? saved.autoHostEndpoint : null;
    if (!host && !endpoint) return;
    let started = false;
    const connect = () => {
      if (started || document.hidden || stopping.current) return;
      started = true;
      setAutomaticAttempts((attempts) => attempts + 1);
      if (endpoint) void handleStartHost(endpoint, true);
      else if (host) void handleJoin(host, true);
    };
    const delay = Math.min(30_000, 1_000 * 2 ** Math.min(automaticAttempts, 5));
    const timer = window.setTimeout(connect, delay);
    document.addEventListener("visibilitychange", connect);
    window.addEventListener("online", connect);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", connect);
      window.removeEventListener("online", connect);
    };
  }, [saved, autoSuspended, automaticAttempts, settingsOpen, qrOpen, busy, friendAction, friendAccess, requestedFriends, isSessionActive, view, config, enableStun, relayOnly, canHost]);

  // Scan automatically the first time the Discover tab is opened, and refresh
  // whenever the session returns to idle so the list reflects live hosts.
  useEffect(() => {
    if (joinTab === "discover" && mode === "join" && !isSessionActive && !busy) {
      void runDiscoveryScan();
    }
    // Intentionally narrow: only re-scan when the tab opens or the session resets.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [joinTab, mode, isSessionActive]);

  async function pasteInvite() {
    try {
      const text = await navigator.clipboard.readText();
      if (text.trim()) {
        setInviteInput(text.trim());
        setUiError(null);
      }
    } catch {
      document.getElementById("vmax-invite")?.focus();
      setUiError(isAndroid ? "Touch and hold the invite field to paste." : "Use Ctrl+V in the invite field.");
    }
  }

  return (
    <>
      <div role="log" aria-label="Player activity" className="pointer-events-none fixed inset-x-4 top-14 z-[80] flex flex-col items-end gap-2">
        {playerNotices.map(({ id, text }) => (
          <div key={id} className="rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100 shadow-lg">
            {text}
          </div>
        ))}
      </div>
      <CloudAccounts section={view !== "streaming" && (page === "friends" || page === "computers" || page === "accounts") ? page : null}
        container={accountPanel} canHost={canHost} active={busy || isSessionActive}
        hostParams={() => buildStartSessionParams(signalingUrl)}
        join={(code) => handleJoin(undefined, false, { code })}
        onAccountNeeded={() => { setPage("accounts"); setView("lobby"); }}
        onOpenComputers={() => setPage("computers")} onOpenFriends={() => setPage("friends")} />
      {canHost && friendRequests.length > 0 && (
        <aside aria-label="Friend requests" className="fixed z-[70] bottom-4 right-4 max-w-[calc(100vw-2rem)] space-y-2 rounded-xl border border-zinc-700 bg-zinc-950 p-4 text-sm text-zinc-100 shadow-xl">
          <h2 className="font-semibold">Join requests</h2>
          {friendRequests.map((fingerprint) => {
            const friend = saved.devices.find((device) => device.role === "client" && device.fingerprint === fingerprint);
            return friend && <div className="vmax-chip-row" key={fingerprint}>
              <span>{friend.name} wants to join</span>
              <button type="button" className="vmax-primary" disabled={!!friendAction || busy || view === "streaming"} onClick={() => void resolveFriendRequest(fingerprint, true)}>Accept</button>
              <button type="button" className="vmax-secondary" disabled={!!friendAction} onClick={() => void resolveFriendRequest(fingerprint, false)}>Decline</button>
            </div>;
          })}
          {friendInboxError && <p role="alert">{friendInboxError}</p>}
        </aside>
      )}
      {view === "streaming" ? (
        <div className="vmax-stream-page flex h-[100dvh] min-h-0 flex-col overflow-hidden bg-black">
          {canChangeStreamWindow && !streamFullscreen && (
            <div data-stream-controls>
              <TitleBar settingsOpen={settingsOpen} onOpenSettings={() => openSettings()} />
            </div>
          )}
          <StreamViewer
            connected={isConnected}
            relayed={telemetry.relayed}
            connectionPath={telemetry.connection_path}
            resolution={config.video.resolution}
            pingMs={livePingMs}
            packetLossPct={telemetry.packet_loss_pct}
            hardwareCursor={config.network.hardwareCursor}
            nativeFps={hardware.fps}
            viewerSharpness={config.video.viewerSharpness}
            showTelemetryPill={config.customization.statsOverlay}
            controllerOpacity={config.customization.controllerOpacity}
            controllerSize={config.customization.controllerSize}
            settingsOpen={settingsOpen}
            fullscreen={canChangeStreamWindow ? streamFullscreen : undefined}
            onToggleFullscreen={() => setWindowPreference({ fullscreen: !streamFullscreen })}
            windowModeError={windowModeError}
            onOpenSettings={() => openSettings()}
            onOpenFriends={isConnected ? () => openSettings("connections") : undefined}
            onDisconnect={() => void handleStop()}
          />
        </div>
      ) : view === "hosting" ? (
        <div className="vmax-shell">
          <TitleBar
            settingsOpen={settingsOpen}
            onOpenSettings={() => openSettings()}
          />
          <HostingDashboard
            telemetry={telemetry}
            hardware={hardware}
            pingSamples={sparklineSamples}
            config={config}
            logs={logs}
            busy={busy}
            hostOnlyInput={hostOnlyInput}
            guestPermissions={guestPermissions}
            onGuestPermissions={async (permissions) => {
              await invoke("set_guest_input_permissions", { ...permissions });
              setGuestPermissions(permissions);
            }}
            onToggleHostOnly={async () => {
              await invoke("set_host_only_input", { enabled: !hostOnlyInput });
              setHostOnlyInput(!hostOnlyInput);
            }}
            switchingToApprovedHosting={!!saved.autoHostEndpoint && !telemetry.saved_session && !autoSuspended}
            onOpenFriends={isConnected && telemetry.peer_savable ? () => openSettings("connections") : undefined}
            onStopHosting={() => void handleStop(!!saved.autoHostEndpoint && !telemetry.saved_session && !autoSuspended)}
          />
        </div>
      ) : (
        <div className="vmax-shell">
          <TitleBar settingsOpen={settingsOpen} onOpenSettings={() => openSettings()} />
          <div className="vmax-workspace">
          <nav className="vmax-nav" aria-label="Main navigation">
            <button type="button" aria-pressed={page === "home"} onClick={() => { setPage("home"); setMode(defaultMode); }}><House size={20} />Home</button>
            <button type="button" aria-pressed={page === "computers"} onClick={() => setPage("computers")}><Monitor size={20} />Computers</button>
            <button type="button" aria-pressed={page === "friends"} onClick={() => { setPage("friends"); setMode("join"); }}><Users size={20} />Friends</button>
            <button type="button" aria-pressed={page === "accounts"} onClick={() => setPage("accounts")}><UserRound size={20} />Accounts</button>
            <button type="button" aria-pressed={page === "sessions"} onClick={openSessions}><Radio size={20} />Sessions</button>
          </nav>
          <main className="vmax-lobby vmax-workspace-content">
            <section className="vmax-panel">
              {page === "home" && (
                <header className="vmax-home">
                  <div className="vmax-brand">
                    <VmaXLogo size={72} className="vmax-brand-logo" />
                    <div><h1>Quick Connect</h1><p>Join a computer with an invite or find one nearby.</p></div>
                  </div>
                  {canHost && (
                    <button type="button" className="vmax-secondary" onClick={openSessions}><Radio size={16} /> Host a stream</button>
                  )}
                </header>
              )}
              <div ref={setAccountPanel} />
              {page === "friends" && savedHosts.length > 0 && (
                <section className="vmax-form" aria-label="Friends">
                  <div className="vmax-session-head">
                    <h2>Saved connections</h2>
                    <div className="vmax-chip-row">
                      <button type="button" className="vmax-secondary" onClick={openInvite}>Join with invite</button>
                      <button type="button" className="vmax-secondary" onClick={() => openSettings("connections")}>Manage friends</button>
                    </div>
                  </div>
                  {savedHosts.length > 0 && <div className="vmax-switch" aria-label="Filter friends">
                    <button type="button" aria-pressed={!favoritesOnly} onClick={() => setFavoritesOnly(false)}>All friends</button>
                    <button type="button" aria-pressed={favoritesOnly} onClick={() => setFavoritesOnly(true)}>Favorites</button>
                  </div>}
                  {visibleHosts.length === 0 && <p className="vmax-status">No favorites yet.</p>}
                  {visibleHosts.map((device) => (
                    <div className="vmax-computer" key={device.fingerprint}>
                      <Monitor size={25} /><div><strong>{device.name}</strong><small>{device.endpoint.startsWith("wss:") ? "Internet" : "Local network"}</small></div>
                      <button type="button" className="vmax-secondary" aria-label={favorites.includes(device.fingerprint) ? "Remove favorite" : "Add favorite"} aria-pressed={favorites.includes(device.fingerprint)} onClick={() => {
                        const next = favorites.includes(device.fingerprint) ? favorites.filter((id) => id !== device.fingerprint) : [...favorites, device.fingerprint];
                        setFavorites(next);
                        try { localStorage.setItem("vmax.favorites", JSON.stringify(next)); } catch { /* Keep the current selection. */ }
                      }}>{favorites.includes(device.fingerprint) ? "★" : "☆"}</button>
                      <div className="vmax-chip-row">
                        {requestedFriends.includes(device.fingerprint) && (
                          <button type="button" className="vmax-secondary" onClick={() => setRequestedFriends((previous) => previous.filter((id) => id !== device.fingerprint))}>Stop waiting</button>
                        )}
                        {friendAccess[device.fingerprint] !== false && (
                          <button type="button" className="vmax-primary" disabled={busy || isSessionActive || !!friendAction} onClick={() => void handleJoin(device)}>{lastFriend === device.fingerprint ? "Reconnect" : "Connect"}</button>
                        )}
                        {friendAccess[device.fingerprint] !== true && (
                          <button type="button" className="vmax-secondary" disabled={busy || isSessionActive || !!friendAction} onClick={() => void requestFriendAccess(device)}>
                            {requestedFriends.includes(device.fingerprint) ? "Request sent · Resend" : "Request to join"}
                          </button>
                        )}
                      </div>
                    </div>
                  ))}
                  {canHost && saved.autoHostEndpoint && (
                    <button type="button" className="vmax-secondary" disabled={busy || isSessionActive}
                      onClick={() => {
                        setAutoSuspended(false);
                        setAutomaticAttempts(0);
                        if (saved.autoHostEndpoint) void handleStartHost(saved.autoHostEndpoint, true);
                      }}><Radio size={16} /> Host for friends</button>
                  )}
                  {friendInboxError && !friendRequests.length && <p className="vmax-status">{friendInboxError}</p>}
                </section>
              )}
              {(page === "home" || page === "sessions") && (page === "sessions" && mode === "host" ? (
                <>
                  <div className="vmax-intro">
                    <div><h1>Hosting</h1><p>{config.video.resolution} · {config.video.framerate} FPS</p></div>
                  </div>
                  <div className="vmax-form">
                    <label htmlFor="vmax-capture-display">Monitor to share</label>
                    <div className="vmax-field-row">
                      <select id="vmax-capture-display" value={captureDisplay} disabled={busy || isSessionActive}
                        onChange={(event) => setCaptureDisplay(event.target.value)}>
                        <option value="">Automatic</option>
                        {captureDisplay && !displays.some((display) => display.id === captureDisplay) &&
                          <option value={captureDisplay}>Selected monitor unavailable</option>}
                        {displays.map((display, index) => <option key={display.id} value={display.id}>
                          {display.id.match(/\d+$/)?.[0] ? `Display ${display.id.match(/\d+$/)?.[0]}` : `Monitor ${index + 1}`} · {display.width} × {display.height} ({display.x}, {display.y})
                        </option>)}
                      </select>
                      <button type="button" className="vmax-secondary" aria-label="Refresh monitors" disabled={busy || isSessionActive}
                        onClick={() => void refreshDisplays()}><RefreshCw size={16} /></button>
                    </div>
                    {displayError && <p className="vmax-error" role="alert">{displayError}</p>}
                    <label className="flex items-center gap-2">
                      <input type="checkbox" checked={hostOnlyInput} disabled={busy || isSessionActive}
                        onChange={(event) => setHostOnlyInput(event.target.checked)} /> Host-only input
                    </label>
                  </div>
                  <div>
                    <button type="button" aria-label="Start Hosting" className="vmax-tile vmax-tile-primary w-full" disabled={busy || isSessionActive} onClick={() => void handleStartHost()}>
                      <Globe size={20} /><span>Start Hosting<small>Local and internet</small></span><ArrowRight size={16} />
                    </button>
                  </div>
                </>
              ) : (
                <>
                  <div className="vmax-switch" aria-label="Join method">
                    <button type="button" aria-pressed={joinTab === "invite"} onClick={() => setJoinTab("invite")}><Link2 size={16} /> Invite</button>
                    <button type="button" aria-pressed={joinTab === "discover"} onClick={() => setJoinTab("discover")}><Wifi size={16} /> Nearby</button>
                  </div>

                  {joinTab === "invite" ? (
                    <form className="vmax-form" onSubmit={(event) => { event.preventDefault(); if (!busy && !isSessionActive) void handleJoin(); }}>
                      <label htmlFor="vmax-invite">Invite link or local code</label>
                      <div className="vmax-field-row vmax-invite-row">
                        <input id="vmax-invite" value={inviteInput} onChange={(event) => setInviteInput(event.target.value)} placeholder="XXXX-XXXX or vmax://join/…" inputMode="url" enterKeyHint="go" autoCapitalize="none" autoCorrect="off" spellCheck={false} disabled={busy || isSessionActive} />
                        <button type="button" className="vmax-secondary" disabled={busy || isSessionActive} onClick={pasteInvite}>Paste</button>
                        <button type="button" className="vmax-secondary" aria-label="Scan QR" title="Scan QR" disabled={busy || isSessionActive} onClick={() => setQrOpen(true)}><ScanLine size={17} /></button>
                      </div>
                      {canChangeStreamWindow && (
                        <label className="vmax-inline">Window
                          <select aria-label="Stream window" value={windowPreference.fullscreen ? "fullscreen" : "windowed"} onChange={(event) => setWindowPreference({ fullscreen: event.target.value === "fullscreen" })}>
                            <option value="windowed">Windowed</option>
                            <option value="fullscreen">Fullscreen</option>
                          </select>
                        </label>
                      )}
                      <button type="submit" className="vmax-primary" disabled={busy || isSessionActive || !inviteInput.trim()}><ArrowRight size={17} /> Join Stream</button>
                    </form>
                  ) : (
                    <div className="vmax-form">
                      <div className="vmax-inline"><span>{scanning ? "Looking for hosts…" : discoveredHosts.length + relayRooms.length ? "Available hosts" : "No hosts found"}</span>
                        <button type="button" className="vmax-secondary" onClick={() => void runDiscoveryScan()} disabled={busy || scanning || isSessionActive}><RefreshCw size={15} /> Refresh</button>
                      </div>
                      {scanError && <p className="vmax-error" role="alert">{scanError}</p>}
                      {discoveredHosts.map((host) => (
                        <button key={host.endpoint + host.code} type="button" className="vmax-device" disabled={busy || isSessionActive} onClick={() => void handleJoin(undefined, false, { code: host.code, endpoint: host.endpoint })}>
                          <Wifi size={17} /><span>{host.name ?? "VmaX host"}<small>{host.code}</small></span><ArrowRight size={15} />
                        </button>
                      ))}
                      {relayRooms.map((room) => (
                        <button key={room.code} type="button" className="vmax-device" disabled={busy || isSessionActive} onClick={() => void handleJoin(undefined, false, { code: room.code })}>
                          <Globe size={17} /><span>{room.name ?? "VmaX host"}<small>{room.code}</small></span><ArrowRight size={15} />
                        </button>
                      ))}
                    </div>
                  )}
                </>
              ))}

              {busy && <p role="status" className="vmax-status">Connecting…</p>}
              {(uiError || telemetry.last_error) && <p role="alert" className="vmax-error">{uiError ?? telemetry.last_error}</p>}
              {isSessionActive && <button type="button" className="vmax-secondary" disabled={busy} onClick={() => void handleStop()}><Square size={14} /> Disconnect</button>}
              {lastJoin && !isSessionActive && (!('fingerprint' in lastJoin) || savedHosts.some((host) => host.fingerprint === lastJoin.fingerprint)) && (
                <button type="button" className="vmax-secondary" disabled={busy} onClick={rejoinLastStream}>Rejoin last stream</button>
              )}

              {(page === "home" || page === "sessions") && <details className="vmax-disclosure">
                <summary><SlidersHorizontal size={16} /> {page === "sessions" && mode === "host" ? "Host options" : "Network options"}<ChevronDown size={15} /></summary>
                <div className="vmax-form">
                  <label htmlFor="vmax-endpoint">Relay address</label>
                  <input id="vmax-endpoint" value={signalingUrl} onChange={(event) => { chooseSignalingUrl(event.target.value); setRelayTestResult(null); }} disabled={busy || isSessionActive || relayTesting} placeholder="ws://127.0.0.1:8787" />
                  <button type="button" className="vmax-secondary" disabled={busy || isSessionActive || relayTesting || !signalingUrl.trim()}
                    onClick={() => void testRelay()}>{relayTesting ? "Testing relay…" : "Test media relay"}</button>
                  {relayTestResult && <p className="vmax-status" role="status">{relayTestResult}</p>}
                  <button type="button" className="vmax-secondary" disabled={busy || isSessionActive} onClick={() => { chooseSignalingUrl(DEFAULT_SIGNALING_URL); setRelayOnly(false); }}>Use local Wi-Fi or hotspot</button>
                  {page === "sessions" && mode === "host" && (
                    <>
                      <button type="button" className="vmax-secondary" onClick={() => openSettings("connections")}>Manage relay addresses</button>
                      <label className="flex items-center gap-2">
                        <input type="checkbox" checked={guestControllerOnly} disabled={busy || isSessionActive}
                          onChange={(event) => setGuestControllerOnly(event.target.checked)} /> Guest controller only
                      </label>
                      <div className="vmax-inline">
                        <label><input type="checkbox" checked={enableStun} disabled={isSessionActive || useRelayOnly} onChange={(event) => setEnableStun(event.target.checked)} /> Try direct internet</label>
                        <label><input type="checkbox" checked={relayOnly} disabled={busy || isSessionActive} onChange={(event) => setRelayOnly(event.target.checked)} /> Always use the relay (internet)</label>
                      </div>
                      <fieldset disabled={busy || isSessionActive} className="vmax-form">
                        <legend>Quality</legend>
                        <div className="vmax-chip-row">
                          <button type="button" className="vmax-secondary" onClick={applyAutomaticConfig}>Auto configure</button>
                          {presets.map((preset) => (
                            <button type="button" className="vmax-secondary" key={preset.id}
                              aria-pressed={preset.id === activePresetId}
                              title={preset.summary}
                              onClick={() => applyPreset(preset.id)}>{preset.name}</button>
                          ))}
                        </div>
                        <div className="vmax-chip-row">
                          <label htmlFor="vmax-dashboard-framerate">Target FPS</label>
                          <select id="vmax-dashboard-framerate" value={config.video.framerate} onChange={(event) => {
                            const fps = STREAM_FRAMERATES.find((candidate) => String(candidate) === event.target.value);
                            if (fps) updateVideo({ framerate: fps });
                          }}>
                            {STREAM_FRAMERATES.map((fps) => <option key={fps} value={fps}>{fps} FPS</option>)}
                          </select>
                          <button type="button" className="vmax-secondary" aria-pressed={config.video.constantFps}
                            onClick={() => updateVideo({ constantFps: !config.video.constantFps })}>
                            Constant FPS {config.video.constantFps ? "on" : "off"}
                          </button>
                        </div>
                        <label htmlFor="vmax-dashboard-bitrate" className="vmax-inline">Bitrate <span>{config.video.bitrateMbps} Mbps</span></label>
                        <input id="vmax-dashboard-bitrate" type="range" min={1} max={MAX_STREAM_BITRATE_MBPS} value={config.video.bitrateMbps} onChange={(event) => updateVideo({ bitrateMbps: Number(event.target.value) })} />
                      </fieldset>
                      <HostSetup disabled={isSessionActive || busy} />
                      <button type="button" className="vmax-secondary" disabled={busy || isSessionActive} onClick={async () => {
                        setUiError(null);
                        setBusy(true);
                        try { await invoke("restart_as_administrator"); }
                        catch (error) { setUiError(String(error)); }
                        finally { setBusy(false); }
                      }}>Restart VmaX as administrator</button>
                      <button type="button" className="vmax-secondary" disabled={busy || isSessionActive} onClick={() => void handleToggleRelay()}>{relayStatus.running ? "Stop local relay" : "Start local relay"}</button>
                    </>
                  )}
                  {relayListNote && joinTab === "discover" && <p className="vmax-status">{relayListNote}</p>}
                </div>
              </details>}
            </section>
          </main>
          </div>
        </div>
      )}
      {qrOpen && <QrScanner onClose={() => setQrOpen(false)} onInvite={(invite) => {
        setQrOpen(false);
        setInviteInput(invite);
        if (!busy && !isSessionActive) void handleJoin(undefined, false, { code: invite });
      }} />}
      <SettingsModal
        open={settingsOpen}
        initialTab={settingsTab}
        onClose={() => setSettingsOpen(false)}
        config={config}
        onUpdateVideo={updateVideo}
        onUpdateAudio={updateAudio}
        onUpdateNetwork={updateNetwork}
        onUpdateCustomization={updateCustomization}
        onReset={resetConfig}
        presets={presets}
        activePresetId={activePresetId}
        onApplyPreset={applyPreset}
        onSavePreset={savePreset}
        onDeletePreset={deletePreset}
        onAutomaticConfig={applyAutomaticConfig}
        onLongDistanceConfig={applyLongDistanceConfig}
        buildVersion={buildVersion}
        savedConnections={
          <SavedConnectionsPanel
            saved={saved}
            endpoint={signalingUrl.trim() || DEFAULT_SIGNALING_URL}
            peerRole={telemetry.peer_savable && isConnected
              ? telemetry.role === "host" ? "client" : "host"
              : null}
            canHost={canHost}
            canConnect={!busy && !isSessionActive && view === "lobby"}
            paused={autoSuspended}
            friendAccess={friendAccess}
            onUpdate={updateSavedConnections}
            onSelect={(endpoint) => {
              chooseSignalingUrl(endpoint);
              setSettingsOpen(false);
            }}
            onConnect={(host) => {
              setSettingsOpen(false);
              void handleJoin(host);
            }}
            onRequest={(host) => {
              setPage("friends");
              setMode("join");
              setSettingsOpen(false);
              void requestFriendAccess(host);
            }}
            onResume={() => {
              setAutomaticAttempts(0);
              setAutoSuspended(false);
              setSettingsOpen(false);
            }}
          />
        }
      />
    </>
  );
}

export default App;

