import { describe, expect, it } from "vitest";
import {
  isOpenCodeZenEndpoint,
  OPENCODE_GO_BASE_URL,
  OPENCODE_ZEN_BASE_URL,
  providerPresetDraft,
} from "../src/lib/providerPresets";

describe("provider presets", () => {
  it("keeps the OpenCode Zen endpoint fixed and does not require an API key for free models", () => {
    expect(providerPresetDraft("opencodeZen")).toEqual({
      name: "OpenCode Zen",
      baseUrl: OPENCODE_ZEN_BASE_URL,
      apiKeyRequired: false,
    });
  });

  it("keeps the OpenCode Go endpoint fixed and requires an API key", () => {
    expect(providerPresetDraft("opencodeGo")).toEqual({
      name: "OpenCode Go",
      baseUrl: OPENCODE_GO_BASE_URL,
      apiKeyRequired: true,
    });
  });

  it("recognizes both the OpenCode Zen and OpenCode Go API roots", () => {
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/v1")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/v1/")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/v1/models")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/go/v1")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/go/v1/")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://opencode.ai/zen/go/v1/models")).toBe(true);
    expect(isOpenCodeZenEndpoint("https://example.test/zen/v1")).toBe(false);
    expect(isOpenCodeZenEndpoint("http://opencode.ai/zen/v1")).toBe(false);
    expect(isOpenCodeZenEndpoint("https://opencode.ai:8443/zen/v1")).toBe(false);
    expect(isOpenCodeZenEndpoint("https://user@opencode.ai/zen/v1")).toBe(false);
  });

  it("leaves custom providers editable and key-optional", () => {
    expect(providerPresetDraft("custom")).toEqual({
      name: "",
      baseUrl: "",
      apiKeyRequired: false,
    });
  });
});
