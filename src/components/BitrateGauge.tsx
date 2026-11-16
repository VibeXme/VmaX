import { Gauge } from "lucide-react";

interface SpeedometerSvgProps {
  bitrateMbps: number;
  minMbps?: number;
  maxMbps?: number;
  decimals?: number;
  className?: string;
}

export function SpeedometerSvg({
  bitrateMbps,
  minMbps = 5,
  maxMbps = 50,
  decimals = 1,
  className = "w-full max-w-[220px] overflow-visible",
}: SpeedometerSvgProps) {
  const clamped = Math.min(maxMbps, Math.max(minMbps, bitrateMbps));
  const ratio = (clamped - minMbps) / Math.max(1, maxMbps - minMbps);

  const cx = 110;
  const cy = 96;
  const r = 74;
  const startAngle = Math.PI; // left (180 deg)
  const activeAngle = startAngle - ratio * Math.PI;

  const polar = (angle: number, radius = r) => ({
    x: cx + radius * Math.cos(angle),
    y: cy - radius * Math.sin(angle),
  });

  const trackStart = polar(startAngle);
  const trackEnd = polar(0);
  const activeEnd = polar(activeAngle);
  const needleTip = polar(activeAngle, r - 12);

  const trackArc = `M ${trackStart.x.toFixed(1)} ${trackStart.y.toFixed(1)} A ${r} ${r} 0 0 1 ${trackEnd.x.toFixed(1)} ${trackEnd.y.toFixed(1)}`;
  const activeArc = `M ${trackStart.x.toFixed(1)} ${trackStart.y.toFixed(1)} A ${r} ${r} 0 0 1 ${activeEnd.x.toFixed(1)} ${activeEnd.y.toFixed(1)}`;

  const ticks = [0, 0.25, 0.5, 0.75, 1];

  return (
    <svg
      viewBox="0 0 220 112"
      className={className}
      role="img"
      aria-label="Dynamic bitrate speedometer"
    >
      {ticks.map((t) => {
        const angle = Math.PI * (1 - t);
        const p1 = polar(angle, r + 6);
        const p2 = polar(angle, r + 11);
        return (
          <line
            key={t}
            x1={p1.x}
            y1={p1.y}
            x2={p2.x}
            y2={p2.y}
            stroke={t <= ratio ? "#d4d4d8" : "var(--vmax-control-border)"}
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        );
      })}

      <path
        d={trackArc}
        fill="none"
        stroke="var(--vmax-line)"
        strokeWidth="10"
        strokeLinecap="round"
      />
      {ratio > 0.001 && (
        <path
          d={activeArc}
          fill="none"
          stroke="var(--vmax-accent)"
          strokeWidth="10"
          strokeLinecap="round"
        />
      )}

      <line
        x1={cx}
        y1={cy}
        x2={needleTip.x}
        y2={needleTip.y}
        stroke="#a1a1aa"
        strokeWidth="2"
        strokeLinecap="round"
      />
      <circle cx={cx} cy={cy} r="4.5" fill="var(--vmax-accent)" />

      <text
        x={cx}
        y={cy - 18}
        textAnchor="middle"
        className="fill-white text-2xl font-semibold tabular-nums"
      >
        {clamped.toFixed(decimals)}
      </text>
      <text
        x={cx}
        y={cy - 4}
        textAnchor="middle"
        className="fill-zinc-500 text-[10px] uppercase tracking-widest"
      >
        Mbps
      </text>
      <text x={24} y={110} textAnchor="middle" className="fill-zinc-600 text-[10px] font-mono">
        {minMbps}
      </text>
      <text x={196} y={110} textAnchor="middle" className="fill-zinc-600 text-[10px] font-mono">
        {maxMbps}
      </text>
    </svg>
  );
}

interface BitrateGaugeProps {
  bitrateMbps: number;
  minMbps?: number;
  maxMbps?: number;
  congestionLabel: string;
}

export function BitrateGauge({
  bitrateMbps,
  minMbps = 5,
  maxMbps = 50,
  congestionLabel,
}: BitrateGaugeProps) {
  return (
    <div className="rounded-2xl border border-zinc-800 bg-zinc-900/90 p-6 flex flex-col justify-between gap-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-widest text-zinc-500">
          <Gauge className="h-3.5 w-3.5 text-zinc-400" />
          <span>Bitrate</span>
        </div>
        <span className="rounded-full border border-zinc-700 bg-zinc-800 px-2.5 py-0.5 text-[11px] font-medium capitalize text-zinc-300">
          {congestionLabel}
        </span>
      </div>

      <div className="flex flex-col items-center justify-center rounded-xl bg-zinc-950 border border-zinc-800/80 py-3 px-4">
        <SpeedometerSvg
          bitrateMbps={bitrateMbps}
          minMbps={minMbps}
          maxMbps={maxMbps}
        />
      </div>
    </div>
  );
}

