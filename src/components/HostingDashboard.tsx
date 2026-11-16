import { useState } from "react";
import { Check, Copy, Globe, Radio, Square, Users, Wifi } from "lucide-react";
import { QRCodeSVG } from "qrcode.react";
import { connectionPathLabel, type ConnectionPhase, type HardwareTelemetry, type NetworkTelemetry, type PacketLogEntry, type VmaXConfig } from "../types";
import { HardwareStats } from "./HardwareStats";
import { PingGraph } from "./PingGraph";

interface HostingDashboardProps {
  telemetry: NetworkTelemetry;
  hardware: HardwareTelemetry;
  pingSamples: number[];
  config: VmaXConfig;
  logs: PacketLogEntry[];
  busy: boolean;
  onStopHosting: () => void;
  onOpenFriends?: () => void;
  hostOnlyInput: boolean;

  onToggleHostOnly: () => Promise<void>;
  guestPermissions: GuestPermissions;
  onGuestPermissions: (permissions: GuestPermissions) => Promise<void>;
  switchingToApprovedHosting: boolean;
}

export interface GuestPermissions {
  keyboard: boolean;
  mouse: boolean;
  controller: boolean;
}

const PHASE_LABEL: Record<ConnectionPhase, string> = {
  idle: "Starting",
  discovering_nat: "Starting",
  waiting_for_peer: "Awaiting Client",
  hole_punching: "Connecting",
  quic_handshake: "Connecting",
  connected: "Live",
  disconnected: "Ended",
  failed: "Couldn’t connect",
};

export function HostingDashboard({ telemetry, hardware, pingSamples, config, logs, busy, onStopHosting, onOpenFriends, switchingToApprovedHosting, hostOnlyInput, onToggleHostOnly, guestPermissions, onGuestPermissions }: HostingDashboardProps) {
  const [copyState, setCopyState] = useState<"link" | "code" | "failed" | null>(null);
  const connected = telemetry.phase === "connected";
  const [inputError, setInputError] = useState("");
  const [updatingInput, setUpdatingInput] = useState(false);
  const invite = telemetry.phase === "waiting_for_peer" && !telemetry.saved_session ? telemetry.invite_uri : null;
  const internet = new URLSearchParams(invite?.split("?")[1]).get("sig")?.startsWith("wss://");

  async function copy(value: string, kind: "link" | "code") {
    try {
      await navigator.clipboard.writeText(value);
      setCopyState(kind);
    } catch {
      setCopyState("failed");
    }
  }

  return (
    <main className="vmax-lobby">
      <section className="vmax-panel vmax-host-panel">
        <div className="vmax-session-head">
          <div className="vmax-intro">
            <div className="vmax-symbol"><Radio size={21} /></div>
            <div><h1>Hosting</h1><p role="status">{PHASE_LABEL[telemetry.phase]}</p></div>
          </div>
          <div className="vmax-chip-row">
            {onOpenFriends && <button type="button" className="vmax-secondary" onClick={onOpenFriends}><Users size={16} /> Friends</button>}
            <button type="button" className="vmax-secondary" disabled={busy} onClick={onStopHosting}><Square size={15} /> {switchingToApprovedHosting ? "Switch to friends-only hosting" : "Stop Hosting"}</button>
          </div>
        </div>

        {telemetry.last_error && <p className="vmax-error" role="alert">{telemetry.last_error}</p>}
        {inputError && <p className="vmax-error" role="alert">{inputError}</p>}
        {connected && (
          <fieldset className="vmax-form" disabled={updatingInput}>
            <legend>Guest input</legend>
            <div className="vmax-chip-row">
              {(["keyboard", "mouse", "controller"] as const).map((device) => (
                <label key={device} className="vmax-secondary capitalize">
                  <input type="checkbox" checked={guestPermissions[device]} disabled={hostOnlyInput} onChange={(event) => {
                    setInputError("");
                    setUpdatingInput(true);
                    void onGuestPermissions({ ...guestPermissions, [device]: event.target.checked })
                      .catch((err) => setInputError(String(err))).finally(() => setUpdatingInput(false));
                  }} /> {device}
                </label>
              ))}
            </div>
          <button type="button" className="vmax-secondary" aria-pressed={hostOnlyInput}
            onClick={() => {
              setInputError("");
              void onToggleHostOnly().catch((err) => setInputError(String(err)));
            }}>
            {hostOnlyInput ? "Restore guest permissions" : "Take exclusive input"}
          </button>
          </fieldset>
        )}

        {invite && (
          <div className="vmax-invite">
            <div className="vmax-qr"><QRCodeSVG value={invite} size={112} level="M" /></div>
            <div className="vmax-form">
              <label htmlFor="vmax-share-link">{internet ? <Globe size={15} /> : <Wifi size={15} />} {internet ? "Local and internet invite" : "Local only · internet relay unavailable"}</label>
              <input id="vmax-share-link" readOnly value={invite} onFocus={(event) => event.currentTarget.select()} />
              <button type="button" className="vmax-primary" aria-label="Copy full invite link" onClick={() => void copy(invite, "link")}>
                {copyState === "link" ? <Check size={16} /> : <Copy size={16} />}
                {copyState === "link" ? "Copied" : "Copy full invite link"}
              </button>
              {!internet && telemetry.room_code && <button type="button" className="vmax-secondary" aria-label="Copy room code" onClick={() => void copy(telemetry.room_code!, "code")}>
                {copyState === "code" ? "Code copied" : telemetry.room_code}
              </button>}
              {copyState === "failed" && <p className="vmax-status" role="status">Select the link and copy it manually.</p>}
            </div>
          </div>
        )}

        {telemetry.saved_session && !connected && <p className="vmax-status">Waiting for an approved device.</p>}
        {connected && (
          <dl className="vmax-metrics">
            <div><dt>Connection</dt><dd>{connectionPathLabel(telemetry.connection_path, telemetry.relayed)}</dd></div>
            <div><dt>Ping</dt><dd>{telemetry.rtt_ms.toFixed(1)} ms</dd></div>
            <div><dt>Video</dt><dd>{hardware.fps} FPS</dd></div>
          </dl>
        )}

        <details className="vmax-disclosure">
          <summary>Stream details</summary>
          <div className="vmax-form">
            <dl className="vmax-metrics">
              <div><dt>Target</dt><dd>{(connected ? telemetry.target_bitrate_bps / 1_000_000 : config.video.bitrateMbps).toFixed(1)} Mbps</dd></div>
              <div><dt>Packet loss</dt><dd>{telemetry.packet_loss_pct.toFixed(1)}%</dd></div>
              <div><dt>Encoder</dt><dd>{hardware.encodeLatencyMs.toFixed(1)} ms</dd></div>
            </dl>
            <PingGraph samples={pingSamples} currentMs={telemetry.rtt_ms} minMs={telemetry.min_rtt_ms} />
            <HardwareStats stats={hardware} rnnoiseEnabled={config.audio.aiNoiseSuppression} />
            {logs.length > 0 && <ul className="vmax-logs">{logs.map((entry, index) => <li key={entry.ts_ms + ":" + index}>{entry.channel} · {entry.summary}</li>)}</ul>}
          </div>
        </details>
      </section>
    </main>
  );
}

