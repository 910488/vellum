// @ts-nocheck -- this Node-only test runs in Vitest; the renderer tsconfig omits Node types.
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { finalizeRemoteDarwinManifest } from "../scripts/finalize-remote-darwin-manifest.mjs";

describe("Darwin remote manifest finalizer", () => {
  it("hashes every Darwin artifact into schema 4", () => {
    const root = mkdtempSync(join(tmpdir(), "vellum-darwin-manifest-"));
    try {
      mkdirSync(join(root, "darwin-arm64"));
      writeFileSync(
        join(root, "manifest.json"),
        JSON.stringify({
          schemaVersion: 3,
          protocolVersion: 3,
          codex: { artifacts: {} },
          agent: { artifacts: {} },
          proxy: { artifacts: {} },
        }),
      );
      writeFileSync(join(root, "darwin-arm64", "codex"), "darwin codex");
      writeFileSync(join(root, "darwin-arm64", "vellum-remote-agent"), "darwin agent");
      writeFileSync(
        join(root, "darwin-arm64", "vellum-proxy-daemon"),
        "darwin proxy",
      );

      const manifest = finalizeRemoteDarwinManifest(root);
      const persisted = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
      expect(manifest.schemaVersion).toBe(4);
      expect(persisted.protocolVersion).toBe(4);
      for (const section of ["codex", "agent", "proxy"]) {
        const artifact = persisted[section].artifacts["darwin-arm64"];
        expect(artifact.url).toMatch(/^bundle:\/\/darwin-arm64\//);
        expect(artifact.sha256).toMatch(/^[0-9a-f]{64}$/);
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
