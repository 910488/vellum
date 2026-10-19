// @ts-nocheck -- Node-only build artifact checks.
import { mkdtempSync, mkdirSync, rmSync, writeFileSync, existsSync, openSync, ftruncateSync, closeSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { gzipSync } from "node:zlib";
import { describe, expect, it } from "vitest";
import { assertRemotePayloadSize } from "../scripts/remote-codex-artifact.mjs";
import { finalizeRemoteLinuxManifest } from "../scripts/finalize-remote-linux-manifest.mjs";

describe("small remote deployment payload", () => {
  it("rejects resources over 80 MiB, including unexpected files", () => {
    const root = mkdtempSync(join(tmpdir(), "vellum-size-"));
    try {
      const file = openSync(join(root, "accidentally-bundled-runtime"), "w");
      try { ftruncateSync(file, 80 * 1024 * 1024 + 1); } finally { closeSync(file); }
      expect(() => assertRemotePayloadSize(root)).toThrow(/exceeds 80 MiB/);
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
  it.each([false, true])("only bundles Codex with the explicit development option: %s", (bundle) => {
    const root = mkdtempSync(join(tmpdir(), "vellum-small-payload-"));
    try {
      for (const arch of ["linux-amd64", "linux-arm64"]) {
        mkdirSync(join(root, arch));
        writeFileSync(join(root, arch, "codex-package.tar.gz"), gzipSync("runtime"));
        for (const name of ["vellum-remote-agent", "vellum-proxy-daemon"]) writeFileSync(join(root, arch, name), "binary");
      }
      const manifest = finalizeRemoteLinuxManifest(root, "1.0.0", "0.147.0", "1.0.0", "abcdef123456", root, bundle);
      expect(manifest.codex.artifacts["linux-x64"].url.startsWith(bundle ? "bundle://" : "https://")).toBe(true);
      expect(existsSync(join(root, "linux-amd64/codex-package.tar.gz"))).toBe(bundle);
      expect(assertRemotePayloadSize(root)).toBeLessThan(80 * 1024 * 1024);
    } finally { rmSync(root, { recursive: true, force: true }); }
  });
});
