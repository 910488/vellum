// @vitest-environment node
// @ts-nocheck -- Node-only release download tests.
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { downloadCodexPackage } from "../scripts/download-codex-package.mjs";

describe("official Codex runtime package downloads", () => {
  it("downloads the full package and verifies publisher metadata before replacing cached bytes", async () => {
    const root = mkdtempSync(join(tmpdir(), "vellum-codex-package-"));
    try {
      const destination = join(root, "codex-package.tar.gz");
      const version = "0.147.0-alpha.6.6";
      const target = "aarch64-unknown-linux-musl";
      const name = `codex-package-${target}.tar.gz`;
      const url = `https://github.com/openai/codex/releases/download/rust-v${version}/${name}`;
      const bytes = Buffer.from("official full runtime archive");
      const sha256 = createHash("sha256").update(bytes).digest("hex");
      const release = { tag_name: `rust-v${version}`, assets: [{ name, browser_download_url: url, digest: `sha256:${sha256}` }] };
      const requests = [];
      const fetcher = async request => {
        requests.push(request);
        return requests.length % 2 === 1 ? Response.json(release) : new Response(bytes);
      };
      expect(await downloadCodexPackage(version, target, destination, fetcher)).toBe(sha256);
      expect(requests[1]).toBe(url);
      expect(readFileSync(destination)).toEqual(bytes);
      for (const mutation of [
        () => { release.assets[0].digest = `sha256:${"0".repeat(64)}`; },
        () => { release.assets[0].digest = null; },
        () => { release.tag_name = "rust-v0.153.0"; },
      ]) {
        mutation();
        writeFileSync(destination, "previous verified package");
        requests.length = 0;
        await expect(downloadCodexPackage(version, target, destination, fetcher)).rejects.toThrow();
        expect(readFileSync(destination, "utf8")).toBe("previous verified package");
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  it("authenticates only the API lookup when CI provides a token", async () => {
    const root = mkdtempSync(join(tmpdir(), "vellum-codex-package-"));
    const previous = process.env.GITHUB_TOKEN;
    process.env.GITHUB_TOKEN = "test-token";
    try {
      const version = "0.147.0-alpha.6.6";
      const target = "aarch64-apple-darwin";
      const name = `codex-package-${target}.tar.gz`;
      const url = `https://github.com/openai/codex/releases/download/rust-v${version}/${name}`;
      const bytes = Buffer.from("archive");
      const sha256 = createHash("sha256").update(bytes).digest("hex");
      const release = { tag_name: `rust-v${version}`, assets: [{ name, browser_download_url: url, digest: `sha256:${sha256}` }] };
      const calls = [];
      const fetcher = async (request, init) => {
        calls.push(init?.headers ?? {});
        return calls.length === 1 ? Response.json(release) : new Response(bytes);
      };
      await downloadCodexPackage(version, target, join(root, "pkg.tar.gz"), fetcher);
      expect(calls[0].Authorization).toBe("Bearer test-token");
      expect(calls[1].Authorization).toBeUndefined();
    } finally {
      if (previous === undefined) delete process.env.GITHUB_TOKEN;
      else process.env.GITHUB_TOKEN = previous;
      rmSync(root, { recursive: true, force: true });
    }
  });
});
