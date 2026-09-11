/**
 * 上下文用量走勢。面積填色 + 淡格線 + 強調末端點 ——
 * 末端是「現在」，那是唯一需要一眼看到的位置。
 */

export function Sparkline({
  points,
  stroke = "var(--haze)",
  label,
}: {
  points: number[];
  stroke?: string;
  label: string;
}) {
  const W = 200;
  const H = 46;
  const PAD = 4;

  if (points.length < 2) {
    return <svg className="spark" viewBox={`0 0 ${W} ${H}`} aria-label={label} />;
  }

  const max = Math.max(...points, 0.0001);
  const step = W / (points.length - 1);
  const toY = (v: number) => H - PAD - (v / max) * (H - PAD * 2);

  const line = points.map((v, i) => `${i === 0 ? "M" : "L"}${(i * step).toFixed(1)},${toY(v).toFixed(1)}`).join(" ");
  const area = `${line} L${W},${H} L0,${H} Z`;

  const lastX = W;
  const lastY = toY(points[points.length - 1] ?? 0);
  const gradId = `spark-${label.replace(/\W/g, "")}`;

  return (
    <svg
      className="spark"
      viewBox={`0 0 ${W} ${H}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
      style={{ width: "100%", height: 46, display: "block", marginTop: 6 }}
    >
      <defs>
        <linearGradient id={gradId} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor={stroke} stopOpacity="0.42" />
          <stop offset="100%" stopColor={stroke} stopOpacity="0" />
        </linearGradient>
      </defs>
      <path d={area} fill={`url(#${gradId})`} />
      <path
        d={line}
        fill="none"
        stroke={stroke}
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
        vectorEffect="non-scaling-stroke"
      />
      <circle cx={lastX} cy={lastY} r="3" fill={stroke} />
      <circle cx={lastX} cy={lastY} r="6" fill={stroke} opacity="0.22" />
    </svg>
  );
}
