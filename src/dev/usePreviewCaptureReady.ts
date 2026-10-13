import { useEffect } from "react";
import i18n from "i18next";

/** Visible DOM marker after fonts and paint; capture mode freezes visual motion. */
export function usePreviewCaptureReady() {
  useEffect(() => {
    // Preview canvases can be focused for keyboard scrolling during visual QA.
    document.querySelector(".canvas")?.setAttribute("tabindex", "-1");
    const captureStyle = document.createElement("style");
    if (new URLSearchParams(window.location.search).get("capture") === "1") {
      captureStyle.textContent = "*, *::before, *::after { transition-duration: 0s !important; animation-duration: 0s !important; animation-iteration-count: 1 !important; scroll-behavior: auto !important; }";
      document.head.append(captureStyle);
    }
    let revision = 0;
    let alive = true;
    const paint = () => new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()));
    const changed = () => {
      const mine = ++revision;
      document.documentElement.dataset.previewReady = "";
      void (async () => {
        await document.fonts.ready;
        await paint(); await paint();
        await paint();
        if (alive && mine === revision) document.documentElement.dataset.previewReady = `${i18n.resolvedLanguage}|${document.documentElement.dataset.theme}|${window.innerWidth}`;
      })();
    };
    const body = new MutationObserver(changed);
    body.observe(document.body, { childList: true, subtree: true, characterData: true });
    const root = new MutationObserver(changed);
    root.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme", "lang"] });
    window.addEventListener("resize", changed);
    const canvas = document.querySelector(".canvas");
    canvas?.addEventListener("scroll", changed);
    changed();
    return () => { alive = false; body.disconnect(); root.disconnect(); captureStyle.remove(); window.removeEventListener("resize", changed); canvas?.removeEventListener("scroll", changed); };
  }, []);
}
