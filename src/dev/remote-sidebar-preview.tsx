import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { useTranslation } from "react-i18next";
import { setLocalePreference } from "@/i18n";
import { Rail } from "@/components/Rail";
import { RemoteSidebarPreference } from "@/components/RemoteSidebarPreference";
import { Card } from "@/components/ui";
import { accessibleScreen, type ScreenId } from "@/screens/registry";
import type { RemoteSidebarMode } from "@/lib/api";
import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

function Preview() {
  // Reproduce the old unconditional Rail without replacing product code.
  const baseline = new URLSearchParams(window.location.search).get("baseline") === "1";
  const { t } = useTranslation();
  const [locale, setLocale] = useState("zh-TW");
  const [theme, setTheme] = useState("light");
  const [hosts, setHosts] = useState(false);
  const [mode, setMode] = useState<RemoteSidebarMode>("auto");
  const [screen, setScreen] = useState<ScreenId>("settings");
  const remoteVisible = baseline || (mode === "auto" ? hosts : mode === "show");
  useEffect(() => { document.documentElement.dataset.theme = theme; }, [theme]);
  return <>
    <div className="devbar">
      <label>Language<select aria-label="Language" value={locale} onChange={(event) => { setLocale(event.target.value); void setLocalePreference(event.target.value as "zh-TW"); }}>{["zh-TW","zh-CN","en","ja"].map((value) => <option key={value}>{value}</option>)}</select></label>
      <label>Theme<select aria-label="Theme" value={theme} onChange={(event) => setTheme(event.target.value)}><option>light</option><option>dark</option></select></label>
      {!baseline ? <label>Hosts<select aria-label="Hosts" value={hosts ? "one" : "none"} onChange={(event) => setHosts(event.target.value === "one")}><option value="none">None</option><option value="one">One managed host</option></select></label> : null}
    </div>
    <div className="shell" style={{ paddingTop: 40 }}>
      <Rail active={accessibleScreen(screen,remoteVisible)} remoteVisible={remoteVisible} onNavigate={setScreen} onOpenUpdates={() => {}} attention={{}} updateAttention={false} />
      <main className="canvas"><h2>{t(`navigation.${accessibleScreen(screen,remoteVisible)}.label`)}</h2>{baseline ? <p>{t("settings.language.blurb")}</p> : <Card quiet className="prefstrips"><RemoteSidebarPreference mode={mode} onChange={setMode} /></Card>}</main>
    </div>
  </>;
}
ReactDOM.createRoot(document.getElementById("root")!).render(<Preview />);
