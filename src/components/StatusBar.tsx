/**
 * Always-on status bar.
 */
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { noticeText } from "@/lib/notice";
import { headline, type SystemStatus } from "@/lib/status";
import { Btn, Pill } from "@/components/ui";

export function StatusBar({
  status,
  refreshing,
  refreshError,
  onRefresh,
}: {
  status: SystemStatus;
  refreshing: boolean;
  refreshError: string | null;
  onRefresh: () => void;
}) {
  const { t } = useTranslation();
  const h = headline(status);
  const [runtimeOpen, setRuntimeOpen] = useState(false);
  const runtimeRef = useRef<HTMLDivElement>(null);
  const enhanced = status.enhancedRuntime;
  const proxyRunning = status.proxy?.running ?? false;
  /* 按鈕講的是「Codex Desktop 現在有沒有走 Enhanced」，不是「設定是不是最新
     的那一份」。一座上一次啟動留下來的 bridge 仍然在服務每一輪，把它說成
     未載入是假的——ready 另外還要求它跑的是這次啟動的設定，那是別的事。
     未驗證用 warn，因為那正是「可用但不保證正確性」:不是故障，也不是沒事。 */
  const loaded = Boolean(enhanced?.serving);
  const enhancedTone = loaded
    ? enhanced?.unverified || !enhanced?.ready
      ? "warn"
      : "ok"
    : proxyRunning && enhanced?.enabled
      ? "warn"
      : "quiet";
  const protocol = enhanced?.protocol ?? null;
  /* 回退只有一個意思：本來要接管，被協定判定擋下來了。
     不能寫成「enabled 但還沒 ready」——「接管好了，等 Codex 下次啟動」也符合
     那個條件，於是同一張浮層會一邊說啟動路徑已接管、一邊說已回到原生 Codex。 */
  const fellBack =
    Boolean(enhanced?.enabled) && proxyRunning && protocol?.verdict === "incompatible";
  const details = [
    ...(protocol?.deltas ?? []).map((delta) => ({
      key: `${delta.kind}:${delta.subject}:${delta.fields.join(",")}`,
      routed: delta.routed,
      text: t(`settings.page.enhancedRuntime.protocol.delta.${delta.kind}`, {
        subject: delta.subject,
        fields: delta.fields.join(", "),
      }),
    })),
    ...(enhanced?.blockers ?? []).map((blocker) => ({
      key: `blocker:${blocker}`,
      routed: false,
      text: blocker,
    })),
  ];

  useEffect(() => {
    if (!runtimeOpen) return;
    const close = (event: PointerEvent) => {
      if (!runtimeRef.current?.contains(event.target as Node)) setRuntimeOpen(false);
    };
    window.addEventListener("pointerdown", close);
    return () => window.removeEventListener("pointerdown", close);
  }, [runtimeOpen]);

  return (
    <header className="statusbar">
      <div className="runtime-indicator" ref={runtimeRef}>
        <button
          type="button"
          className={`runtime-indicator__button runtime-indicator__button--${enhancedTone}`}
          aria-expanded={runtimeOpen}
          onClick={() => setRuntimeOpen((open) => !open)}
        >
          <i className="pill__dot" />
          Enhanced {loaded
            ? enhanced?.unverified
              ? t("status.enhancedUnverified")
              : t("status.enhancedLoaded")
            : t("status.enhancedNotLoaded")}
        </button>
        {runtimeOpen ? (
          <section className="runtime-popover" aria-label={t("settings.page.enhancedRuntime.title")}>
            <strong>{t("settings.page.enhancedRuntime.title")}</strong>
            {/* 服務中但不是這次啟動的設定，要講成「跑的是上一份設定」，
                不能沿用 armed 那句——那句說的是 Desktop 還在原生 Codex 上，
                而它此刻正走在 Enhanced 上。 */}
            <p>{t(`settings.page.enhancedRuntime.inject.mean.${enhanced?.ready ? "on" : loaded ? "staleLaunch" : proxyRunning && enhanced?.environmentState === "leased" ? "armed" : proxyRunning && enhanced?.enabled ? "wanted" : "off"}`)}</p>
            {/* 常駐的只有三行狀態。PID、digest、差異、blocker 都是診斷，
                一次全攤開會把浮層撐破，而且沒有人是為了讀那些才點開它的。 */}
            <dl>
              <dt>{t("settings.page.enhancedRuntime.activationLabel")}</dt>
              <dd>{t(`settings.page.enhancedRuntime.activation.${enhanced?.activationState ?? "disabled"}`)}</dd>
              <dt>{t("settings.page.enhancedRuntime.environmentLabel")}</dt>
              <dd>{t(`settings.page.enhancedRuntime.environment.${enhanced?.environmentState ?? "released"}`)}</dd>
              <dt>{t("settings.page.enhancedRuntime.protocol.label")}</dt>
              <dd>
                {protocol
                  ? t(`settings.page.enhancedRuntime.protocol.verdict.${protocol.verdict}`)
                  : t("settings.page.enhancedRuntime.protocol.unavailable")}
              </dd>
            </dl>

            {fellBack ? <p>{t("settings.page.enhancedRuntime.protocol.fallback")}</p> : null}

            {details.length ? (
              <details className="runtime-popover__tray">
                <summary>
                  {t("settings.page.enhancedRuntime.protocol.details", { count: details.length })}
                </summary>
                {/* 判定與差異是可以讀的句子；schema 雜湊與 runtime digest 不是。
                    它們在這裡也不能拿來比對——兩串十二個十六進位字元誰都認不出
                    來，只會把托盤撐出一條左右捲軸。要對雜湊的人看的是設定頁。 */}
                {protocol ? (
                  <p>{t(`settings.page.enhancedRuntime.protocol.mean.${protocol.verdict}`)}</p>
                ) : null}
                <ul>
                  {details.map((detail) => (
                    <li
                      key={detail.key}
                      className={detail.routed ? "runtime-popover__routed" : undefined}
                    >
                      {detail.routed
                        ? `${t("settings.page.enhancedRuntime.protocol.routed")} · `
                        : null}
                      {detail.text}
                    </li>
                  ))}
                </ul>
                <dl>
                  <dt>{t("settings.page.enhancedRuntime.observedBridge")}</dt>
                  <dd>{enhanced?.bridgePid ? `${enhanced.bridgePid} · ${enhanced.bridgeState ?? "—"}` : "—"}</dd>
                  <dt>Official child</dt>
                  <dd>{enhanced?.officialChildPid ?? "—"}</dd>
                  <dt>Enhanced child</dt>
                  <dd>{enhanced?.enhancedChildPid ?? "—"}</dd>
                </dl>
              </details>
            ) : null}
          </section>
        ) : null}
      </div>
      <span className={`statusbar__state statusbar__state--${h.tone}`}>
        <i className="pill__dot" />
        {t(h.labelKey)}
      </span>

      {h.model ? (
        <span className="statusbar__model">
          <span className="statusbar__muted">
            {h.routeSource === "telemetry"
              ? t("status.lastSuccessfulRequest")
              : t("status.defaultRoute")}
          </span>
          <b className={h.modelIsLive ? undefined : "statusbar__model--stale"}>{h.model}</b>
          {h.provider ? <span className="statusbar__sep">{h.provider}</span> : null}
        </span>
      ) : (
        <span className="statusbar__muted">{t("status.noProviderYet")}</span>
      )}

      {h.endpoint ? (
        <span className="statusbar__muted statusbar__endpoint">
          {h.endpoint.replace(/^https?:\/\//, "")}
        </span>
      ) : null}

      <span className="statusbar__spacer" />

      {h.quotaRemaining !== null ? (
        <span className="statusbar__muted">
          {t("status.quotaRemaining", { percent: h.quotaRemaining })}
        </span>
      ) : null}

      {/* 待重啟是尚未套用的變更，不是故障。只在右側用琥珀色提醒一次；左側
          繼續陳述 Proxy 的真實狀態，避免同一句警告出現兩次。 */}
      {h.restartRequired ? <Pill tone="warn" dot>{t("status.restartCodexRequired")}</Pill> : null}
      {h.updateAttention === "available" ? <Pill tone="warn" dot>{t("status.updateAvailable")}</Pill> : null}
      {h.updateAttention === "waitingIdle" ? <Pill tone="warn" dot>{t("status.updateWaitingIdle")}</Pill> : null}
      {h.updateAttention === "waitingRestart" ? <Pill tone="warn" dot>{t("status.updateWaitingRestart")}</Pill> : null}
      {h.updateAttention === "failed" ? <Pill tone="warn" dot>{t("status.updateFailed")}</Pill> : null}

      {/* 一般通報也維持警告色；只有真正的 provider/proxy 故障才使用紅色，
          讓錯誤與「需要稍後處理」保有清楚的嚴重度差異。 */}
      {h.notice ? <Pill tone="warn" dot>{noticeText(h.notice, t)}</Pill> : null}

      {h.error ? <Pill tone="crit" dot>{h.error}</Pill> : null}
      {!h.error && refreshError ? <Pill tone="warn" dot>{refreshError}</Pill> : null}

      <Btn soft onClick={onRefresh} disabled={refreshing}>
        {refreshing ? t("common.refreshing") : t("common.refresh")}
      </Btn>
    </header>
  );
}
