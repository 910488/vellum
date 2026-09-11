import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { SCREENS } from "@/screens/registry";
import {
  CASES,
  MATRIX,
  MATRIX_VERSION,
  PASS_CLAIM,
  REQUIRED_DOMAINS,
  REQUIRED_SURFACES,
  REQUIRED_TABS,
  TAB_CONTROL,
  VERDICTS,
} from "../qa/matrix/v1.mjs";

const CASE_FIELDS = [
  "preconditions",
  "steps",
  "expected",
  "verificationSources",
  "automation",
  "evidence",
] as const;

describe("versioned acceptance matrix", () => {
  it("is v1 and uses the four-way verdict vocabulary", () => {
    expect(MATRIX_VERSION).toBe("v1");
    expect(VERDICTS).toEqual(["PASS", "FAIL", "BLOCKED", "NOT_RUN"]);
    expect(MATRIX.verdicts).toEqual(VERDICTS);
  });

  it("excludes macOS from the pass claim and keeps Official live as NOT_RUN", () => {
    expect(PASS_CLAIM.excludePlatforms).toContain("macos");
    expect(PASS_CLAIM.platforms).toEqual(["windows"]);
    expect(PASS_CLAIM.officialLive).toBe("NOT_RUN");
    const official = CASES.filter((item) => item.officialLive);
    expect(official.length).toBeGreaterThan(0);
    for (const item of official) {
      expect(item.defaultVerdict ?? "NOT_RUN").toBe("NOT_RUN");
    }
  });

  it("documents that new Tabs must update the matrix", () => {
    expect(PASS_CLAIM.tabSyncRule).toMatch(/ScreenId/);
    expect(PASS_CLAIM.tabSyncRule).toMatch(/tests\/qa-matrix\.test\.ts/);
  });

  it("covers every required domain and Tab surface", () => {
    const domains = new Set(CASES.map((item) => item.domain));
    for (const domain of REQUIRED_DOMAINS) {
      expect(domains.has(domain)).toBe(true);
    }
    const surfaces = new Set(CASES.map((item) => item.surface));
    for (const surface of REQUIRED_SURFACES) {
      expect(surfaces.has(surface)).toBe(true);
    }
    expect(REQUIRED_TABS).toEqual([
      "today",
      "models",
      "context",
      "enhanced",
      "remote",
      "log",
      "settings",
    ]);
  });

  it("gives every case the six required fields", () => {
    expect(CASES.length).toBeGreaterThan(50);
    for (const item of CASES) {
      for (const field of CASE_FIELDS) {
        const value = item[field];
        expect(value).toBeTruthy();
        if (Array.isArray(value)) expect(value.length).toBeGreaterThan(0);
      }
    }
  });

  it("gives every desktop-ui case a control name so the driver cannot PASS on window presence", () => {
    for (const item of CASES) {
      if (item.automation === "desktop-ui") {
        expect(item.control || TAB_CONTROL[item.surface]).toBeTruthy();
      }
    }
  });

  it("does not require PNG screenshots for docker-dev-host cases", () => {
    for (const item of CASES) {
      if (item.automation === "docker-dev-host") {
        expect(item.evidence.some((file: string) => file.toLowerCase().endsWith(".png"))).toBe(false);
      }
    }
  });

  it("fails CI if a ScreenId is added without matrix cases", () => {
    const screenIds = SCREENS.map((screen) => screen.id).sort();
    expect(screenIds).toEqual([...REQUIRED_TABS].sort());
    for (const id of screenIds) {
      const hits = CASES.filter((item) => item.surface === id);
      expect(hits.length).toBeGreaterThan(0);
    }
  });

  it("does not treat renderer-mock-only or Broker-as-authority as live Enhanced/Remote pass criteria", () => {
    const live = CASES.filter((item) => item.lane === "live");
    for (const item of live) {
      expect(item.automation).not.toBe("vitest-jsdom");
      expect(String(item.preconditions + item.expected)).not.toMatch(/legacy Broker as authority/i);
    }
  });
});

describe("process docs", () => {
  const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

  it("indexes the QA process from docs/README.md", () => {
    const index = readFileSync(path.join(repo, "docs", "README.md"), "utf8");
    expect(index).toContain("qa-standard-process.md");
    expect(index).toContain("qa-acceptance-matrix.md");
  });
});
