import { useTranslation } from "react-i18next";
import { Cap, Segment } from "@/components/ui";
import type { RemoteSidebarMode } from "@/lib/api";

export function RemoteSidebarPreference({ mode, onChange }: { mode: RemoteSidebarMode; onChange: (mode: RemoteSidebarMode) => void }) {
  const { t } = useTranslation();
  return <div className="prefstrip">
    <div className="prefstrip__copy"><Cap>{t("settings.remoteSidebar.label")}</Cap><p className="note">{t(`settings.remoteSidebar.hint.${mode}`)}</p></div>
    <Segment<RemoteSidebarMode>
      value={mode}
      onChange={onChange}
      options={(["auto", "show", "hide"] as const).map((value) => ({ value, label: t(`settings.remoteSidebar.mode.${value}`) }))}
    />
  </div>;
}
