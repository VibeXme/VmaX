import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type WheelEvent as ReactWheelEvent,
} from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import {
  Activity,
  ChevronDown,
  ClipboardCopy,
  Crosshair,
  Gamepad2,
  Gauge,
  Keyboard,
  LogOut,
  Maximize,
  Minimize,
  MousePointer2,
  Settings,
  SlidersHorizontal,
  Users,
  X,
} from "lucide-react";
import {
  extractModifierMask,
  keyboardCodeToAtScancode,
} from "../config/scancodes";
import {
  connectionPathLabel,
  type ConnectionPath,
  type ParsecTelemetryPayload,
  type RemoteInputPayload,
  type ResolutionOption,
} from "../types";
import { FramePresenter } from "../lib/framePresenter";
import { DecoderHealth } from "../lib/decodeHealth";
import { buildDiagnosticsReport, copyDiagnostics } from "../lib/diagnostics";
import { measureRefreshHz } from "../lib/displayRefresh";
import { MAX_STREAM_BITRATE_MBPS } from "../config/defaults";
import { TelemetryBar } from "./TelemetryBar";
import { TouchGamepad } from "./TouchGamepad";
import { ControllerEditor } from "./ControllerEditor";
import {
  CONTROLLER_LAYOUT_KEY,
  loadControllerLayout,
  type ControllerLayout,
} from "../config/touchController";
import { hevcSequence, type VideoSize } from "../config/hevc";
import { useDesktopTouch, type StreamPoint } from "./useDesktopTouch";

interface StreamViewerProps {
  connected: boolean;
  relayed: boolean;
  connectionPath?: ConnectionPath | null;
  resolution: ResolutionOption;
  pingMs: number;
  packetLossPct?: number;
  hardwareCursor?: boolean;
  nativeFps: number;
  viewerSharpness: number;
  showTelemetryPill: boolean;
  controllerOpacity: number;
  controllerSize: number;
  settingsOpen: boolean;
  fullscreen?: boolean;
  onToggleFullscreen?: () => void;
  windowModeError?: string | null;
  onOpenSettings: () => void;
  onOpenFriends?: () => void;
  onDisconnect: () => void;
}

const RESOLUTION_DIMENSIONS: Record<
  ResolutionOption,
  { width: number; height: number }
> = {
  // Only a pre-decode fallback: once the first frame arrives the real size comes
  // from the decoder, so "native" needs no host query here.
  native: { width: 1920, height: 1080 },
  "720p": { width: 1280, height: 720 },
  "1080p": { width: 1920, height: 1080 },
  "1440p": { width: 2560, height: 1440 },
  "4K": { width: 3840, height: 2160 },
};

const KEYFRAME_REQUEST_COOLDOWN_MS = 300;
/**
 * `latencyMode` is part of WebCodecs, but not every bundled TypeScript DOM
 * library knows it yet. Widening the config type locally keeps the hint while
 * the runtime ignores it on decoders that do not support it.
 */
type DecoderConfigWithLatency = VideoDecoderConfig & {
  latencyMode?: "quality" | "realtime";
};
/**
 * Consecutive lost frames tolerated before the decoder is reset to wait for a
 * keyframe. Four frames is ~67 ms at 60 FPS and ~33 ms at 120 FPS. Isolated
 * loss still gets cheap intra-refresh recovery, but a genuinely broken
 * reference chain can no longer stay visible for ~200 ms.
 */
const MAX_TOLERATED_GAPS = 4;
const HEADER_SIZE_BYTES = 16;
const CODEC_HEVC = 1;
const CODEC_H264 = 2;
const CODEC_HEVC_MAIN10 = 3;
const CODEC_HEVC_444 = 4;
const CODEC_AV1 = 5;
const isHevc = (codec: number) => codec === CODEC_HEVC || codec === CODEC_HEVC_MAIN10 || codec === CODEC_HEVC_444;
// Ordered by preference; older phones and GPUs only answer to a subset of
// these profile/level strings, so the probe walks the whole list.
const HEVC_444_CANDIDATES = ["hev1.4.10.L123.9E.08", "hev1.4.10.L153.9E.08"];
const HEVC_MAIN10_CANDIDATE_STRINGS = [
  "hev1.2.4.L120.B0",
  "hvc1.2.4.L120.B0",
  "hev1.2.4.L123.B0",
];

const HEVC_CANDIDATE_STRINGS = [
  "hev1.1.6.L120.90",
  "hvc1.1.6.L120.90",
  "hvc1.1.6.L93.B0",
  "hev1.1.6.L93.B0",
];
const H264_CANDIDATE_STRINGS = [
  "avc1.64002A",
  "avc1.4D401F",
  "avc1.42E01E",
  "avc1.42001E",
];
// AV1 Main profile. Level 4.0 covers 1080p60 and 5.0 the 4K tier; the probe
// walks the list because decoders advertise only the levels they can hold.
const AV1_CANDIDATE_STRINGS = ["av01.0.08M.08", "av01.0.12M.08", "av01.0.04M.08"];
const KEYBOARD_SENTINEL = "\u200b";
let remoteTapId = 0;
let nextKeyboardCaptureId = 0;

function hasTauriRuntime(): boolean {
  return (
    typeof window !== "undefined" &&
    ("__TAURI_INTERNALS__" in window || "__TAURI__" in window)
  );
}

/**
 * AV1 is only negotiated when a *hardware* decoder can hold it: software AV1
 * at 1080p60 costs far more CPU than the HEVC hardware path it would replace,
 * which is the opposite of the point. `powerEfficient` is the platform's own
 * "this runs on a dedicated decode block" answer.
 */
async function av1HardwareDecodeAvailable(): Promise<boolean> {
  const capabilities = navigator.mediaCapabilities;
  if (!capabilities || typeof capabilities.decodingInfo !== "function") return false;
  try {
    const info = await capabilities.decodingInfo({
      type: "file",
      video: {
        contentType: 'video/mp4; codecs="av01.0.08M.08"',
        width: 1920,
        height: 1080,
        bitrate: 20_000_000,
        framerate: 60,
      },
    });
    return info.supported && info.powerEfficient;
  } catch {
    return false;
  }
}

async function probeDecoder(candidates: string[]): Promise<VideoDecoderConfig | null> {
  if (typeof VideoDecoder === "undefined") {
    return null;
  }
  // Some WebViews expose VideoDecoder without isConfigSupported. Assume the
  // best candidate works and let configure() or the error callback decide.
  if (typeof VideoDecoder.isConfigSupported !== "function") {
    return candidates.length > 0
      ? ({ codec: candidates[0], optimizeForLatency: true, latencyMode: "realtime" } as DecoderConfigWithLatency)
      : null;
  }
  // Hardware first, then default preference, then the bare config: older
  // WebKit builds reject config members they do not know instead of ignoring
  // them, which made the whole probe fail on those devices.
  //
  // `latencyMode: "realtime"` tells the decoder not to build a reorder buffer.
  // It is the documented hint for interactive streaming and the single most
  // effective decoder-side setting for glass-to-glass latency; decoders that
  // do not know it are covered by the bare variant below.
  const variants: Array<Partial<DecoderConfigWithLatency>> = [
    { hardwareAcceleration: "prefer-hardware", optimizeForLatency: true, latencyMode: "realtime" },
    { hardwareAcceleration: "no-preference", optimizeForLatency: true, latencyMode: "realtime" },
    { hardwareAcceleration: "prefer-hardware", optimizeForLatency: true },
    {},
  ];
  for (const variant of variants) {
    for (const codec of candidates) {
      try {
        const config = { codec, ...variant };
        if ((await VideoDecoder.isConfigSupported(config)).supported) return config;
      } catch {
        // Try next codec profile candidate.
      }
    }
  }
  return null;
}

function normalizeBinaryFrame(payload: unknown): Uint8Array | null {
  if (payload instanceof Uint8Array) {
    return payload;
  }
  if (payload instanceof ArrayBuffer) {
    return new Uint8Array(payload);
  }
  if (Array.isArray(payload)) {
    return Uint8Array.from(payload);
  }
  return null;
}

function dispatchRemoteInput(input: RemoteInputPayload) {
  if (!hasTauriRuntime()) {
    return Promise.resolve();
  }
  return invoke<void>("send_input_event", { input }).catch(() => {
    // Session might be disconnecting; ignore transient IPC rejection.
  });
}

function isLocalSystemKey(event: KeyboardEvent) {
  return (
    event.metaKey ||
    event.key === "Meta" ||
    event.code === "MetaLeft" ||
    event.code === "MetaRight" ||
    (event.code === "Tab" && event.altKey)
  );
}

const RELEASE_KEYS = { KeyQ: "Q", KeyX: "X", F10: "F10", F11: "F11", F12: "F12" };
type ReleaseKey = keyof typeof RELEASE_KEYS;

function savedReleaseKey(): ReleaseKey {
  const key = localStorage.getItem("vmax.keyboard-release-key");
  return key && Object.prototype.hasOwnProperty.call(RELEASE_KEYS, key)
    ? (key as ReleaseKey)
    : "F12";
}

