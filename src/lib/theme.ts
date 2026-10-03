/** 外觀偏好。tokens.css 已經把 `:root[data-theme]` 寫成會蓋過系統的 media query，
    這裡只負責記住選擇、設定那個屬性，並讓原生標題列跟著換。 */

export type ThemePreference = "system" | "light" | "dark";

export const THEME_STORAGE_KEY = "vellum.theme";

export function isThemePreference(value: string | null | undefined): value is ThemePreference {
  return value === "system" || value === "light" || value === "dark";
}

export function readThemePreference(): ThemePreference {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    if (isThemePreference(raw)) return raw;
  } catch {
    // 讀不到就當跟隨系統
  }
  return "system";
}

export function applyThemePreference(preference: ThemePreference): void {
  const root = document.documentElement;
  if (preference === "system") delete root.dataset.theme;
  else root.dataset.theme = preference;
  /* 視窗用的是原生 decorations，標題列只認 Tauri 的視窗主題；null 回到跟隨系統。
     預覽台與瀏覽器裡沒有 Tauri，失敗就算了，頁面本身已經換好。 */
  if ("__TAURI_INTERNALS__" in window) {
    void import("@tauri-apps/api/window")
      .then(({ getCurrentWindow }) =>
        getCurrentWindow().setTheme(preference === "system" ? null : preference),
      )
      .catch(() => undefined);
  }
}

export function setThemePreference(preference: ThemePreference): void {
  try {
    if (preference === "system") localStorage.removeItem(THEME_STORAGE_KEY);
    else localStorage.setItem(THEME_STORAGE_KEY, preference);
  } catch {
    // 存不了仍然套用到這次
  }
  applyThemePreference(preference);
}
