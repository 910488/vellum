import { afterEach, describe, expect, it } from "vitest";
import apiSource from "../src/lib/api.ts?raw";
import rustSource from "../src-tauri/src/lib.rs?raw";
import appSource from "../src/App.tsx?raw";
import modelsSource from "../src/screens/Models.tsx?raw";
import {
  getInvokeRing,
  INVOKE_RING_CAP,
  recordInvokeResult,
  resetInvokeRing,
} from "../src/lib/api";

function unique(values: Iterable<string>): string[] {
  return [...new Set(values)].sort();
}

describe("Tauri API contract", () => {
  afterEach(() => {
    resetInvokeRing();
  });

  it("registers every command invoked by the renderer", () => {
    const invoked = unique(
      [...apiSource.matchAll(/call(?:<[^;]+?>)?\("([a-z0-9_]+)"/g)].flatMap((match) =>
        match[1] ? [match[1]] : [],
      ),
    );
    const registered = new Set(
      [
        ...rustSource.matchAll(/commands::([a-z0-9_]+)/g),
        ...rustSource.matchAll(/ssh_trust::([a-z0-9_]+)/g),
      ].flatMap((match) => (match[1] ? [match[1]] : [])),
    );
    expect(invoked.filter((command) => !registered.has(command))).toEqual([]);
  });

  it("manual refresh propagates a force-refresh epoch to page quota queries", () => {
    expect(appSource).toMatch(/setRefreshVersion/);
    expect(appSource).toMatch(/refresh\(true, false\)/);
    expect(appSource).toMatch(/pageRefreshComplete/);
    expect(modelsSource).toMatch(/getCodexOAuthAccountQuota\([\s\S]*forceQuotaRefresh/);
    expect(modelsSource).toMatch(/getGrokAccountQuota\([\s\S]*forceQuotaRefresh/);
    expect(apiSource).toMatch(/get_provider_overviews[\s\S]*forceRefresh/);
  });

  it("keeps a capped in-memory invoke ring for success and failure", () => {
    resetInvokeRing();
    expect(INVOKE_RING_CAP).toBe(200);
    expect(apiSource).toMatch(/INVOKE_RING_CAP = 200/);
    recordInvokeResult("get_request_log", true);
    recordInvokeResult("missing_command", false, "command not found");
    const ring = getInvokeRing();
    expect(ring).toHaveLength(2);
    expect(ring[0]).toMatchObject({ cmd: "get_request_log", ok: true });
    expect(ring[1]).toMatchObject({
      cmd: "missing_command",
      ok: false,
      error: "command not found",
    });
    for (let i = 0; i < INVOKE_RING_CAP + 5; i += 1) {
      recordInvokeResult(`cmd_${i}`, i % 2 === 0);
    }
    const capped = getInvokeRing();
    expect(capped).toHaveLength(INVOKE_RING_CAP);
    expect(capped[0]?.cmd).toBe("cmd_5");
    expect(capped[capped.length - 1]?.cmd).toBe(`cmd_${INVOKE_RING_CAP + 4}`);
  });
});
