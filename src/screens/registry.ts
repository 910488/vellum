/**
 * Screen registry. Labels/titles/blurbs come from i18n keys.
 */

export type ScreenId = "today" | "models" | "enhanced" | "context" | "remote" | "log" | "settings";

export interface ScreenDef {
  id: ScreenId;
  labelKey: string;
  titleKey: string;
  blurbKey: string;
}

export const SCREENS: ScreenDef[] = [
  {
    id: "today",
    labelKey: "navigation.today.label",
    titleKey: "navigation.today.title",
    blurbKey: "navigation.today.blurb",
  },
  {
    id: "models",
    labelKey: "navigation.models.label",
    titleKey: "navigation.models.title",
    blurbKey: "navigation.models.blurb",
  },
  {
    id: "context",
    labelKey: "navigation.context.label",
    titleKey: "navigation.context.title",
    blurbKey: "navigation.context.blurb",
  },
  {
    id: "enhanced",
    labelKey: "navigation.enhanced.label",
    titleKey: "navigation.enhanced.title",
    blurbKey: "navigation.enhanced.blurb",
  },
  {
    id: "remote",
    labelKey: "navigation.remote.label",
    titleKey: "navigation.remote.title",
    blurbKey: "navigation.remote.blurb",
  },
  {
    id: "log",
    labelKey: "navigation.log.label",
    titleKey: "navigation.log.title",
    blurbKey: "navigation.log.blurb",
  },
  {
    id: "settings",
    labelKey: "navigation.settings.label",
    titleKey: "navigation.settings.title",
    blurbKey: "navigation.settings.blurb",
  },
];

export function screenById(id: ScreenId): ScreenDef {
  const found = SCREENS.find((s) => s.id === id);
  if (!found) throw new Error(`unknown screen: ${id}`);
  return found;
}
