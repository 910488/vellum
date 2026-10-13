import { useTranslation } from "react-i18next";
import type { VellumUpdateState } from "@/components/VellumUpdate";

/** One fixed slot; progress uses the same combined byte count as the panel. */
export function UpdateIndicator({ state, onClick }: { state: VellumUpdateState; onClick: () => void }) {
  const { t } = useTranslation();
  const visible = !["current", "checking", "unsigned"].includes(state.kind);
  const percent = state.kind === "downloading" ? Math.max(0, Math.min(100, state.percent)) : 0;
  const restart = state.kind === "ready" || state.kind === "scheduled";
  const label = state.kind === "available" ? t("status.updateAvailable")
    : restart ? t("status.updateWaitingRestart")
    : state.kind === "failed" ? t("status.updateFailed")
    : t(`settings.page.updates.simple.state.${state.kind}`, {
      percent: Math.round(percent), version: "version" in state ? state.version : "",
    });
  return <span className="update-indicator-slot">
    {visible ? <button key={state.kind} type="button" className={`update-indicator update-indicator--${state.kind}`}
      onClick={onClick} title={label} aria-label={label}>
      <svg viewBox="0 0 32 32" fill="none" stroke="currentColor" strokeWidth="1.5"
        strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        {state.kind === "downloading" ? <>
          <circle className="update-indicator__track" cx="16" cy="16" r="14" />
          <circle className="update-indicator__progress" cx="16" cy="16" r="14" pathLength="100"
            strokeDasharray="100" strokeDashoffset={100 - percent} />
        </> : null}
        {restart || state.kind === "installing" ? <><path d="M23 13a8 8 0 1 0 0 7" /><path d="M23 7v6h-6" /></>
          : state.kind === "failed" ? <><path d="M16 8v10" /><circle cx="16" cy="23" r=".5" /></>
          : state.kind === "updated" ? <path d="m9 16 5 5 10-11" />
          : state.kind === "downloading"
            /* Inside the ring the full-size glyph's tray touches the circle;
               a smaller arrow over a plain baseline keeps clear space. */
            ? <><path d="M16 9.5v10m-3.5-3.5 3.5 3.5 3.5-3.5" /><path d="M11 22.5h10" /></>
            : <><path d="M16 8v13m-5-5 5 5 5-5M8 23v3h16v-3" /></>}
      </svg>
    </button> : null}
  </span>;
}
