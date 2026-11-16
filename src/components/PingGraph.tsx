import { useId } from "react";
import { Activity } from "lucide-react";

interface PingGraphProps {
  samples: number[];
  currentMs: number;
  minMs: number;
}

export function PingGraph({ samples, currentMs, minMs }: PingGraphProps) {
  const gradId = useId();
  const width = 360;
  const height = 110;
  const padX = 4;
  const padY = 10;

  const series = samples.length >= 2 ? samples : [currentMs, currentMs];
  const maxVal = Math.max(20, ...series) * 1.15;
  const minVal = 0;

  const points = series.map((val, idx) => {
    const x = padX + (idx / Math.max(1, series.length - 1)) * (width - padX * 2);
    const ratio = (val - minVal) / Math.max(1, maxVal - minVal);
    const y = height - padY - ratio * (height - padY * 2);
    return { x, y };
  });

  const linePath = points
    .map((pt, i) => `${i === 0 ? "M" : "L"}${pt.x.toFixed(1)},${pt.y.toFixed(1)}`)
    .join(" ");

  const first = points[0];
  const last = points[points.length - 1];
  const areaPath = `${linePath} L${last.x.toFixed(1)},${(height - padY).toFixed(1)} L${first.x.toFixed(1)},${(height - padY).toFixed(1)} Z`;

  const jitter = Math.max(0, currentMs - minMs);

  return (
    <div className="rounded-2xl border border-zinc-800 bg-zinc-900/90 p-6 flex flex-col justify-between gap-4">
      <div className="flex items-start justify-between">
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
          <Activity className="h-3.5 w-3.5 text-zinc-400" />
          <span>Ping</span>
        </div>
        <div className="text-right">
          <div className="text-3xl font-semibold tracking-tight text-white tabular-nums">
            {currentMs.toFixed(1)}
            <span className="ml-1 text-xs font-normal text-zinc-500">ms</span>
          </div>
          <div className="text-[11px] font-mono text-zinc-500 tabular-nums">
            min {minMs.toFixed(1)} · Δ {jitter.toFixed(1)} ms
          </div>
        </div>
      </div>

      <div className="rounded-xl bg-zinc-950 border border-zinc-800/80 p-3">
        <svg
          viewBox={`0 0 ${width} ${height}`}
          className="w-full h-24 overflow-visible"
          role="img"
          aria-label="Live ping telemetry sparkline"
        >
          <defs>
            <linearGradient id={gradId} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--vmax-accent)" stopOpacity="0.22" />
              <stop offset="100%" stopColor="var(--vmax-accent)" stopOpacity="0.0" />
            </linearGradient>
          </defs>

          {[0.25, 0.5, 0.75].map((pct) => {
            const y = padY + pct * (height - padY * 2);
            return (
              <line
                key={pct}
                x1={padX}
                y1={y}
                x2={width - padX}
                y2={y}
                stroke="var(--vmax-line)"
                strokeWidth="1"
                strokeDasharray="3 4"
              />
            );
          })}

          <path d={areaPath} fill={`url(#${gradId})`} />
          <path
            d={linePath}
            fill="none"
            stroke="var(--vmax-accent)"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
          <circle cx={last.x} cy={last.y} r="3.5" fill="var(--vmax-accent)" />
          <circle cx={last.x} cy={last.y} r="7" fill="var(--vmax-accent)" fillOpacity="0.18" />
        </svg>
      </div>
    </div>
  );
}

