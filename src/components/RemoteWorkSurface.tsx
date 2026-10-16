import { useTranslation } from "react-i18next";
import { Btn, Meter } from "./ui";
import type { WorkRemedy, WorkSurfaceModel } from "@/lib/remoteWorkSurface";
import "@/styles/remote-work-surface.css";

const REMEDY_LABELS: Record<WorkRemedy, string> = {
  stopAppOwned: "remote.act.stopAppOwned", pairChatGpt: "remote.act.chatgptPair",
  activateChatGpt: "remote.act.chatgptActivate", repair: "remote.act.repair",
  installCodex: "remote.act.installCodex", updateAgent: "remote.act.updateAgent",
  bootstrap: "remoteWork.redeploy", plan: "remote.act.plan", retry: "remote.act.retry",
  inspect: "remoteWork.inspect", syncDesktopCodex: "remote.act.syncDesktopCodex",
};

/** The work surface has one sentence, one primary action, progress and one remedy. */
export function RemoteWorkSurface({ model, disabled, remedyDisabled, onDeploy, onRemedy, elapsed }: {
  model: WorkSurfaceModel;
  disabled: boolean;
  remedyDisabled: boolean;
  onDeploy: () => void;
  onRemedy: (action: WorkRemedy) => void;
  elapsed: string | undefined;
}) {
  const { t } = useTranslation();
  const remedy = model.remedy;
  return <section className={`remote-work remote-work--${model.state}`} aria-label={t("remoteWork.label")}>
    <div className="remote-work__line">
      <p className="remote-work__sentence" role="status">{t(model.statusKey)}</p>
      <Btn disabled={disabled || model.primaryDisabled} onClick={onDeploy}>{t(model.primaryKey)}</Btn>
    </div>
    {model.progress ? <div className="remote-work__progress" aria-label={t("remoteWork.progress")}>
      <div className="remote-work__phase"><span>{model.progress.kindKey ? t(model.progress.kindKey) : model.progress.kind}</span><span>{model.progress.percent}%{elapsed ? ` · ${elapsed}` : ""}</span></div>
      <Meter percent={model.progress.percent} />
      {model.progress.phaseKey !== model.statusKey ? <p>{model.progress.phaseKey ? t(model.progress.phaseKey) : model.progress.phase}</p> : null}
    </div> : null}
    {remedy ? <div className="remote-work__remedy">
      <p>{t(remedy.messageKey)}</p>
      {remedy.repair ? <code>{remedy.repair}</code> : null}
      {remedy.action === "inspect"
        ? <p>{t("remoteWork.inspect")}</p>
        : <Btn soft mini disabled={remedyDisabled} onClick={() => onRemedy(remedy.action)}>
          {t("remoteWork.nextAction", { action: t(REMEDY_LABELS[remedy.action]) })}
        </Btn>}
    </div> : null}
  </section>;
}
