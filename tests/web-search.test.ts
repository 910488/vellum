import { describe, expect, it } from "vitest";
import {
  domainRuleCount,
  formatDomainList,
  isOn,
  needsBraveKey,
  parseDomainList,
  reachOf,
  setReach,
  turnOff,
  turnOn,
} from "@/lib/webSearch";
import type { WebSearchSettings } from "@/types";

function settings(patch: Partial<WebSearchSettings> = {}): WebSearchSettings {
  return {
    enabled: false,
    mode: "live",
    domainPolicy: { allow: [], block: [] },
    searchContextSize: "medium",
    ...patch,
  };
}

describe("isOn", () => {
  it("兩個關只要有一個關著就是關著 —— enabled 與 mode 講的是同一件事", () => {
    expect(isOn(settings({ enabled: false, mode: "live" }))).toBe(false);
    expect(isOn(settings({ enabled: true, mode: "disabled" }))).toBe(false);
    /* cached 的 allows_search() 是 false，開著等於每次搜尋都拋錯 */
    expect(isOn(settings({ enabled: true, mode: "cached" }))).toBe(false);
    expect(isOn(settings({ enabled: true, mode: "indexed" }))).toBe(true);
    expect(isOn(settings({ enabled: true, mode: "live" }))).toBe(true);
  });
});

describe("turnOn / turnOff", () => {
  it("開的時候一次寫對兩個欄位，不會留下 enabled=true 但 mode=disabled 的組合", () => {
    const next = turnOn(settings({ enabled: false, mode: "disabled" }));
    expect(next).toMatchObject({ enabled: true, mode: "live" });
    expect(isOn(next)).toBe(true);
  });

  it("已經選過「只讀搜尋結果」就留著 —— 關了再開不該擅自放寬成可開啟網頁", () => {
    expect(turnOn(settings({ enabled: false, mode: "indexed" })).mode).toBe("indexed");
  });

  it("關只動 enabled，範圍留著，下次開回得到同一個選擇", () => {
    const off = turnOff(settings({ enabled: true, mode: "indexed" }));
    expect(off).toMatchObject({ enabled: false, mode: "indexed" });
    expect(turnOn(off).mode).toBe("indexed");
  });
});

describe("reachOf / setReach", () => {
  it("只有 indexed 是「只讀搜尋結果」，其餘一律當可開啟網頁", () => {
    expect(reachOf(settings({ mode: "indexed" }))).toBe("indexed");
    expect(reachOf(settings({ mode: "live" }))).toBe("live");
  });

  it("換範圍不會動到開關", () => {
    const next = setReach(settings({ enabled: true, mode: "live" }), "indexed");
    expect(next).toMatchObject({ enabled: true, mode: "indexed" });
  });
});

describe("needsBraveKey", () => {
  it("Brave 是唯一的後端；開著卻沒有金鑰就是還沒設定完", () => {
    expect(needsBraveKey(settings({ enabled: true, mode: "live" }), false)).toBe(true);
    expect(needsBraveKey(settings({ enabled: true, mode: "live" }), true)).toBe(false);
  });

  it("關著的時候不管有沒有金鑰都不算「還沒設定完」", () => {
    expect(needsBraveKey(settings({ enabled: false }), false)).toBe(false);
    expect(needsBraveKey(settings({ enabled: true, mode: "disabled" }), false)).toBe(false);
  });
});

describe("網域清單", () => {
  it("換行與逗號都當分隔，空行不算一個網域", () => {
    expect(parseDomainList("example.com\n\n  docs.rs , \nnews.ycombinator.com\n")).toEqual([
      "example.com",
      "docs.rs",
      "news.ycombinator.com",
    ]);
  });

  it("往返不掉值", () => {
    const domains = ["example.com", "docs.rs"];
    expect(parseDomainList(formatDomainList(domains))).toEqual(domains);
  });

  it("收起來時要看得到規則數量 —— 允許與封鎖一起算", () => {
    expect(
      domainRuleCount(
        settings({ domainPolicy: { allow: ["a.com"], block: ["b.com", "c.com"] } }),
      ),
    ).toBe(3);
  });
});
