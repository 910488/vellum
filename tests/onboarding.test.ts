import { describe, expect, it } from "vitest";
import {
  ONBOARDING_ENABLED,
  buildOnboardingConnections,
  codexCompatibleProbeModels,
  markOnboardingCompleted,
  onboardingWasCompleted,
  shouldShowOnboarding,
} from "../src/lib/onboarding";
import type { ProbeResult, Route } from "../src/types";

const baseProbe: ProbeResult = {
  reachable: true,
  wire: "responses",
  models: ["typed", "text-only", "unknown"],
  contextWindow: 128_000,
  streaming: true,
  reasoning: true,
  serverSideResume: false,
  needsInput: [],
  modelCapabilities: [
    { model: "typed", contextWindow: 128_000, wire: "responses", streaming: true, reasoning: false, toolCalling: true, probeVersion: 4 },
    { model: "text-only", contextWindow: 128_000, wire: "chat", streaming: true, reasoning: false, toolCalling: false, probeVersion: 4 },
    { model: "unknown", contextWindow: null, wire: null, streaming: null, reasoning: null, toolCalling: null, probeVersion: null },
  ],
};

describe("onboarding wiring", () => {
  it("only offers models that passed the typed Codex tool-call probe", () => {
    expect(codexCompatibleProbeModels(baseProbe).map((item) => item.model)).toEqual([
      "typed",
    ]);
  });

  it("maps persisted accounts and custom routes into the connection register", () => {
    const routes = [
      { name: "Self-hosted vLLM", providerKind: "openAiCompatible" },
      { name: "Grok Build", providerKind: "grokCli" },
    ] as Route[];
    const connections = buildOnboardingConnections(
      routes,
      {
        authenticated: true,
        defaultAccountId: "openai-1",
        accounts: [{ accountId: "openai-1", email: "openai@example.test", authenticatedAt: 1, isDefault: true }],
      },
      {
        authenticated: true,
        defaultAccountId: "grok-1",
        accounts: [{
          accountId: "grok-1",
          email: "grok@example.test",
          authenticatedAt: 1,
          isDefault: true,
          source: "managed",
        }],
      },
      "Grok CLI unavailable",
    );
    expect(connections.chatgpt.accounts).toEqual(["openai@example.test"]);
    expect(connections.grok.accounts).toEqual(["grok@example.test"]);
    expect(connections.grok.unavailable).toBeNull();
    expect(connections.custom.accounts).toEqual(["Self-hosted vLLM"]);
  });

  it("marks Grok unavailable only when neither CLI route nor account exists", () => {
    const connections = buildOnboardingConnections([], null, null, "not installed");
    expect(connections.grok.unavailable).toBe("not installed");
  });

  it("persists completion using the same key read on the next launch", () => {
    const values = new Map<string, string>();
    const storage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => void values.set(key, value),
    };
    expect(onboardingWasCompleted(storage)).toBe(false);
    markOnboardingCompleted(storage);
    expect(onboardingWasCompleted(storage)).toBe(true);
  });

  it("keeps onboarding hidden while the release switch is off", () => {
    const empty = { getItem: () => null };
    const completed = { getItem: () => "1" };

    // Flipping ONBOARDING_ENABLED is the whole release switch, so it should be a
    // deliberate edit that updates this test rather than something a refactor
    // can flip by accident.
    expect(ONBOARDING_ENABLED).toBe(false);
    expect(shouldShowOnboarding(empty)).toBe(false);

    // The switch never overrides a real completion — once it is back on, only
    // people who have not finished onboarding will see it.
    expect(onboardingWasCompleted(completed)).toBe(true);
    expect(shouldShowOnboarding(completed)).toBe(false);
  });
});
