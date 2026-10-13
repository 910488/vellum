import { useTranslation } from "react-i18next";
import { Cap, Toggle } from "@/components/ui";

export function RemoteSidebarPreference({ forced, onChange }: { forced: boolean; onChange: (forced: boolean) => void }) {
  const { t } = useTranslation();
  return <div className="prefstrip">
    <div className="prefstrip__copy"><Cap>{t("settings.remoteSidebar.label")}</Cap><p className="note">{t("settings.remoteSidebar.hint")}</p></div>
    <Toggle label={t("settings.remoteSidebar.label")} checked={forced} onChange={onChange} />
  </div>;
}