function getVideoBounds(canvas: HTMLCanvasElement) {
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0 || canvas.width <= 0 || canvas.height <= 0) {
    return null;
  }

  const containerAspect = rect.width / rect.height;
  const videoAspect = canvas.width / canvas.height;

  let drawWidth = rect.width;
  let drawHeight = rect.height;
  let offsetX = 0;
  let offsetY = 0;

  if (containerAspect > videoAspect) {
    drawWidth = rect.height * videoAspect;
    offsetX = (rect.width - drawWidth) / 2;
  } else {
    drawHeight = rect.width / videoAspect;
    offsetY = (rect.height - drawHeight) / 2;
  }

  return { x: offsetX, y: offsetY, width: drawWidth, height: drawHeight, rect };
}

function mapCanvasPointerNormalized(
  canvas: HTMLCanvasElement,
  clientX: number,
  clientY: number,
  clamp = false,
): StreamPoint | null {
  const bounds = getVideoBounds(canvas);
  if (!bounds) return null;
  const xNorm = (clientX - bounds.rect.left - bounds.x) / bounds.width;
  const yNorm = (clientY - bounds.rect.top - bounds.y) / bounds.height;
  if (!clamp && (xNorm < 0 || xNorm > 1 || yNorm < 0 || yNorm > 1)) return null;
  return { xNorm: Math.min(1, Math.max(0, xNorm)), yNorm: Math.min(1, Math.max(0, yNorm)) };
}

