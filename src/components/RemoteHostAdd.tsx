import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import type { AddedRemoteHost } from "@/types";
import { Btn, Card, Notice } from "./ui";

export function RemoteHostAdd({ onAdded }: { onAdded: (host: AddedRemoteHost) => void }) {
  const { t } = useTranslation();
  const [destination, setDestination] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const lock = useRef(false);
  async function add() {
    if (lock.current || !destination.trim()) return;
    lock.current = true; setBusy(true); setFailure(null);
    try { onAdded(await api.addRemoteSshHost(destination.trim())); }
    catch (error) { setFailure(String(error)); }
    finally { lock.current = false; setBusy(false); }
  }
  return <Card><h3>{t("remoteAdd.title")}</h3><p className="note">{t("remoteAdd.description")}</p>
    <form onSubmit={(event) => { event.preventDefault(); void add(); }} style={{ display: "grid", gap: 12 }}>
      <label style={{ display: "grid", gap: 8 }}>{t("remoteAdd.destination")}<input data-remote-add-input className="input" value={destination} onChange={(event) => setDestination(event.target.value)} placeholder="user@host:22" disabled={busy} autoComplete="off" spellCheck={false} /></label>
      <div><Btn type="submit" disabled={busy || !destination.trim()}>{t(busy ? "remoteAdd.adding" : "remoteAdd.add")}</Btn></div>
    </form>
    {busy ? <p role="status">{t("remoteAdd.saving")}</p> : null}
    {failure ? <Notice tone="warn" raw={failure}>{t("remoteAdd.failed")}</Notice> : null}
  </Card>;
}

export function RemoteHostLoginGuide({ host }: { host: AddedRemoteHost }) {
  const { t } = useTranslation();
  const [failure, setFailure] = useState<string | null>(null);
  const copy = async (text: string) => { try { await navigator.clipboard.writeText(text); } catch (error) { setFailure(String(error)); } };
  return <Notice acts={<Btn mini soft onClick={() => void copy(host.keyInstallCommand)}>{t("remoteAdd.copyInstall")}</Btn>}>
    <strong>{t("remoteAdd.saved")}</strong><p>{t("remoteAdd.loginHint")}</p>
    <p><code style={{ overflowWrap: "anywhere" }}>{host.loginCommand}</code> <Btn mini soft onClick={() => void copy(host.loginCommand)}>{t("remoteAdd.copyLogin")}</Btn></p>
    <p>{t("remoteAdd.keyHint")}</p><p><code style={{ overflowWrap: "anywhere" }}>{host.keyInstallCommand}</code></p>
    {failure ? <p>{t("remoteAdd.copyFailed")}</p> : null}
  </Notice>;
}
