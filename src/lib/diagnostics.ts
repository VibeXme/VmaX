/**
 * One-click stream diagnostics.
 *
 * The report is plain text on purpose: it is meant to be pasted back verbatim,
 * with no parsing, and it carries exactly the numbers that separate a network
 * problem from an encoder or presentation problem. Everything here is
 * best-effort — an unknown value prints as an em dash rather than a zero, so a
 * missing number can never be mistaken for a healthy one.
 */

export interface StreamDiagnostics {
  /** Host pipeline label, including which low-latency requests the encoder accepted. */
  pipelineLabel?: string | null;
  codec?: string | null;
  /** Resolution the host was asked for, as configured in settings. */
  requestedResolution?: string | null;
  decodedWidth?: number;
  decodedHeight?: number;
  displayWidth?: number;
  displayHeight?: number;
  fps?: number | null;
  bitrateMbps?: number | null;
  rttMs?: number | null;
  networkMs?: number | null;
  packetLossPct?: number | null;
  decodeMs?: number | null;
  encodeMs?: number | null;
  presentIntervalMs?: number | null;
  presentJitterMs?: number | null;
  supersededFrames?: number | null;
  transport?: string | null;
  /** True when the Android MediaCodec surface feeds the screen instead of WebCodecs. */
  nativePath?: boolean;
  sharpness?: number | null;
  /** True when the client is decoding on the GPU rather than in software. */
  hardwareDecode?: boolean | null;
}

export interface DiagnosticsContext {
  version?: string | null;
  now?: Date;
  userAgent?: string;
  platform?: string;
  devicePixelRatio?: number;
}

const UNKNOWN = "—";

function fixed(value: number | null | undefined, digits: number): string | null {
  return typeof value === "number" && Number.isFinite(value) ? value.toFixed(digits) : null;
}

function ms(value: number | null | undefined, digits = 2): string {
  const text = fixed(value, digits);
  return text === null ? UNKNOWN : `${text} ms`;
}

function bps(value: number | null | undefined): string {
  const text = fixed(value, 2);
  return text === null ? UNKNOWN : `${text} Mbps`;
}

function pct(value: number | null | undefined, digits = 1): string {
  const text = fixed(value, digits);
  return text === null ? UNKNOWN : `${text} %`;
}

function size(width: number | undefined, height: number | undefined): string {
  return typeof width === "number" && typeof height === "number" && width > 0 && height > 0
    ? `${width}x${height}`
    : UNKNOWN;
}

function text(value: string | null | undefined): string {
  const trimmed = value?.trim();
  return trimmed ? trimmed : UNKNOWN;
}

function line(key: string, value: string): string {
  return `  ${key.padEnd(10)} ${value}`;
}

/**
 * Renders the copyable report. Section order follows the order in which a
 * stream breaks down: what was encoded, what crossed the network, and what the
 * client managed to show.
 */
export function buildDiagnosticsReport(
  snapshot: StreamDiagnostics,
  context: DiagnosticsContext = {},
): string {
  const now = context.now ?? new Date();
  const version = context.version?.trim() || UNKNOWN;
  const platform = context.platform?.trim() || UNKNOWN;
  const ua = (context.userAgent ?? "").trim();

  const decoder = snapshot.nativePath
    ? "MediaCodec (native surface)"
    : snapshot.hardwareDecode === false
      ? "WebCodecs (software)"
      : snapshot.hardwareDecode === true
        ? "WebCodecs (hardware)"
        : "WebCodecs";

  const present =
    typeof snapshot.presentIntervalMs === "number" && snapshot.presentIntervalMs > 0
      ? `${ms(snapshot.presentIntervalMs)} ± ${fixed(snapshot.presentJitterMs, 2) ?? UNKNOWN}`
      : UNKNOWN;
  const superseded =
    typeof snapshot.supersededFrames === "number"
      ? ` (${snapshot.supersededFrames} superseded frames)`
      : "";

  const display =
    typeof context.devicePixelRatio === "number"
      ? `${size(snapshot.displayWidth, snapshot.displayHeight)} @ ${context.devicePixelRatio.toFixed(2)}x dpr`
      : size(snapshot.displayWidth, snapshot.displayHeight);

  const lines = [
    "VmaX diagnostics",
    line("time", now.toISOString()),
    line("version", version),
    line("client", platform),
    ua ? line("agent", ua.length > 180 ? `${ua.slice(0, 177)}...` : ua) : null,
    "",
    "[encoder]",
    line("pipeline", text(snapshot.pipelineLabel)),
    line("codec", text(snapshot.codec)),
    line("requested", text(snapshot.requestedResolution)),
    line("decoded", size(snapshot.decodedWidth, snapshot.decodedHeight)),
    line("fps", typeof snapshot.fps === "number" ? String(snapshot.fps) : UNKNOWN),
    line("bitrate", bps(snapshot.bitrateMbps)),
    "",
    "[network]",
    line("path", text(snapshot.transport)),
    line("rtt", ms(snapshot.rttMs)),
    line("one-way", ms(snapshot.networkMs)),
    line("tx loss", pct(snapshot.packetLossPct)),
    "",
    "[client]",
    line("decoder", decoder),
    line("decode", ms(snapshot.decodeMs)),
    line("encode", ms(snapshot.encodeMs)),
    line("present", `${present}${superseded}`),
    line("display", display),
    line("sharpness", typeof snapshot.sharpness === "number" ? String(snapshot.sharpness) : UNKNOWN),
  ].filter((entry): entry is string => entry !== null);

  return `${lines.join("\n")}\n`;
}

/**
 * Copies `report` to the clipboard. Returns false when every path is blocked,
 * so the caller can say so instead of pretending it worked: WebView2 refuses
 * the async clipboard API outside a user gesture and some WebViews do not
 * implement it at all, hence the execCommand fallback.
 */
export async function copyDiagnostics(report: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(report);
    return true;
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const area = document.createElement("textarea");
    area.value = report;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.top = "-1000px";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const copied = document.execCommand("copy");
    document.body.removeChild(area);
    return copied;
  } catch {
    return false;
  }
}
