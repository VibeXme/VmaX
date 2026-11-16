import { Cpu, Monitor } from "lucide-react";
import type { HardwareTelemetry } from "../types";

interface HardwareStatsProps {
  stats: HardwareTelemetry;
  rnnoiseEnabled: boolean;
}

export function HardwareStats({ stats, rnnoiseEnabled }: HardwareStatsProps) {
  return (
    <div className="rounded-2xl border border-zinc-800 bg-zinc-900/90 p-4 space-y-3">
      <p className="flex items-center gap-2 text-sm text-zinc-200">
        <Monitor size={16} /> {stats.pipelineLabel ?? "Encoder unavailable"}
      </p>
      <dl className="grid grid-cols-3 gap-3 text-xs">
        <div>
          <dt className="text-zinc-500">Encode</dt>
          <dd className="tabular-nums font-semibold text-white">
            {stats.encodeLatencyMs > 0 ? `${stats.encodeLatencyMs.toFixed(2)} ms` : "—"}
          </dd>
        </div>
        <div>
          <dt className="text-zinc-500">Decode</dt>
          <dd className="tabular-nums font-semibold text-white">
            {stats.decodeLatencyMs > 0 ? `${stats.decodeLatencyMs.toFixed(2)} ms` : "—"}
          </dd>
        </div>
        <div>
          <dt className="text-zinc-500 inline-flex items-center gap-1">
            <Cpu size={13} /> System CPU
          </dt>
          <dd className="tabular-nums">{stats.cpuLoadPct.toFixed(0)}%</dd>
        </div>
      </dl>
      <p className="text-xs text-zinc-500">
        Audio filter {rnnoiseEnabled ? "enabled in settings" : "off"}
      </p>
    </div>
  );
}