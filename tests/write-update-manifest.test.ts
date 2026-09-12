import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  assignTargetProtocolDigest,
  hashSchemaDir,
  isCoreArchiveName,
} from "../scripts/lib/core-manifest-protocol.mjs";

const LOCK_FIXTURE = "ed3876ba3f0d615256174caa177bfd10feb2e9925de692d01fe900adaacb887e";

describe("core manifest protocol digest", () => {
  it("keeps protocol sidecars out of the signed archive asset list", () => {
    expect(isCoreArchiveName("enhanced-core.zip")).toBe(true);
    expect(isCoreArchiveName("enhanced-core.tar.gz")).toBe(true);
    expect(isCoreArchiveName("enhanced-core.tgz")).toBe(true);
    expect(isCoreArchiveName("enhanced-core.zip.protocol-schema-sha256")).toBe(false);
  });

  it("does not write a disagreeing lock fixture hash as the target digest", () => {
    const probed = "ab".repeat(32);
    const digest = assignTargetProtocolDigest({
      lockDigest: LOCK_FIXTURE,
      probedDigest: probed,
    });
    expect(digest).toBe(probed);
    expect(digest).not.toBe(LOCK_FIXTURE);
  });

  it("prefers a sidecar probe over the lock fixture", () => {
    const sidecar = "cd".repeat(32);
    const digest = assignTargetProtocolDigest({
      lockDigest: `sha256:${LOCK_FIXTURE}`,
      sidecarDigest: sidecar,
    });
    expect(digest).toBe(sidecar);
    expect(digest).not.toBe(LOCK_FIXTURE);
  });

  it("refuses to copy the lock fixture when no probe exists", () => {
    expect(() =>
      assignTargetProtocolDigest({
        lockDigest: LOCK_FIXTURE,
      }),
    ).toThrow(/refusing to copy the lock fixture protocol hash/);
  });

  it("hashes schema documents the same way desktop_manager probes", () => {
    const root = mkdtempSync(path.join(tmpdir(), "vellum-schema-"));
    try {
      mkdirSync(path.join(root, "nested"));
      writeFileSync(path.join(root, "ClientRequest.json"), "{\"a\":1}");
      writeFileSync(path.join(root, "nested", "b.json"), "xyz");
      const actual = hashSchemaDir(root);
      const hasher = createHash("sha256");
      hasher.update("ClientRequest.json");
      hasher.update(Buffer.from([0]));
      hasher.update("{\"a\":1}");
      hasher.update("nested/b.json");
      hasher.update(Buffer.from([0]));
      hasher.update("xyz");
      expect(actual).toBe(hasher.digest("hex"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
