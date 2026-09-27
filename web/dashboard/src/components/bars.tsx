import { useState } from "react";
import { formatCount } from "@/format";

export interface Bar {
  label: string;
  value: number;
  detail?: string;
}

/// Single-series bar chart: thin marks, 4px rounded data-ends anchored to
/// the baseline, 2px surface gaps, recessive grid, per-mark hover tooltip.
/// One series → the title names it, no legend.
export function Bars(props: {
  title: string;
  bars: Bar[];
  format?: (v: number) => string;
}) {
  const [hover, setHover] = useState<number | null>(null);
  const fmt = props.format ?? formatCount;
  const { bars } = props;
  const w = 640;
  const h = 180;
  const pad = { l: 6, r: 6, t: 10, b: 22 };
  const max = Math.max(1, ...bars.map((b) => b.value));
  const innerW = w - pad.l - pad.r;
  const innerH = h - pad.t - pad.b;
  const step = innerW / Math.max(1, bars.length);
  const barW = Math.max(4, Math.min(40, step - 2)); // 2px surface gap

  return (
    <div className="rounded-lg border border-borderline bg-surface-1 p-4">
      <div className="mb-2 text-sm font-medium text-ink">{props.title}</div>
      <div className="relative">
        <svg
          viewBox={`0 0 ${w} ${h}`}
          className="w-full"
          role="img"
          aria-label={props.title}
        >
          {/* recessive gridlines at 0 / 50 / 100% */}
          {[0, 0.5, 1].map((f) => (
            <line
              key={f}
              x1={pad.l}
              x2={w - pad.r}
              y1={pad.t + innerH * (1 - f)}
              y2={pad.t + innerH * (1 - f)}
              stroke="var(--border)"
              strokeWidth="1"
            />
          ))}
          {bars.map((b, i) => {
            const bh = Math.max(b.value > 0 ? 2 : 0, (b.value / max) * innerH);
            const x = pad.l + i * step + (step - barW) / 2;
            const y = pad.t + innerH - bh;
            return (
              <g key={i}>
                {/* hit target wider than the mark */}
                <rect
                  x={pad.l + i * step}
                  y={pad.t}
                  width={step}
                  height={innerH}
                  fill="transparent"
                  onMouseEnter={() => setHover(i)}
                  onMouseLeave={() => setHover(null)}
                />
                <path
                  d={roundedTopBar(x, y, barW, bh, 4)}
                  fill="var(--series-1)"
                  opacity={hover === null || hover === i ? 1 : 0.45}
                  pointerEvents="none"
                />
              </g>
            );
          })}
          {/* x labels: first, middle, last only — recessive */}
          {bars.length > 0 &&
            [0, Math.floor((bars.length - 1) / 2), bars.length - 1]
              .filter((v, i, a) => a.indexOf(v) === i)
              .map((i) => (
                <text
                  key={i}
                  x={pad.l + i * step + step / 2}
                  y={h - 6}
                  textAnchor="middle"
                  fontSize="10"
                  fill="var(--text-muted)"
                >
                  {bars[i].label}
                </text>
              ))}
        </svg>
        {hover !== null && bars[hover] && (
          <div
            className="pointer-events-none absolute rounded-md border border-borderline bg-surface-1 px-2.5 py-1.5 text-xs shadow-[var(--shadow-1)]"
            style={{
              left: `${((pad.l + hover * step + step / 2) / w) * 100}%`,
              top: 0,
              transform: "translateX(-50%)",
            }}
            data-testid="chart-tooltip"
          >
            <div className="font-medium text-ink">{bars[hover].label}</div>
            <div className="text-ink-2">
              {fmt(bars[hover].value)}
              {bars[hover].detail ? ` · ${bars[hover].detail}` : ""}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function roundedTopBar(
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
): string {
  if (h <= 0) return "";
  const rr = Math.min(r, w / 2, h);
  return [
    `M ${x} ${y + h}`,
    `L ${x} ${y + rr}`,
    `Q ${x} ${y} ${x + rr} ${y}`,
    `L ${x + w - rr} ${y}`,
    `Q ${x + w} ${y} ${x + w} ${y + rr}`,
    `L ${x + w} ${y + h}`,
    "Z",
  ].join(" ");
}
