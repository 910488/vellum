/**
 * Vellum 品牌標記 —— 與 app icon 同一個構圖。
 *
 * 目前畫面上沒有渲染它：左上角的「Vellum / LOCAL PROXY」品牌區已移除
 * （常駐在自己機器上的工具，不需要每次抬頭確認它叫什麼）。這個檔案留著，
 * 因為它是 app icon 幾何的另一半事實來源 —— 見下面與 make_icon.py 的對齊註記。
 *
 * 兩筆 V 是寬頭筆掃出的帶狀多邊形（筆尖角度 -35°），所以左筆粗、右筆細。
 * 幾何值與 src-tauri/icons/make_icon.py 對齊；改一邊要改另一邊。
 */

// 筆尖偏移：cos(-35°)·7.3 , sin(-35°)·7.3
const LEFT = "22.82,31.39 34.78,23.01 55.98,67.31 44.02,75.69";
const RIGHT = "44.02,75.69 55.98,67.31 77.18,23.01 65.22,31.39";

export function Mark({ size = 30 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 100 100"
      aria-hidden="true"
      style={{ display: "block", flex: "none", borderRadius: size * 0.222 }}
    >
      <defs>
        <linearGradient id="vellum-ground" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0%" stopColor="#F8F1E3" />
          <stop offset="100%" stopColor="#E5CEAE" />
        </linearGradient>
      </defs>
      <rect width="100" height="100" rx="22.2" fill="url(#vellum-ground)" />
      <g stroke="#C6A27B" strokeLinecap="round" strokeWidth="2">
        <line x1="15.5" y1="30" x2="84.5" y2="30" opacity="0.36" />
        <line x1="15.5" y1="47" x2="84.5" y2="47" opacity="0.42" />
        <line x1="15.5" y1="64" x2="84.5" y2="64" opacity="0.36" />
      </g>
      <polygon points={LEFT} fill="#62509E" />
      <polygon points={RIGHT} fill="#62509E" />
      <rect
        x="0.5"
        y="0.5"
        width="99"
        height="99"
        rx="21.9"
        fill="none"
        stroke="#CEB38F"
        strokeOpacity="0.57"
        strokeWidth="1.1"
      />
    </svg>
  );
}
