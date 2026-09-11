import { describe, expect, it } from "vitest";
import { providerNameFromEndpoint } from "@/lib/providerName";

describe("providerNameFromEndpoint", () => {
  const fallback = "Custom endpoint";

  it("strips api/www/gateway service prefixes", () => {
    expect(providerNameFromEndpoint("https://api.provider.example/v1", fallback)).toBe("provider");
    expect(providerNameFromEndpoint("https://api.openrouter.ai/v1", fallback)).toBe("openrouter");
    expect(providerNameFromEndpoint("https://openrouter.ai/v1", fallback)).toBe("openrouter");
    expect(providerNameFromEndpoint("https://www.example.com/v1", fallback)).toBe("example");
    expect(providerNameFromEndpoint("https://gateway.api.openai.com/v1", fallback)).toBe("openai");
  });

  it("keeps a plain second-level label", () => {
    expect(providerNameFromEndpoint("https://openai.com/v1", fallback)).toBe("openai");
    expect(providerNameFromEndpoint("https://mistral.ai/v1", fallback)).toBe("mistral");
  });

  it("falls back for localhost, IP literals and invalid URLs", () => {
    expect(providerNameFromEndpoint("http://localhost:11434", fallback)).toBe(fallback);
    expect(providerNameFromEndpoint("http://127.0.0.1:8080/v1", fallback)).toBe(fallback);
    expect(providerNameFromEndpoint("http://[::1]:8080/v1", fallback)).toBe(fallback);
    expect(providerNameFromEndpoint("192.168.1.10", fallback)).toBe(fallback);
    expect(providerNameFromEndpoint("not a url", fallback)).toBe(fallback);
    expect(providerNameFromEndpoint("", fallback)).toBe(fallback);
  });

  it("does not invent a name from a bare hostname label", () => {
    expect(providerNameFromEndpoint("https://localhost/v1", fallback)).toBe(fallback);
  });
});