export function StreamViewer({
  connected,
  relayed,
  connectionPath,
  resolution,
  pingMs,
  packetLossPct,
  hardwareCursor = true,
  nativeFps,
  viewerSharpness,
  showTelemetryPill,
  controllerOpacity,
  controllerSize,
  settingsOpen,
  fullscreen,
  onToggleFullscreen,
  windowModeError,
  onOpenSettings,
  onOpenFriends,
  onDisconnect,
}: StreamViewerProps) {
  const sharpnessRef = useRef(viewerSharpness / 100);
  sharpnessRef.current = viewerSharpness / 100;
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const fallbackCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const touchCursorRef = useRef<HTMLDivElement>(null);
  const lastTouchPoint = useRef<StreamPoint | null>(null);
  const keyboardRef = useRef<HTMLTextAreaElement>(null);
  const touchAvailable = navigator.maxTouchPoints > 0;
  const [overlayVisible, setOverlayVisible] = useState(false);
  const [hostEncodeMs, setHostEncodeMs] = useState<number | null>(null);
  const [parsecTelemetry, setParsecTelemetry] = useState<ParsecTelemetryPayload | null>(null);
  const [recoverStalls, setRecoverStalls] = useState(false);
  const recoverStallsRef = useRef(recoverStalls);
  recoverStallsRef.current = recoverStalls;
  const [activeCodecLabel, setActiveCodecLabel] = useState<string>("H.265");
  const [gamepadPort, setGamepadPort] = useState<number | null>(null);
  const [gamepadError, setGamepadError] = useState<string | null>(null);
  const [bitrateLimit, setBitrateLimit] = useState(MAX_STREAM_BITRATE_MBPS);
  const [bitrateFeedback, setBitrateFeedback] = useState<string | null>(null);
  const [diagnosticsFeedback, setDiagnosticsFeedback] = useState<string | null>(null);

  useEffect(() => {
    if (!hasTauriRuntime()) return;
    const sub = listen<number>("vmax://host-encode-ms", ({ payload }) => setHostEncodeMs(payload));
    const pad = listen<number | null>("vmax://gamepad-port", ({ payload }) => setGamepadPort(payload));
    const parsec = listen<ParsecTelemetryPayload>("vmax://parsec-telemetry", ({ payload }) => {
      setParsecTelemetry(payload);
      if (payload.encodeMs > 0) {
        setHostEncodeMs(payload.encodeMs);
      }
    });
    return () => {
      void sub.then((release) => release()).catch(() => {});
      void pad.then((release) => release()).catch(() => {});
      void parsec.then((release) => release()).catch(() => {});
    };
  }, []);

  const [connectionOpen, setConnectionOpen] = useState(false);
  const [gaming, setGaming] = useState(false);
  const [editingController, setEditingController] = useState(false);
  const [controllerLayout, setControllerLayout] = useState(loadControllerLayout);
  const touchControllerAvailable = touchAvailable && hasTauriRuntime();
  const [mouseMode, setMouseMode] = useState<"trackpad" | "direct">("trackpad");
  const [keyboardOpen, setKeyboardOpen] = useState(false);
  const [pointerLocked, setPointerLocked] = useState(false);
  const [pointerLockError, setPointerLockError] = useState("");
  const windowsClient = /Windows/.test(navigator.userAgent) && hasTauriRuntime();
  const [releaseKey, setReleaseKey] = useState(savedReleaseKey);
  const [keyboardCapture, setKeyboardCapture] = useState(false);
  const [captureError, setCaptureError] = useState("");
  const keyboardCaptured = useRef(false);
  const captureRequest = useRef(0);
  const pressedMouse = useRef(new Set<number>());
  const releaseMouse = useCallback(() => {
    for (const button of pressedMouse.current) {
      void dispatchRemoteInput({ kind: "mouse_button", button, pressed: false });
    }
    pressedMouse.current.clear();
  }, []);

  const releaseKeyboardCapture = useCallback(() => {
    const captureId = captureRequest.current;
    captureRequest.current = 0;
    if (!keyboardCaptured.current) return;
    keyboardCaptured.current = false;
    setKeyboardCapture(false);
    void invoke("set_fullscreen_keyboard_capture", {
      enabled: false,
      shortcutScan: 0,
      shortcutModifiers: 0,
      captureId,
    }).catch(() => {});
  }, []);

  const [hasFirstFrame, setHasFirstFrame] = useState(false);
  const [nativeVideo, setNativeVideo] = useState(false);
  const nativeVideoRef = useRef(false);
  const connectedRef = useRef(connected);
  connectedRef.current = connected;
  const nativeFrameSeen = useRef(false);

  useEffect(() => {
    if (!nativeVideo || !connected) nativeFrameSeen.current = false;
    else if (nativeFps > 0) nativeFrameSeen.current = true;
  }, [connected, nativeFps, nativeVideo]);
  const webviewDecoderReset = useRef<(() => void) | null>(null);
  const [streamNotice, setStreamNotice] = useState<string>("Awaiting keyframe");
  const [streamSize, setStreamSize] = useState<{ w: number; h: number } | null>(null);
  const [playback, setPlayback] = useState<{
    fps: number;
    decodeMs: number | null;
    videoMbps: number;
    intervalMs: number;
    jitterMs: number;
    superseded: number;
  } | null>(null);

  const inputBlocked = settingsOpen || editingController;
  const settingsOpenRef = useRef(inputBlocked);
  settingsOpenRef.current = inputBlocked;
  const desktopTouchEnabled =
    connected && hasFirstFrame && touchAvailable && !inputBlocked && !gaming;
  const desktopTouchRef = useRef(desktopTouchEnabled);
  desktopTouchRef.current = desktopTouchEnabled;
  const pendingTap = useRef<number | null>(null);
  const composing = useRef(false);
  const textQueue = useRef(Promise.resolve());
  const touchQueue = useRef(Promise.resolve());
  const pendingMotion = useRef<{ input: RemoteInputPayload } | null>(null);

  const dims = RESOLUTION_DIMENSIONS[resolution];

  useEffect(() => {
    if (!windowsClient) return;
    const subscription = listen<number>("vmax://keyboard-capture-released", ({ payload }) => {
      if (payload !== captureRequest.current) return;
      captureRequest.current = 0;
      keyboardCaptured.current = false;
      setKeyboardCapture(false);
      if (document.pointerLockElement === canvasRef.current) document.exitPointerLock?.();
    });
    return () => {
      releaseKeyboardCapture();
      void subscription.then((unlisten) => unlisten()).catch(() => {});
    };
  }, [windowsClient, releaseKeyboardCapture]);

  useEffect(() => {
    if (!hasTauriRuntime()) return;
    let disposed = false;
    const applyNativeState = (state: {
      active: boolean;
      width?: number | null;
      height?: number | null;
    }) => {
      if (disposed) return;
      const wasNative = nativeVideoRef.current;
      nativeVideoRef.current = state.active;
      setNativeVideo(state.active);
      if (!state.active) {
        if (wasNative) webviewDecoderReset.current?.();
        return;
      }
      setHasFirstFrame(true);
      if (state.width && state.height) {
        setStreamSize({ w: state.width, h: state.height });
      }
    };
    let routeEventSeen = false;
    const stop = listen<{
      active: boolean;
      width?: number | null;
      height?: number | null;
    }>("vmax://native-video", ({ payload }) => {
      routeEventSeen = true;
      applyNativeState(payload);
    });
    void invoke<{
      active: boolean;
      width?: number | null;
      height?: number | null;
    }>("native_video_state")
      .then((state) => {
        if (!routeEventSeen) applyNativeState(state);
      })
      .catch(() => {});
    return () => {
      disposed = true;
      void stop.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  useEffect(() => {
    if (!fullscreen || !connected || inputBlocked) releaseKeyboardCapture();
    if (!connected || inputBlocked) releaseMouse();
  }, [fullscreen, connected, inputBlocked, releaseKeyboardCapture, releaseMouse]);

  useEffect(() => {
    if (connected && !inputBlocked && !touchAvailable)
      canvasRef.current?.focus({ preventScroll: true });
  }, [connected, inputBlocked, touchAvailable]);

  function saveControllerLayout(layout: ControllerLayout) {
    try {
      localStorage.setItem(CONTROLLER_LAYOUT_KEY, JSON.stringify(layout));
      setControllerLayout(layout);
      setEditingController(false);
      return true;
    } catch {
      return false;
    }
  }

  const toggleKeyboardCapture = async () => {
    if (keyboardCaptured.current) {
      releaseKeyboardCapture();
      return;
    }
    const shortcutScan = keyboardCodeToAtScancode(releaseKey);
    if (shortcutScan === null) return;
    setCaptureError("");
    const request = ++nextKeyboardCaptureId;
    captureRequest.current = request;
    keyboardCaptured.current = true;
    try {
      await invoke("set_fullscreen_keyboard_capture", {
        enabled: true,
        shortcutScan,
        shortcutModifiers: 6,
        captureId: request,
      });
      if (request !== captureRequest.current) {
        if (!keyboardCaptured.current) {
          await invoke("set_fullscreen_keyboard_capture", {
            enabled: false,
            shortcutScan: 0,
            shortcutModifiers: 0,
            captureId: request,
          });
        }
        return;
      }
      setKeyboardCapture(true);
      setOverlayVisible(false);
    } catch (error) {
      if (request !== captureRequest.current) return;
      keyboardCaptured.current = false;
      setCaptureError(String(error));
    }
  };

  const changeKeyboard = useCallback((visible: boolean) => {
    const field = keyboardRef.current;
    if (!field) return;
    if (visible) {
      if (document.activeElement !== field) {
        field.value = KEYBOARD_SENTINEL;
        field.focus({ preventScroll: true });
        field.setSelectionRange(1, 1);
      }
    } else {
      composing.current = false;
      field.blur();
    }
    setKeyboardOpen(visible);
    if (hasTauriRuntime()) void invoke("set_remote_keyboard", { visible }).catch(() => {});
  }, []);

  const cancelFocus = useCallback(() => {
    pendingTap.current = null;
  }, []);
  const nextTap = useCallback(() => {
    pendingTap.current = ++remoteTapId;
    return remoteTapId;
  }, []);
  const showTouchCursor = useCallback((point: StreamPoint) => {
    const cursor = touchCursorRef.current;
    const canvas = canvasRef.current;
    const bounds = canvas && getVideoBounds(canvas);
    if (!cursor || !bounds) return;
    lastTouchPoint.current = point;
    cursor.style.transform = `translate(${bounds.x + bounds.width * point.xNorm}px, ${bounds.y + bounds.height * point.yNorm}px)`;
    cursor.hidden = false;
  }, []);
  const sendDesktopTouch = useCallback(
    (input: RemoteInputPayload) => {
      if (input.kind === "touch_pointer" || input.kind === "mouse_move_abs")
        showTouchCursor(input);
      const motion =
        input.kind === "mouse_move_abs" ||
        (input.kind === "touch_pointer" && input.action === "drag");
      if (motion && pendingMotion.current?.input.kind === input.kind) {
        pendingMotion.current.input = input;
        return;
      }
      const next = { input };
      pendingMotion.current = motion ? next : null;
      touchQueue.current = touchQueue.current.then(async () => {
        if (pendingMotion.current === next) pendingMotion.current = null;
        await dispatchRemoteInput(next.input);
      });
    },
    [showTouchCursor],
  );
  const desktopTouch = useDesktopTouch(
    desktopTouchEnabled,
    mouseMode,
    mapCanvasPointerNormalized,
    sendDesktopTouch,
    nextTap,
    cancelFocus,
  );

  useEffect(() => {
    if (!desktopTouchEnabled) {
      cancelFocus();
      if (document.activeElement === keyboardRef.current) changeKeyboard(false);
    }
    if (!connected) setGaming(false);
  }, [desktopTouchEnabled, connected, cancelFocus, changeKeyboard]);

  useEffect(() => {
    if (!desktopTouchEnabled || !hardwareCursor || !canvasRef.current) {
      lastTouchPoint.current = null;
      return;
    }
    const observer = new ResizeObserver(() => {
      if (lastTouchPoint.current) showTouchCursor(lastTouchPoint.current);
    });
    observer.observe(canvasRef.current);
    return () => observer.disconnect();
  }, [desktopTouchEnabled, hardwareCursor, showTouchCursor]);

  useEffect(() => {
    if (!touchAvailable || !hasTauriRuntime()) return;
    let disposed = false;
    const stop = listen<{ tapId: number; editable: boolean }>(
      "vmax://text-focus",
      ({ payload }) => {
        if (disposed || !desktopTouchRef.current || pendingTap.current !== payload.tapId) return;
        cancelFocus();
        if (payload.editable || document.activeElement === keyboardRef.current) {
          changeKeyboard(payload.editable);
        }
      },
    );
    return () => {
      disposed = true;
      void stop.then((unlisten) => unlisten()).catch(() => {});
      if (hasTauriRuntime() && document.activeElement === keyboardRef.current) {
        void invoke("set_remote_keyboard", { visible: false }).catch(() => {});
      }
    };
  }, [touchAvailable, cancelFocus, changeKeyboard]);

  const forwardText = (text: string) => {
    if (text && desktopTouchRef.current && hasTauriRuntime()) {
      textQueue.current = textQueue.current.then(() =>
        invoke<void>("send_remote_text", { text }).catch(() => {}),
      );
    }
  };
  const flushText = (field: HTMLTextAreaElement) => {
    forwardText(field.value.replace(KEYBOARD_SENTINEL, ""));
    field.value = KEYBOARD_SENTINEL;
    field.setSelectionRange(1, 1);
  };
  const handleKeyboardInput = (event: FormEvent<HTMLTextAreaElement>) => {
    const input = event.nativeEvent as InputEvent;
    if (composing.current || input.isComposing) return;
    if (input.inputType === "deleteContentBackward") forwardText("\b");
    flushText(event.currentTarget);
  };

  useEffect(() => {
    function handlePointerLockChange() {
      setPointerLocked(document.pointerLockElement === canvasRef.current);
    }
    document.addEventListener("pointerlockchange", handlePointerLockChange);
    return () => {
      document.removeEventListener("pointerlockchange", handlePointerLockChange);
      if (document.pointerLockElement === canvasRef.current) {
        document.exitPointerLock?.();
      }
    };
  }, []);

  useEffect(() => {
    function handleOverlayHotkey(event: KeyboardEvent) {
      if (!event.ctrlKey || !event.shiftKey || event.altKey) return;
      if (event.key.toLowerCase() !== "m") return;
      event.preventDefault();
      event.stopPropagation();
      setOverlayVisible((visible) => !visible);
    }
    window.addEventListener("keydown", handleOverlayHotkey, true);
    return () => window.removeEventListener("keydown", handleOverlayHotkey, true);
  }, []);

  useEffect(() => {
    if (!hasTauriRuntime()) {
      return;
    }

    let cancelled = false;
    let decoder: VideoDecoder | null = null;
    let activeDecoderConfig: VideoDecoderConfig | null = null;
    let activeCodecByte = 0;
    let awaitingKeyframe = true;
    let lastSubmittedFrame: number | null = null;
    let consecutiveGaps = 0;
    // Decode-everything, drop-at-present: backlog alone never tears down the
    // reference chain (see lib/decodeHealth.ts).
    const decoderHealth = new DecoderHealth();
    let receivedFirstFrame = false;
    let lastKeyframeReqMs = -Infinity;
    let av1Config: VideoDecoderConfig | null = null;
    let hevcConfig: VideoDecoderConfig | null = null;
    let hevcMain10Config: VideoDecoderConfig | null = null;
    let hevc444Config: VideoDecoderConfig | null = null;
    let fullChromaCodec: string | null = null;
    let hevcSize: VideoSize | null = null;
    const decodeFailures = new Map<number, number>();
    const rejectedCodecs = new Set<number>();
    const isAndroid = /Android/i.test(navigator.userAgent);
    let refreshHz = 0;

    async function reportCapabilities(includeNative: boolean) {
      const preferred: Array<"av1" | "hevc" | "h264"> = [];
      if (av1Config && !rejectedCodecs.has(CODEC_AV1)) preferred.push("av1");
      if (hevcConfig && !rejectedCodecs.has(CODEC_HEVC)) preferred.push("hevc");
      preferred.push("h264");
      const scale = window.devicePixelRatio || 1;
      await invoke("report_client_capabilities", {
        codecs: preferred,
        nativeDecoder: includeNative,
        display: {
          deviceType: isAndroid ? "mobile" : "desktop",
          screenWidth: Math.min(16384, Math.round(window.screen.width * scale)),
          screenHeight: Math.min(16384, Math.round(window.screen.height * scale)),
          maxRefreshHz: refreshHz,
          maxDecodeMbps: 0,
          supportsHaptics: isAndroid || /Windows|Linux/i.test(navigator.userAgent),
          hevc444: Boolean(hevc444Config && !rejectedCodecs.has(CODEC_HEVC_444)),
          hevcMain10: Boolean(hevcMain10Config && hevcConfig && !rejectedCodecs.has(CODEC_HEVC_MAIN10)),
        },
      });
    }
    let h264Config: DecoderConfigWithLatency = {
      codec: H264_CANDIDATE_STRINGS[0],
      optimizeForLatency: true,
      latencyMode: "realtime",
    };
    const decodeSubmittedAt = new Map<number, number>();
    let emaDecodeMs: number | null = null;
    let videoBytes = 0;
    let drawnFrames = 0;
    let lastDrawnAt = 0;
    let decodeSamples = 0;
    let decodeTotalMs = 0;
    let lastPlaybackSample = performance.now();
    let subscribed = false;
    let lastCodecLabel = "";

    // Vsync-paced presentation: at most one draw per display refresh and the
    // newest decoded frame always wins. Presenting straight from the decoder
    // callback handed the compositor two frames for one refresh and let a
    // backlog of already-decoded frames turn into permanent display latency.
    const presenter = new FramePresenter(
      () => canvasRef.current,
      () => fallbackCanvasRef.current,
      () => sharpnessRef.current,
      {
        onSize: (width, height) => setStreamSize({ w: width, h: height }),
        onDrawn: () => {
          drawnFrames++;
          lastDrawnAt = performance.now();
          if (!receivedFirstFrame) {
            receivedFirstFrame = true;
            setHasFirstFrame(true);
          }
        },
      },
    );

    const playbackTimer = window.setInterval(() => {
      const now = performance.now();
      const elapsedMs = Math.max(1, now - lastPlaybackSample);
      const presentWindow = presenter.takeWindow();
      if (receivedFirstFrame) {
        const windowDecodeMs = decodeSamples ? decodeTotalMs / decodeSamples : emaDecodeMs;
        if (windowDecodeMs !== null && windowDecodeMs > 0) {
          void invoke("report_client_decode_latency", {
            decodeUs: Math.round(windowDecodeMs * 1000),
          }).catch(() => {});
        }
        const measuredMbps = (videoBytes * 8) / elapsedMs / 1000;
        setPlayback({
          fps: Math.round((drawnFrames * 1000) / elapsedMs),
          decodeMs: windowDecodeMs,
          videoMbps: measuredMbps,
          intervalMs: presentWindow.intervalMs,
          jitterMs: presentWindow.jitterMs,
          superseded: presentWindow.superseded,
        });
      }
      const waitingForFirstFrame = nativeVideoRef.current
        ? !nativeFrameSeen.current
        : !receivedFirstFrame;
      if (subscribed && connectedRef.current && !document.hidden && waitingForFirstFrame) {
        requestSyncKeyframe("awaiting_first_frame");
      }
      if (
        receivedFirstFrame &&
        !nativeVideoRef.current &&
        !document.hidden &&
        now - lastDrawnAt >= 1_000 &&
        recoverStallsRef.current
      ) {
        requestSyncKeyframe("video_stalled");
      }
      lastPlaybackSample = now;
      videoBytes = drawnFrames = decodeSamples = decodeTotalMs = 0;
    }, 500);

    /**
     * Asks the host for a clean reference.
     *
     * `lastGoodFrameId` names the newest frame this viewer can still decode.
     * Naming one lets the host answer with an intra-refresh wave, which costs a
     * few bits per frame; passing `null` means "no usable reference" and buys a
     * full IDR, which is a keyframe-sized burst. Only a decoder that has been
     * rebuilt is in that second state.
     */
    const requestSyncKeyframe = (reason: string, lastGoodFrameId: number | null = null) => {
      const now = performance.now();
      if (now - lastKeyframeReqMs < KEYFRAME_REQUEST_COOLDOWN_MS) {
        return;
      }
      lastKeyframeReqMs = now;
      void invoke("request_keyframe", { reason, lastGoodFrameId }).catch(() => {});
    };

    const clearPendingFrames = () => {
      awaitingKeyframe = true;
      lastSubmittedFrame = null;
      consecutiveGaps = 0;
      decoderHealth.reset();
      decodeSubmittedAt.clear();
      // A frame decoded for the previous reference chain must never be shown
      // after a reset.
      presenter.discard();
    };

    const closeDecoder = () => {
      const previous = decoder;
      decoder = null;
      activeDecoderConfig = null;
      activeCodecByte = 0;
      clearPendingFrames();
      if (previous && previous.state !== "closed") previous.close();
    };

    webviewDecoderReset.current = () => {
      closeDecoder();
      receivedFirstFrame = false;
      setPlayback(null);
      setHasFirstFrame(false);
      requestSyncKeyframe("native_video_fallback");
    };

    const resetDecoder = () => {
      clearPendingFrames();
      try {
        if (decoder?.state === "configured") decoder.reset();
      } catch {
        closeDecoder();
      }
    };

    const recoverOnVisible = () => {
      // A frame decoded just before the tab was hidden must not surface on the
      // way back: it would show a stale picture for a refresh.
      if (document.hidden) presenter.discard();
      if (!nativeVideoRef.current && !document.hidden && awaitingKeyframe)
        requestSyncKeyframe("viewer_visible");
    };
    document.addEventListener("visibilitychange", recoverOnVisible);

    const handleDecodeFailure = (codec: number, reason: string) => {
      const failures = (decodeFailures.get(codec) ?? 0) + 1;
      decodeFailures.set(codec, failures);
      if (codec === CODEC_H264 && failures === 3) {
        // Two consecutive errors can be a transient (a driver reset while the
        // display mode changes, a GPU handoff). Downgrading to software decode
        // on that is a permanent latency increase for the rest of the session,
        // so it takes three.
        h264Config = { ...h264Config, hardwareAcceleration: "prefer-software" };
      }
      if (codec === CODEC_AV1 && failures >= 2) {
        if (rejectedCodecs.has(codec)) return;
        rejectedCodecs.add(codec);
        setStreamNotice("AV1 is struggling here — switching codec…");
        void reportCapabilities(false).then(() => requestSyncKeyframe(reason)).catch(() => {
          setStreamNotice("Couldn’t switch away from AV1. Reconnect to retry.");
        });
        return;
      }
      if (isHevc(codec) && failures >= 2) {
        if (rejectedCodecs.has(codec)) return;
        rejectedCodecs.add(codec);
        if (codec === CODEC_HEVC) rejectedCodecs.add(CODEC_HEVC_MAIN10);
        setStreamNotice("Switching to a compatible video codec…");
        void reportCapabilities(false).then(() => requestSyncKeyframe(reason)).catch(() => {
          setStreamNotice("Couldn’t request a compatible video codec. Reconnect to retry.");
        });
        return;
      }
      requestSyncKeyframe(reason);
    };

    const ensureDecoderForCodec = (codecByte: number): VideoDecoder | null => {
      if (rejectedCodecs.has(codecByte)) return null;
      if (typeof VideoDecoder === "undefined") {
        setStreamNotice("WebCodecs unavailable in this WebView");
        return null;
      }
      if (decoder && activeDecoderConfig && activeCodecByte === codecByte) {
        try {
          if (decoder.state === "unconfigured") decoder.configure(activeDecoderConfig);
          if (decoder.state === "configured") return decoder;
        } catch {
          closeDecoder();
        }
      }

      const hevc = codecByte === CODEC_HEVC_444
        ? hevc444Config && { ...hevc444Config, codec: fullChromaCodec ?? hevc444Config.codec }
        : codecByte === CODEC_HEVC_MAIN10 ? hevcMain10Config : hevcConfig;
      const config = isHevc(codecByte)
        ? hevc &&
          (hevcSize
            ? {
                ...hevc,
                codedWidth: hevcSize.width,
                codedHeight: hevcSize.height,
                displayAspectWidth: hevcSize.width,
                displayAspectHeight: hevcSize.height,
              }
            : hevc)
        : codecByte === CODEC_H264
          ? h264Config
          : codecByte === CODEC_AV1
            ? av1Config
            : null;
      if (!config) {
        handleDecodeFailure(codecByte, "unsupported_codec");
        return null;
      }

      closeDecoder();

      const instance = new VideoDecoder({
        output: (frame: VideoFrame) => {
          if (cancelled || decoder !== instance) {
            frame.close();
            return;
          }
          const submittedAt = decodeSubmittedAt.get(frame.timestamp);
          if (submittedAt !== undefined) {
            const deltaMs = Math.max(0.05, performance.now() - submittedAt);
            emaDecodeMs = emaDecodeMs === null ? deltaMs : emaDecodeMs * 0.85 + deltaMs * 0.15;
            decodeSamples++;
            decodeTotalMs += deltaMs;
            decodeSubmittedAt.delete(frame.timestamp);
          }
          decodeFailures.delete(codecByte);
          decoderHealth.noteProgress(performance.now());
          // The presenter owns the frame now: it is closed after the refresh
          // it is drawn on, or immediately when a newer frame supersedes it.
          presenter.submit(frame);
        },
        error: () => {
          if (decoder !== instance) return;
          closeDecoder();
          handleDecodeFailure(codecByte, "webcodecs_decode_error");
        },
      });

      try {
        instance.configure(config);
        decoder = instance;
        activeDecoderConfig = config;
        activeCodecByte = codecByte;
        awaitingKeyframe = true;
        return decoder;
      } catch {
        try {
          instance.close();
        } catch {
          // Ignore an already-closed decoder.
        }
        handleDecodeFailure(codecByte, "codec_configure_failed");
        return null;
      }
    };

    const startPipeline = async () => {
      const [fullChroma, main10, hevc, h264, av1, measuredRefreshHz] = await Promise.all([
        probeDecoder(HEVC_444_CANDIDATES),
        probeDecoder(HEVC_MAIN10_CANDIDATE_STRINGS),
        probeDecoder(HEVC_CANDIDATE_STRINGS),
        probeDecoder(H264_CANDIDATE_STRINGS),
        av1HardwareDecodeAvailable().then((hardware) =>
          hardware ? probeDecoder(AV1_CANDIDATE_STRINGS) : null,
        ),
        measureRefreshHz(),
      ]);
      if (cancelled) {
        return;
      }
      // Measured next to the decoder probes so the host can pace the encoder at
      // the rate this display can actually present.
      refreshHz = measuredRefreshHz;

      av1Config = av1;
      hevcConfig = hevc;
      hevc444Config = fullChroma;
      hevcMain10Config = main10;
      if (h264) h264Config = h264;

      void reportCapabilities(isAndroid).catch(() => {});
      if (!isAndroid && !hevc && !h264 && typeof VideoDecoder === "undefined") {
        setStreamNotice("This WebView has no video decoder. Update the system WebView to connect.");
      }

      const onFrame = new Channel<ArrayBuffer>();
      onFrame.onmessage = (payload) => {
        if (cancelled) {
          return;
        }
        const bytes = normalizeBinaryFrame(payload);
        if (!bytes || bytes.byteLength <= HEADER_SIZE_BYTES) {
          return;
        }

        const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
        const frameId = view.getUint32(0, false);
        const tsUs = Number(view.getBigUint64(4, false));
        const codecByte = bytes[12];
        const isKeyframe = bytes[13] === 1;
        const codecLabel =
          codecByte === CODEC_AV1
            ? "AV1"
            : codecByte === CODEC_H264
              ? "H.264"
              : codecByte === CODEC_HEVC_MAIN10
                ? "H.265 10-bit"
                : codecByte === CODEC_HEVC_444
                  ? "H.265 4:4:4"
                  : "H.265";
        // Re-rendering the overlay on every frame is pure main-thread cost at
        // 60-144 FPS; only an actual codec change needs a repaint.
        if (codecLabel !== lastCodecLabel) {
          lastCodecLabel = codecLabel;
          setActiveCodecLabel(codecLabel);
        }

        const annexBPayload = bytes.subarray(HEADER_SIZE_BYTES);
        videoBytes += annexBPayload.byteLength;

        if (isKeyframe && isHevc(codecByte)) {
          const size = hevcSequence(annexBPayload);
          if (codecByte === CODEC_HEVC_444 && size?.chroma === 3) {
            if (fullChromaCodec && fullChromaCodec !== size.codec) closeDecoder();
            fullChromaCodec = size.codec;
          }
          if (size && (size.width !== hevcSize?.width || size.height !== hevcSize.height)) {
            hevcSize = size;
            if (isHevc(activeCodecByte)) closeDecoder();
          }
        }

        const gap = lastSubmittedFrame !== null && ((lastSubmittedFrame + 1) >>> 0) !== frameId;
        const now = performance.now();
        // Backlog is only a problem when the decoder is no longer keeping up,
        // and only a genuine stall justifies tearing down the reference chain.
        // A three-frame queue is what a burst of datagrams looks like.
        const verdict = decoderHealth.evaluate(decoder?.decodeQueueSize ?? 0, now);
        if (verdict.action === "reset") {
          consecutiveGaps = 0;
          resetDecoder();
        } else if (gap && !isKeyframe) {
          // A single lost delta breaks the reference chain, but the decoder
          // conceals it and the host heals it with intra-refresh. Freezing the
          // picture until the next IDR on every isolated loss is exactly what
          // makes a healthy 60 FPS stream feel like it stutters, so ask for a
          // clean reference in the background and keep presenting. Naming the
          // last good frame keeps that request cheap: the host refreshes the
          // picture progressively instead of emitting an IDR burst.
          consecutiveGaps++;
          requestSyncKeyframe("frame_loss", lastSubmittedFrame);
          if (consecutiveGaps >= MAX_TOLERATED_GAPS) {
            consecutiveGaps = 0;
            resetDecoder();
          }
        } else {
          consecutiveGaps = 0;
        }

        const activeDecoder = ensureDecoderForCodec(codecByte);
        if (!activeDecoder) {
          return;
        }

        if (awaitingKeyframe) {
          if (!isKeyframe) {
            // Nothing in this decoder can be decoded until a keyframe lands, so
            // this request must be for an IDR, not a refresh wave.
            requestSyncKeyframe(
              verdict.reason ? `decoder_${verdict.reason}` : "awaiting_sync_frame",
            );
            return;
          }
          awaitingKeyframe = false;
        }

        try {
          if (decodeSubmittedAt.size >= 64) {
            const oldest = decodeSubmittedAt.keys().next();
            if (!oldest.done) decodeSubmittedAt.delete(oldest.value);
          }
          decodeSubmittedAt.set(tsUs, performance.now());
          activeDecoder.decode(
            new EncodedVideoChunk({
              type: isKeyframe ? "key" : "delta",
              timestamp: tsUs,
              data: annexBPayload,
            }),
          );
          lastSubmittedFrame = frameId;
        } catch {
          closeDecoder();
          handleDecodeFailure(codecByte, "chunk_decode_exception");
        }
      };

      try {
        await invoke("subscribe_stream_frames", { onFrame });
        subscribed = true;
      } catch (error) {
        if (!cancelled) setStreamNotice(`Viewer could not open the video channel. ${String(error)}`);
      }
    };

    void startPipeline();

    return () => {
      cancelled = true;
      webviewDecoderReset.current = null;
      document.removeEventListener("visibilitychange", recoverOnVisible);
      window.clearInterval(playbackTimer);
      presenter.dispose();
      closeDecoder();
    };
  }, []);

  useEffect(() => {
    const pressedScancodes = new Set<number>();

    const onKeyDown = (event: KeyboardEvent) => {
      if (keyboardCaptured.current) return;
      if (isLocalSystemKey(event)) {
        releaseHeldKeys();
        return;
      }
      if (
        settingsOpenRef.current ||
        event.target === keyboardRef.current ||
        (event.target instanceof Element && event.target.closest("[data-stream-controls]"))
      ) {
        return;
      }
      const scanCode = keyboardCodeToAtScancode(event.code);
      if (scanCode === null) {
        return;
      }
      event.preventDefault();
      pressedScancodes.add(scanCode);
      dispatchRemoteInput({
        kind: "key",
        scanCode,
        pressed: true,
        modifiers: extractModifierMask(event),
      });
    };

    const onKeyUp = (event: KeyboardEvent) => {
      const scanCode = keyboardCodeToAtScancode(event.code);
      if (scanCode === null || !pressedScancodes.has(scanCode)) {
        return;
      }
      if (!isLocalSystemKey(event)) event.preventDefault();
      pressedScancodes.delete(scanCode);
      dispatchRemoteInput({
        kind: "key",
        scanCode,
        pressed: false,
        modifiers: extractModifierMask(event),
      });
    };

    function releaseHeldKeys() {
      if (pressedScancodes.size === 0) {
        return;
      }
      for (const scanCode of pressedScancodes) {
        dispatchRemoteInput({
          kind: "key",
          scanCode,
          pressed: false,
          modifiers: 0,
        });
      }
      pressedScancodes.clear();
    }

    const onBlur = () => {
      releaseHeldKeys();
      releaseMouse();
      releaseKeyboardCapture();
      if (document.pointerLockElement && document.pointerLockElement === canvasRef.current) {
        document.exitPointerLock();
      }
    };

    const onMouseUp = (event: MouseEvent) => {
      if (pressedMouse.current.delete(event.button)) {
        void dispatchRemoteInput({ kind: "mouse_button", button: event.button, pressed: false });
      }
    };

    const releaseOnHidden = () => {
      if (document.hidden) onBlur();
    };
    document.addEventListener("visibilitychange", releaseOnHidden);
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);
    window.addEventListener("blur", onBlur);
    window.addEventListener("mouseup", onMouseUp, true);
    return () => {
      document.removeEventListener("visibilitychange", releaseOnHidden);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("mouseup", onMouseUp, true);
      onBlur();
    };
  }, [releaseKeyboardCapture, releaseMouse]);

  const handleCanvasPointerMove = (event: ReactPointerEvent<HTMLCanvasElement>) => {
    if (event.pointerType === "touch") {
      desktopTouch.move(event);
      return;
    }
    if (inputBlocked) {
      return;
    }
    const canvas = canvasRef.current;
    if (!canvas) {
      return;
    }

    if (document.pointerLockElement === canvas) {
      const dx = Math.round(event.movementX);
      const dy = Math.round(event.movementY);
      if (dx !== 0 || dy !== 0) {
        dispatchRemoteInput({ kind: "mouse_move_rel", dx, dy });
      }
      return;
    }

    const coords = mapCanvasPointerNormalized(canvas, event.clientX, event.clientY, true);
    if (coords) {
      dispatchRemoteInput({
        kind: "mouse_move_abs",
        xNorm: coords.xNorm,
        yNorm: coords.yNorm,
      });
    }
  };

  const handleCanvasPointerDown = (event: ReactPointerEvent<HTMLCanvasElement>) => {
    setOverlayVisible(false);
    if (event.pointerType === "touch") {
      if (desktopTouchEnabled && mouseMode === "direct") {
        const point = mapCanvasPointerNormalized(
          event.currentTarget,
          event.clientX,
          event.clientY,
        );
        if (point) showTouchCursor(point);
      }
      desktopTouch.down(event);
      return;
    }
    if (inputBlocked) {
      return;
    }
    const canvas = canvasRef.current;
    if (canvas && document.pointerLockElement !== canvas) {
      canvas.focus({ preventScroll: true });
      const coords = mapCanvasPointerNormalized(canvas, event.clientX, event.clientY, true);
      if (coords) {
        dispatchRemoteInput({
          kind: "mouse_move_abs",
          xNorm: coords.xNorm,
          yNorm: coords.yNorm,
        });
      }
    }
    if (document.pointerLockElement !== canvas) {
      event.currentTarget.setPointerCapture(event.pointerId);
    }
  };

  const handleMouseDown = (event: ReactMouseEvent<HTMLCanvasElement>) => {
    const nativeEvent = event.nativeEvent as MouseEvent & {
      sourceCapabilities?: { firesTouchEvents?: boolean };
    };
    if (nativeEvent.sourceCapabilities?.firesTouchEvents) return;
    if (inputBlocked || event.button < 0 || event.button > 4) return;
    event.preventDefault();
    pressedMouse.current.add(event.button);
    void dispatchRemoteInput({ kind: "mouse_button", button: event.button, pressed: true });
  };

  const handleCanvasPointerUp = (event: ReactPointerEvent<HTMLCanvasElement>) => {
    if (event.pointerType === "touch") {
      desktopTouch.end(event);
      return;
    }
  };

  const handleCanvasWheel = useCallback(
    (event: ReactWheelEvent<HTMLCanvasElement>) => {
      if (inputBlocked) {
        return;
      }
      const scale = event.deltaMode === 1 ? 40 : 1.2;
      const clampI16 = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v)));
      const deltaX = clampI16(event.deltaX * scale);
      const deltaY = clampI16(event.deltaY * scale);
      if (deltaX !== 0 || deltaY !== 0) {
        dispatchRemoteInput({
          kind: "mouse_wheel",
          deltaX,
          deltaY,
        });
      }
    },
    [inputBlocked],
  );

  const handleTogglePointerLock = useCallback(async () => {
    const canvas = canvasRef.current;
    if (!canvas) {
      return;
    }
    if (document.pointerLockElement === canvas) {
      document.exitPointerLock?.();
    } else {
      setPointerLockError("");
      canvas.focus({ preventScroll: true });
      setOverlayVisible(false);
      try {
        await canvas.requestPointerLock();
      } catch {
        setPointerLockError("Mouse lock failed. Click the stream and try again.");
      }
    }
  }, []);

  const handleContextMenu = useCallback((event: ReactMouseEvent<HTMLCanvasElement>) => {
    event.preventDefault();
  }, []);

  async function applyBitrateLimit(mbps: number) {
    setBitrateFeedback(null);
    try {
      await invoke("set_stream_bitrate_limit", { mbps });
      setBitrateFeedback(mbps === MAX_STREAM_BITRATE_MBPS ? "Using the host’s limit" : `Requested up to ${mbps} Mbps`);
    } catch (error) {
      setBitrateFeedback(String(error));
    }
  }

  /**
   * Builds and copies the plain-text diagnostics block. Everything it reports
   * is what the user is already looking at in this panel, plus the encoder
   * report the host attaches to the pipeline label.
   */
  async function copyStreamDiagnostics() {
    setDiagnosticsFeedback(null);
    let version: string | null = null;
    try {
      version = await getVersion();
    } catch {
      version = null;
    }
    const canvas = canvasRef.current;
    const report = buildDiagnosticsReport(
      {
        pipelineLabel: parsecFormatString,
        codec: activeCodecLabel,
        requestedResolution: resolution,
        decodedWidth: streamSize?.w ?? dims.width,
        decodedHeight: streamSize?.h ?? dims.height,
        displayWidth: canvas?.clientWidth,
        displayHeight: canvas?.clientHeight,
        fps: displayFps ?? null,
        bitrateMbps: displayBitrateMbps,
        rttMs: pingMs,
        networkMs: displayNetworkMs,
        packetLossPct,
        decodeMs: displayDecodeMs,
        encodeMs: displayEncodeMs,
        presentIntervalMs: playback?.intervalMs ?? null,
        presentJitterMs: playback?.jitterMs ?? null,
        supersededFrames: playback?.superseded ?? null,
        transport: pathLabel,
        nativePath: nativeVideo,
        sharpness: viewerSharpness,
      },
      {
        version,
        userAgent: typeof navigator !== "undefined" ? navigator.userAgent : undefined,
        platform: typeof navigator !== "undefined" ? navigator.platform : undefined,
        devicePixelRatio: typeof window !== "undefined" ? window.devicePixelRatio : undefined,
      },
    );
    const copied = await copyDiagnostics(report);
    setDiagnosticsFeedback(
      copied ? "Diagnostics copied to the clipboard" : "Could not copy the diagnostics",
    );
  }

  const hideCursor = !hardwareCursor || pointerLocked;
  const touchButton =
    "inline-flex min-h-11 items-center gap-2 rounded-xl border border-zinc-800 bg-zinc-950 px-3.5 text-xs font-medium text-zinc-200 hover:bg-zinc-800";
  const pathLabel = connectionPathLabel(connectionPath, relayed);

  // Parsec-exact 2-decimal-place telemetry calculations
  const displayDecodeMs =
    playback?.decodeMs ?? (parsecTelemetry && parsecTelemetry.decodeMs > 0 ? parsecTelemetry.decodeMs : null);
  const displayEncodeMs =
    hostEncodeMs ?? (parsecTelemetry && parsecTelemetry.encodeMs > 0 ? parsecTelemetry.encodeMs : null);
  const displayNetworkMs =
    parsecTelemetry && parsecTelemetry.networkMs > 0
      ? parsecTelemetry.networkMs
      : pingMs > 0
        ? pingMs / 2.0
        : null;
  // Everything VmaX itself controls on the way to the glass: host encode, the
  // one-way trip, and client decode.  Propagation and display refresh are not
  // in here because no encoder setting can shorten them.
  const displayPipelineMs =
    displayEncodeMs !== null && displayDecodeMs !== null && displayNetworkMs !== null
      ? displayEncodeMs + displayDecodeMs + displayNetworkMs
      : null;
  const displayFps = nativeVideo ? nativeFps : playback?.fps;
  const displayBitrateMbps =
    parsecTelemetry?.bitrateMbps ?? playback?.videoMbps ?? 0;
  const displayWidth = streamSize?.w ?? dims.width;
  const displayHeight = streamSize?.h ?? dims.height;
  const parsecFormatString =
    parsecTelemetry?.pipelineLabel ||
    `${activeCodecLabel} ${displayWidth}x${displayHeight}`;

  function closeOverlay() {
    setOverlayVisible(false);
    setConnectionOpen(false);
  }

  function toggleController() {
    cancelFocus();
    if (keyboardOpen) changeKeyboard(false);
    setGaming(!gaming);
    closeOverlay();
  }

  function openControllerEditor() {
    cancelFocus();
    if (keyboardOpen) changeKeyboard(false);
    if (document.pointerLockElement === canvasRef.current) document.exitPointerLock?.();
    closeOverlay();
    setGaming(true);
    setEditingController(true);
  }

  return (
    <div className="vmax-stream relative w-full min-w-0 min-h-0 flex-1 overflow-hidden bg-zinc-950 flex flex-col select-none">
      <div className="relative flex-1 min-w-0 min-h-0">
        <canvas
          id="stream-canvas"
          ref={canvasRef}
          tabIndex={-1}
          onPointerMove={handleCanvasPointerMove}
          onPointerDown={handleCanvasPointerDown}
          onMouseDown={handleMouseDown}
          onAuxClick={(event) => event.preventDefault()}
          onPointerUp={handleCanvasPointerUp}
          onPointerCancel={(event) => {
            if (event.pointerType === "touch") desktopTouch.end(event, true);
            else releaseMouse();
          }}
          onLostPointerCapture={(event) => {
            if (event.pointerType === "touch") desktopTouch.end(event, true);
          }}
          onWheel={handleCanvasWheel}
          onContextMenu={handleContextMenu}
          aria-label="Remote stream video canvas"
          className={`w-full h-full max-w-full max-h-full object-contain block touch-none outline-none ${nativeVideo ? "opacity-0" : ""} ${hideCursor ? "cursor-none" : "cursor-default"}`}
        />

        <canvas ref={fallbackCanvasRef} hidden aria-hidden="true"
          className={`pointer-events-none absolute inset-0 h-full w-full object-contain ${nativeVideo ? "opacity-0" : ""}`} />
        {desktopTouchEnabled && hardwareCursor && (
          <div
            ref={touchCursorRef}
            hidden
            aria-hidden="true"
            className="pointer-events-none absolute top-0 left-0"
          >
            <MousePointer2 className="-translate-x-1 -translate-y-0.5 h-4 w-4 fill-white stroke-zinc-950 drop-shadow-md" />
          </div>
        )}
        {!hasFirstFrame && (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <div className="rounded-xl border border-zinc-800/80 bg-zinc-950/85 px-4 py-2 font-mono text-xs text-zinc-400">
              {streamNotice}
            </div>
          </div>
        )}
      </div>

      {touchAvailable && (
        <textarea
          ref={keyboardRef}
          aria-label="Remote keyboard"
          tabIndex={-1}
          autoCapitalize="off"
          autoComplete="off"
          autoCorrect="off"
          spellCheck={false}
          className="pointer-events-none absolute left-1/2 top-1/2 h-4 w-4 resize-none p-0 text-base opacity-0"
          onInput={handleKeyboardInput}
          onCompositionStart={() => {
            composing.current = true;
          }}
          onCompositionEnd={(event) => {
            composing.current = false;
            if (document.activeElement === event.currentTarget) flushText(event.currentTarget);
          }}
          onBlur={() => {
            composing.current = false;
            setKeyboardOpen(false);
          }}
          onKeyDown={(event) => {
            if (event.key === "Escape") {
              event.preventDefault();
              changeKeyboard(false);
            }
          }}
        />
      )}

      {connected && !inputBlocked && touchControllerAvailable && gaming && (
        <TouchGamepad
          layout={controllerLayout}
          opacity={controllerOpacity}
          size={controllerSize}
        />
      )}

      {connected && editingController && !settingsOpen && (
        <ControllerEditor
          initial={controllerLayout}
          opacity={controllerOpacity}
          size={controllerSize}
          onSave={saveControllerLayout}
          onClose={() => setEditingController(false)}
        />
      )}

      <TelemetryBar visible={connected && !gaming && showTelemetryPill && !overlayVisible} />

      {connected && !editingController && (
        <div
          data-stream-controls
          onPointerDown={(event) => {
            if (
              document.activeElement === keyboardRef.current &&
              event.target instanceof Element &&
              event.target.closest("button")
            )
              event.preventDefault();
          }}
          className="absolute top-1 left-3 z-40 flex w-max max-w-[calc(100vw-1.5rem)] flex-col items-start gap-2 pt-[env(safe-area-inset-top)]"
        >
          <button
            type="button"
            aria-expanded={overlayVisible}
            aria-controls={touchAvailable ? "stream-touch-toolbar" : "stream-overlay-panel"}
            aria-label={overlayVisible ? "Hide stream controls" : "Show stream controls"}
            onClick={() => {
              setOverlayVisible(!overlayVisible);
              setConnectionOpen(false);
            }}
            className="stream-handle"
            data-compact={gaming || !showTelemetryPill}
          >
            {!gaming && showTelemetryPill && (
              <>
                <span className="stream-brand">
                  <b>V</b> VmaX
                </span>
                <span className="stream-summary">
                  <Activity size={15} /> {pathLabel}
                </span>
                <span className="stream-summary">
                  Decode {displayDecodeMs !== null ? `${displayDecodeMs.toFixed(2)}ms` : "—"}
                </span>
                <span className="stream-summary">
                  Encode {displayEncodeMs !== null ? `${displayEncodeMs.toFixed(2)}ms` : "—"}
                </span>
                <span className="stream-summary">
                  Network {displayNetworkMs !== null ? `${displayNetworkMs.toFixed(2)}ms` : "—"}
                </span>
                <span
                  className={`stream-summary ${displayPipelineMs !== null && displayPipelineMs <= 20 ? "text-emerald-300" : ""}`}
                  title="Encode + one-way network + decode"
                >
                  Pipeline {displayPipelineMs !== null ? `${displayPipelineMs.toFixed(2)}ms` : "—"}
                </span>
                <span className="stream-summary stream-summary-video">
                  Bitrate {displayBitrateMbps.toFixed(2)}Mbps · {displayFps ?? "—"} FPS
                </span>
              </>
            )}
            {(gaming || !showTelemetryPill) && <span aria-hidden="true">V</span>}
            <ChevronDown size={20} className={overlayVisible ? "rotate-180" : ""} />
          </button>

          {overlayVisible && touchAvailable && (
            <div
              id="stream-touch-toolbar"
              className="stream-toolbar"
              role="toolbar"
              aria-label="Stream controls"
            >
              <span className="stream-brand">
                <b>V</b> VmaX
              </span>
              <button
                type="button"
                aria-label="Toggle FPS mouse lock"
                title="Relative mouse for games"
                aria-pressed={pointerLocked}
                onClick={handleTogglePointerLock}
              >
                <Crosshair size={20} />
              </button>
              <button
                type="button"
                aria-label="Keyboard"
                title="Keyboard"
                aria-pressed={keyboardOpen}
                disabled={!desktopTouchEnabled}
                onClick={() => {
                  cancelFocus();
                  changeKeyboard(!keyboardOpen);
                  closeOverlay();
                }}
              >
                <Keyboard size={20} />
              </button>
              <button
                type="button"
                aria-label="Toggle mouse mode"
                title={`Mouse: ${mouseMode === "trackpad" ? "Trackpad" : "Direct touch"}`}
                aria-pressed={mouseMode === "direct" && !gaming}
                onClick={() => {
                  if (gaming) toggleController();
                  else {
                    setMouseMode(mouseMode === "trackpad" ? "direct" : "trackpad");
                    closeOverlay();
                  }
                }}
              >
                <MousePointer2 size={20} />
              </button>
              {touchControllerAvailable && (
                <>
                  <button
                    type="button"
                    aria-pressed={gaming}
                    disabled={settingsOpen}
                    aria-label={gaming ? "Hide touch controller" : "Show touch controller"}
                    title="Touch controller"
                    onClick={toggleController}
                  >
                    <Gamepad2 size={22} />
                  </button>
                  <button
                    type="button"
                    disabled={settingsOpen}
                    aria-label="Edit controller layout"
                    title="Edit controller layout"
                    onClick={openControllerEditor}
                  >
                    <SlidersHorizontal size={19} />
                  </button>
                </>
              )}
              <button
                type="button"
                aria-label="Connection details"
                title="Connection details"
                aria-expanded={connectionOpen}
                onClick={() => setConnectionOpen(!connectionOpen)}
              >
                <Activity size={19} />
              </button>
              <button
                type="button"
                aria-label="Open settings"
                title="Stream settings"
                onClick={onOpenSettings}
              >
                <Settings size={19} />
              </button>
              {onOpenFriends && (
                <button
                  type="button"
                  aria-label="Friends"
                  title="Friends"
                  onClick={onOpenFriends}
                >
                  <Users size={19} />
                </button>
              )}
              {fullscreen !== undefined && onToggleFullscreen && (
                <button
                  type="button"
                  aria-label={fullscreen ? "Exit fullscreen" : "Fullscreen"}
                  title={fullscreen ? "Exit fullscreen" : "Fullscreen"}
                  onClick={onToggleFullscreen}
                >
                  {fullscreen ? <Minimize size={19} /> : <Maximize size={19} />}
                </button>
              )}
              <button
                type="button"
                className="stream-disconnect"
                aria-label="Disconnect"
                title="Disconnect"
                onClick={onDisconnect}
              >
                <LogOut size={20} />
              </button>
            </div>
          )}

          <div
            id="stream-overlay-panel"
            aria-label={touchAvailable ? "Connection details" : "Stream controls"}
            className={
              overlayVisible && (!touchAvailable || connectionOpen)
                ? "flex max-h-[min(64dvh,28rem)] w-[min(22rem,calc(100vw-1.5rem))] flex-col overflow-y-auto overscroll-contain rounded-2xl border border-zinc-800 bg-zinc-950/95 shadow-2xl backdrop-blur"
                : "hidden"
            }
          >
            <div className="space-y-2.5 border-b border-zinc-800 px-4 py-3">
              <div className="flex items-center justify-between font-mono text-[10px] uppercase tracking-widest text-zinc-500">
                <span>Connection</span>
                <span className="tabular-nums text-zinc-400" title="Live decoded stream resolution">
                  {displayWidth}×{displayHeight}
                </span>
                {touchAvailable && (
                  <button
                    type="button"
                    aria-label="Close connection details"
                    onClick={() => setConnectionOpen(false)}
                  >
                    <X size={17} />
                  </button>
                )}
              </div>
              <p className="font-mono text-[10px] text-zinc-400">{parsecFormatString}</p>
              <dl className="grid grid-cols-2 gap-x-4 gap-y-1.5 font-mono text-[11px]">
                <dt className="text-zinc-500">Decode</dt>
                <dd className="tabular-nums font-semibold text-white">
                  {displayDecodeMs !== null ? `${displayDecodeMs.toFixed(2)} ms` : "—"}
                </dd>
                <dt className="text-zinc-500">Encode</dt>
                <dd className="tabular-nums font-semibold text-white">
                  {displayEncodeMs !== null ? `${displayEncodeMs.toFixed(2)} ms` : "—"}
                </dd>
                <dt className="text-zinc-500">Network</dt>
                <dd
                  title="One-way network latency (RTT / 2)"
                  className={`tabular-nums font-semibold ${displayNetworkMs !== null && displayNetworkMs <= 15 ? "text-emerald-300" : "text-white"}`}
                >
                  {displayNetworkMs !== null ? `${displayNetworkMs.toFixed(2)} ms` : "—"}
                </dd>
                <dt className="text-zinc-500">Pipeline</dt>
                <dd
                  title="Encode + one-way network + decode. Everything VmaX controls; display refresh and propagation are extra"
                  className={`tabular-nums font-semibold ${displayPipelineMs !== null && displayPipelineMs <= 20 ? "text-emerald-300" : "text-white"}`}
                >
                  {displayPipelineMs !== null ? `${displayPipelineMs.toFixed(2)} ms` : "—"}
                </dd>
                <dt className="text-zinc-500">Bitrate</dt>
                <dd className="tabular-nums font-semibold text-white">
                  {`${displayBitrateMbps.toFixed(2)} Mbps`}
                </dd>
                <dt className="text-zinc-500">FPS drawn</dt>
                <dd className="tabular-nums text-zinc-200">{displayFps ?? "—"}</dd>
                <dt className="text-zinc-500">Frame pacing</dt>
                <dd
                  title="Median interval between presented frames and the 95th-percentile deviation from it"
                  className={`tabular-nums font-semibold ${
                    playback && playback.intervalMs > 0 && playback.jitterMs <= 1.2
                      ? "text-emerald-300"
                      : "text-white"
                  }`}
                >
                  {playback && playback.intervalMs > 0
                    ? `${playback.intervalMs.toFixed(2)} ms ± ${playback.jitterMs.toFixed(2)}`
                    : "—"}
                </dd>
                <dt className="text-zinc-500">Transport</dt>
                <dd className="text-zinc-200">{pathLabel}</dd>
                <dt className="text-zinc-500">TX loss</dt>
                <dd className="tabular-nums text-zinc-200">
                  {packetLossPct !== undefined ? `${packetLossPct.toFixed(1)}%` : "—"}
                </dd>
              </dl>
              <button
                type="button"
                className="vmax-secondary mt-1 inline-flex w-full items-center justify-center gap-1.5"
                onClick={() => void copyStreamDiagnostics()}
              >
                <ClipboardCopy className="h-3.5 w-3.5" /> Copy diagnostics
              </button>
              {diagnosticsFeedback && (
                <p role="status" className="font-mono text-[10px] text-zinc-400">
                  {diagnosticsFeedback}
                </p>
              )}
              <form className="mt-2 flex flex-wrap items-center gap-2 text-xs" onSubmit={(event) => {
                event.preventDefault();
                void applyBitrateLimit(bitrateLimit);
              }}>
                <label className="flex items-center gap-2">
                  Viewer bitrate limit
                  <input type="number" min={1} max={MAX_STREAM_BITRATE_MBPS} required
                    value={bitrateLimit} onChange={(event) => setBitrateLimit(Number(event.target.value))} />
                  Mbps
                </label>
                <button type="submit" className="vmax-secondary">Apply</button>
                <button type="button" className="vmax-secondary" onClick={() => {
                  setBitrateLimit(MAX_STREAM_BITRATE_MBPS);
                  void applyBitrateLimit(MAX_STREAM_BITRATE_MBPS);
                }}>Use host limit</button>
                {bitrateFeedback && <span role="status">{bitrateFeedback}</span>}
              </form>
              <div className="mt-2 flex flex-wrap items-center justify-between gap-2 text-xs">
                <span>Host controller port: {gamepadPort ?? "—"}</span>
                <button
                  type="button"
                  className="vmax-secondary"
                  disabled={gamepadPort === null}
                  onClick={() => {
                    setGamepadError(null);
                    void invoke("reconnect_stream_gamepad").catch((error) =>
                      setGamepadError(String(error)),
                    );
                  }}
                >
                  Reconnect controller
                </button>
                {gamepadError && <p role="alert">{gamepadError}</p>}
              </div>
            </div>

            {!touchAvailable && (
              <div className="flex flex-col gap-2 p-3">
                <button
                  type="button"
                  onClick={handleTogglePointerLock}
                  title="Toggle Relative FPS Mouse Lock"
                  aria-label="Toggle FPS mouse lock"
                  className={`inline-flex min-h-11 items-center justify-center gap-1.5 rounded-xl border px-3 py-1.5 text-xs font-medium transition-colors cursor-pointer ${
                    pointerLocked
                      ? "border-zinc-300 bg-zinc-200 text-zinc-950"
                      : "border-zinc-800 bg-zinc-900 text-zinc-200 hover:border-zinc-700 hover:bg-zinc-800 hover:text-white"
                  }`}
                >
                  <Crosshair className="h-3.5 w-3.5" />
                  <span>{pointerLocked ? "Locked" : "FPS Lock"}</span>
                </button>

                <button
                  type="button"
                  aria-pressed={recoverStalls}
                  onClick={() => setRecoverStalls((enabled) => !enabled)}
                  className={touchButton}
                >
                  <Gauge className="h-4 w-4" />
                  <span>Stall recovery {recoverStalls ? "ON" : "OFF"}</span>
                </button>

                {fullscreen !== undefined && onToggleFullscreen && (
                  <button type="button" onClick={onToggleFullscreen} className={touchButton}>
                    {fullscreen ? (
                      <Minimize className="h-4 w-4" />
                    ) : (
                      <Maximize className="h-4 w-4" />
                    )}
                    {fullscreen ? "Windowed" : "Fullscreen"}
                  </button>
                )}

                {windowsClient && fullscreen && (
                  <div className="grid gap-2">
                    <button
                      type="button"
                      aria-pressed={keyboardCapture}
                      className={touchButton}
                      onClick={() => void toggleKeyboardCapture()}
                    >
                      <Keyboard className="h-4 w-4" />
                      {keyboardCapture ? "Release keyboard" : "Send system shortcuts to host"}
                    </button>
                    <label className="flex items-center justify-between gap-2 text-xs text-zinc-200">
                      Release shortcut
                      <select
                        aria-label="Keyboard capture release shortcut"
                        className="rounded bg-zinc-900 p-2"
                        disabled={keyboardCapture}
                        value={releaseKey}
                        onChange={(event) => {
                          const key = event.target.value as ReleaseKey;
                          localStorage.setItem("vmax.keyboard-release-key", key);
                          setReleaseKey(key);
                        }}
                      >
                        {(Object.keys(RELEASE_KEYS) as ReleaseKey[]).map((key) => (
                          <option value={key} key={key}>
                            Ctrl + Alt + {RELEASE_KEYS[key]}
                          </option>
                        ))}
                      </select>
                    </label>
                    {captureError && (
                      <div role="alert" className="text-xs text-red-300">
                        {captureError}
                      </div>
                    )}
                  </div>
                )}

                {onOpenFriends && (
                  <button type="button" className={touchButton} onClick={onOpenFriends}>
                    <Users size={16} /> Friends
                  </button>
                )}
                <div className="grid grid-cols-2 gap-2">
                  <button
                    type="button"
                    onClick={onOpenSettings}
                    title="Stream Settings"
                    aria-label="Open settings"
                    className="inline-flex min-h-11 items-center justify-center gap-2 rounded-xl border border-zinc-800 bg-zinc-900 px-3 py-2 text-xs font-medium text-zinc-200 hover:border-zinc-700 hover:bg-zinc-800 hover:text-white transition-colors cursor-pointer"
                  >
                    <Settings className="h-4 w-4 text-zinc-300" />
                    <span>Settings</span>
                  </button>
                  <button
                    type="button"
                    onClick={onDisconnect}
                    title="Disconnect session"
                    aria-label="Disconnect"
                    className="inline-flex min-h-11 items-center justify-center gap-2 rounded-xl border border-zinc-800 bg-zinc-900 px-3 py-2 text-xs font-medium text-zinc-200 hover:border-red-900 hover:bg-zinc-800 transition-colors cursor-pointer"
                  >
                    <LogOut className="h-4 w-4 text-red-500" />
                    <span>Disconnect</span>
                  </button>
                </div>
              </div>
            )}
          </div>
          {(pointerLockError || windowModeError) && (
            <p role="status" className="rounded-lg bg-zinc-950 px-3 py-2 text-xs text-white">
              {pointerLockError || windowModeError}
            </p>
          )}
        </div>
      )}
    </div>
  );
}