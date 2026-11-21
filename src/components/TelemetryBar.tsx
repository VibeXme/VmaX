import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

/**
 * One `vmax://telemetry-tick` payload, emitted by the client at 5 Hz.
 * Mirrors `vmax::telemetry::TelemetryTick` (serde camelCase).
 */
export interface TelemetryTick {
  decodeMs: number;
  encodeMs: number;
  networkMs: number;
  bitrateMbps: number;
  hardware: boolean;
  codec: string;
  width: number;
  height: number;
  chroma: string;
  range: string;
  audio: string;
  idle: boolean;
}

function formatMs(value: number): string {
  return `${value.toFixed(2)}ms`;
}

/**
 * Parsec-style top telemetry line:
 * `Decode 5.50ms  Encode 2.45ms  Network 12.00ms  Bitrate 25.00Mbps •
 *  Hardware H.265 1920x1080 4:2:0 Full Range (Opus Audio)`
 *
 * The host does the smoothing (30-frame EMA, alpha 0.15) so the text here is a
 * straight render: no client-side filtering, no layout jitter beyond the
 * two-decimal numbers themselves.
 */
export function TelemetryBar({ visible }: { visible: boolean }) {
  const [tick, setTick] = useState<TelemetryTick | null>(null);

  useEffect(() => {
    let disposed = false;
    const unlisten = listen<TelemetryTick>("vmax://telemetry-tick", (event) => {
      if (!disposed) setTick(event.payload);
    });
    return () => {
      disposed = true;
      void unlisten.then((off) => off()).catch(() => {});
    };
  }, []);

  if (!visible || !tick) return null;

  const device = tick.hardware ? "Hardware" : "Software";
  const resolution = tick.width > 0 ? `${tick.width}x${tick.height}` : "—";

  return (
    <div className="telemetry-bar" role="status" aria-live="off" data-idle={tick.idle} data-native-video-overlay>
      <span className="telemetry-bar-latency">
        Decode {formatMs(tick.decodeMs)}
      </span>
      <span className="telemetry-bar-latency">
        Encode {formatMs(tick.encodeMs)}
      </span>
      <span className="telemetry-bar-latency">
        Network {formatMs(tick.networkMs)}
      </span>
      <span className="telemetry-bar-latency">
        Bitrate {tick.bitrateMbps.toFixed(2)}Mbps
      </span>
      <span className="telemetry-bar-sep" aria-hidden="true">
        •
      </span>
      <span className="telemetry-bar-detail">
        {device} {tick.codec} {resolution} {tick.chroma} {tick.range}
      </span>
      <span className="telemetry-bar-detail">({tick.audio})</span>
      {tick.idle && <span className="telemetry-bar-idle">Idle</span>}
    </div>
  );
}

export default TelemetryBar;
