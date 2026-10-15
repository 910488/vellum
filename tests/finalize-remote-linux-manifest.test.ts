// @ts-nocheck -- Node-only artifact tests.
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { finalizeRemoteLinuxManifest } from "../scripts/finalize-remote-linux-manifest.mjs";
import { finalizeRemoteDarwinManifest } from "../scripts/finalize-remote-darwin-manifest.mjs";

describe("native Linux remote manifest", () => {
  it("hashes native payloads without broker/archive and preserves schema 5 when Darwin is added", () => {
    const root = mkdtempSync(join(tmpdir(), "vellum-native-manifest-"));
    try {
      for (const arch of ["linux-amd64", "linux-arm64", "darwin-arm64"]) {
        mkdirSync(join(root, arch));
        for (const file of ["codex", "vellum-remote-agent", "vellum-proxy-daemon"]) {
          writeFileSync(join(root, arch, file), `${arch}/${file}`);
        }
      }
      const manifest = finalizeRemoteLinuxManifest(root, "0.2.3", "0.147.0-alpha.6.6", "0.3.0", "abcdef123456");
      expect(manifest.schemaVersion).toBe(5);
      expect(manifest.protocolVersion).toBe(5);
      expect(manifest.broker).toBeUndefined();
      expect(manifest.proxy.image).toBeUndefined();
      expect(manifest.proxy.artifacts["linux-arm64"]).toEqual({
        url: "bundle://linux-arm64/vellum-proxy-daemon",
        sha256: createHash("sha256").update("linux-arm64/vellum-proxy-daemon").digest("hex"),
      });
      const linuxArtifacts = structuredClone(manifest.proxy.artifacts);
      const finalized = finalizeRemoteDarwinManifest(root);
      expect(finalized.schemaVersion).toBe(5);
      expect(finalized.protocolVersion).toBe(5);
      expect(finalized.proxy.artifacts["linux-arm64"]).toEqual(linuxArtifacts["linux-arm64"]);
      expect(JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"))).toEqual(finalized);
      rmSync(join(root, "linux-arm64", "vellum-proxy-daemon"));
      expect(() => finalizeRemoteLinuxManifest(root, "0.2.3", "0.147.0", "0.3.0", "abcdef123456")).toThrow();
      expect(JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"))).toEqual(finalized);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
