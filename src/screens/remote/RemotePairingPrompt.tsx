import { Btn, Notice } from "@/components/ui";
import { convergePairingSuggested } from "@/lib/remoteVocabulary";
import type { RemoteController } from './useRemoteController';
/**
 * 配對的提示與配對碼，放在工作面正下方。
 *
 * 以前配對碼只出現在帳號卡裡，而帳號卡排在交接、模型之後：按了上面的
 * 「配對」，瀏覽器開了，要輸入的那串碼卻在要往下捲才看得到的地方；帳號卡
 * 沒出現時（還沒勾必選模型）甚至完全看不到。按鈕在哪裡，碼就在哪裡。
 */
export function RemotePairingPrompt({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    selectedHostId,
    modelObservation,
    busy,
    chatgptLogin,
    pairings,
    pairQueueLength,
    pairChatGpt,
    pairAllChatGptAccounts,
    skipPairing,
    workRemedy,
    ownOperation,
  } = model;
  if (chatgptLogin) {
    return <Notice tone="info" acts={<Btn mini soft onClick={skipPairing} disabled={busy !== null}>{t("remote.act.chatgptPairSkip")}</Btn>}>
      {t("remote.account.pairingHint")}
      {" "}
      <a href={chatgptLogin.verificationUrl} target="_blank" rel="noreferrer">{chatgptLogin.verificationUrl}</a>
      {" · "}
      <strong>{chatgptLogin.userCode}</strong>
      {pairQueueLength ? <> · {t("remote.account.pairingRemaining", { remaining: pairQueueLength })}</> : null}
    </Notice>;
  }
  // 上面已經有一顆配對按鈕（工作面的補救、部署後的建議）就不再重複一顆。
  if (workRemedy === "pairChatGpt") return null;
  if (ownOperation?.state === "completed" && convergePairingSuggested(ownOperation.result)) return null;
  // 跟帳號卡同一個條件：這台用得到 ChatGPT 模型時，配對才是待辦事項。
  const usesChatGpt = modelObservation?.hostId === selectedHostId
    && modelObservation.models.some((row) => row.mandatory && row.selected);
  if (!usesChatGpt) return null;
  const pending = pairings.filter((row) => ((!row.paired && !row.active) || row.credentialState === "expired") && !row.detail);
  if (!pending.length) return null;
  const only = pending.length === 1 ? pending[0] : null;
  return <Notice acts={<Btn mini soft onClick={only ? () => pairChatGpt(only.accountId, false) : pairAllChatGptAccounts} disabled={busy !== null}>
    {only ? t("remote.act.chatgptAuthorize") : t("remote.act.chatgptPairAll")}
  </Btn>}>
    {only
      ? t("remote.account.pendingPairingOne", { who: only.email ?? only.accountId })
      : t("remote.account.pendingPairing", { count: pending.length })}
  </Notice>;
}
