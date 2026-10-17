import { api } from "@/lib/api";
import { Confirm } from "@/components/ui";
import type { RemoteController } from './useRemoteController';
export function RemoteConfirmations({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    takeoverPrompt,
    setTakeoverPrompt,
    pending,
    setPending,
    host,
    run,
    runPending,
    confirmConfig,
  } = model;
  return <>{takeoverPrompt ? <Confirm open title={t("remote.act.takeoverNative")} confirmLabel={t("remote.act.takeoverNative")} facts={[
    { key: t("remote.fact.changes"), value: t("remote.confirm.stopAppOwned.changes") },
    { key: t("remote.fact.untouched"), value: t("remote.confirm.stopAppOwned.untouched") },
    { key: t("remote.fact.turns"), value: t("remote.confirm.stopAppOwned.turns") },
  ]} onCancel={() => setTakeoverPrompt(null)} onConfirm={() => {
    const prompt = takeoverPrompt;
    setTakeoverPrompt(null);
    void run("remote.act.takeoverNative", () => api.confirmRemoteDeploymentTakeover(prompt.hostId, prompt.operationId, prompt.token));
  }} /> : null}
    {pending && confirmConfig ? (<Confirm open title={t(confirmConfig.titleKey)} confirmLabel={t(confirmConfig.titleKey)} danger={confirmConfig.danger} gate={confirmConfig.gated && host
      ? { label: t("remote.confirm.gate", { host: host.displayName }), phrase: host.displayName }
      : null} facts={[
        { key: t("remote.fact.changes"), value: t(`remote.confirm.${confirmConfig.factsKey}.changes`) },
        { key: t("remote.fact.untouched"), value: t(`remote.confirm.${confirmConfig.factsKey}.untouched`) },
        { key: t("remote.fact.turns"), value: t(`remote.confirm.${confirmConfig.factsKey}.turns`) },
      ]} onConfirm={() => runPending(pending)} onCancel={() => setPending(null)} />) : null}</>;
}
