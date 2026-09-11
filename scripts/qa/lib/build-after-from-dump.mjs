import { readFileSync } from "node:fs";

const UI_FIELDS = ["activeTab", "uiState", "refreshedAt", "uiValue"];

/**
 * Build the `after` snapshot for assertionAfterAction from a dump.ps1 probe.
 * `clickedControl` is ignored for UI fields so a click cannot invent a PASS.
 */
export function buildAfterFromDump({ clickedControl, dump } = {}) {
  void clickedControl;
  const src = dump && typeof dump === "object" ? dump : {};
  const after = {
    processes: Array.isArray(src.processes) ? [...src.processes] : null,
    proxyRunning: Object.prototype.hasOwnProperty.call(src, "proxyRunning") ? Boolean(src.proxyRunning) : null,
    activeTab: src.activeTab ?? null,
    refreshedAt: src.refreshedAt ?? null,
    uiState: src.uiState ?? null,
    uiValue: src.uiValue ?? null,
    backendValue: src.backendValue ?? null,
    fileChanged: Boolean(src.fileChanged),
    unit: src.unit ?? null,
    usageSource: src.usageSource ?? null,
    attestation: src.attestation ?? null,
    digest: src.digest ?? src.attestation?.digest ?? null,
    backendSources: Array.isArray(src.backendSources) ? [...src.backendSources] : [],
    window: src.window ?? null,
  };
  for (const field of UI_FIELDS) {
    if (clickedControl && after[field] === undefined) after[field] = null;
  }
  return after;
}

export { UI_FIELDS };

if (String(process.argv[1] ?? "").replace(/\\/g, "/").endsWith("build-after-from-dump.mjs")) {
  const dump = JSON.parse(readFileSync(process.argv[2], "utf8"));
  const clickedControl = process.argv[3] ?? "";
  process.stdout.write(`${JSON.stringify(buildAfterFromDump({ clickedControl, dump }))}\n`);
}
