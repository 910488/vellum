/**
 * Left navigation rail.
 */
import { useTranslation } from "react-i18next";
import type { ScreenId } from "@/screens/registry";
import { SCREENS } from "@/screens/registry";

export function Rail({
  active,
  onNavigate,
  attention,
}: {
  active: ScreenId;
  onNavigate: (id: ScreenId) => void;
  attention: Partial<Record<ScreenId, number>>;
}) {
  const { t } = useTranslation();
  return (
    <nav className="rail" aria-label={t("navigation.mainAria")}>
      {SCREENS.map((screen, index) => {
        const count = attention[screen.id] ?? 0;
        const current = screen.id === active;
        const label = t(screen.labelKey);
        return (
          <button
            key={screen.id}
            type="button"
            className="navitem"
            aria-current={current ? "page" : undefined}
            onClick={() => onNavigate(screen.id)}
            title={t("common.shortcutTitle", { label, n: index + 1 })}
          >
            <span className="navitem__top">
              <span className="navitem__label">{label}</span>
              {count > 0 ? (
                <span className="navitem__badge">
                  {count}
                  <span className="sr-only"> {t("common.itemsNeedAttention", { count })}</span>
                </span>
              ) : null}
            </span>
            {current ? <span className="navitem__blurb">{t(screen.blurbKey)}</span> : null}
          </button>
        );
      })}
    </nav>
  );
}
