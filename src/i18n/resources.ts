import zhTW from "./locales/zh-TW";
import zhCN from "./locales/zh-CN";
import en from "./locales/en";
import ja from "./locales/ja";
import type { AppLocale } from "./locale";

export type ResourceTree = {
  [key: string]: string | ResourceTree;
};

export const resources = {
  "zh-TW": { translation: zhTW },
  "zh-CN": { translation: zhCN },
  en: { translation: en },
  ja: { translation: ja },
} as const satisfies Record<AppLocale, { translation: ResourceTree }>;

export type TranslationResources = typeof resources;

function flattenKeys(tree: ResourceTree, prefix = ""): string[] {
  const keys: string[] = [];
  for (const [key, value] of Object.entries(tree)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "string") keys.push(path);
    else keys.push(...flattenKeys(value, path));
  }
  return keys.sort();
}

export function resourceKeySet(locale: AppLocale): string[] {
  return flattenKeys(resources[locale].translation);
}

export function assertResourceParity(): void {
  const baseline = resourceKeySet("zh-TW");
  for (const locale of Object.keys(resources) as AppLocale[]) {
    if (locale === "zh-TW") continue;
    const keys = resourceKeySet(locale);
    if (keys.length !== baseline.length || keys.some((key, index) => key !== baseline[index])) {
      const missing = baseline.filter((key) => !keys.includes(key));
      const extra = keys.filter((key) => !baseline.includes(key));
      throw new Error(
        `i18n resource key mismatch for ${locale}: missing=${missing.join(",")} extra=${extra.join(",")}`,
      );
    }
  }
}
