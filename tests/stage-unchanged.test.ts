import { chmodSync, mkdtempSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { stageUnchangedSkip } from "../scripts/stage-unchanged.mjs";

describe("sidecar staging", () => {
  it("does not rewrite a destination whose bytes already match", () => {
    const dir = mkdtempSync(join(tmpdir(), "stage-unchanged-"));
    const source = join(dir, "in.bin");
    const dest = join(dir, "out.bin");
    writeFileSync(source, Buffer.from("same-bytes"));
    writeFileSync(dest, Buffer.from("same-bytes"));
    const before = statSync(dest).mtimeMs;
    utimesSync(dest, new Date(before - 60_000), new Date(before - 60_000));
    const stamped = statSync(dest).mtimeMs;
    expect(stageUnchangedSkip(source, dest)).toBe(false);
    expect(statSync(dest).mtimeMs).toBe(stamped);
    expect(readFileSync(dest).toString()).toBe("same-bytes");
  });

  it("replaces the destination when content changes", () => {
    const dir = mkdtempSync(join(tmpdir(), "stage-changed-"));
    const source = join(dir, "in.bin");
    const dest = join(dir, "out.bin");
    writeFileSync(source, Buffer.from("new-bytes"));
    writeFileSync(dest, Buffer.from("old-bytes"));
    expect(stageUnchangedSkip(source, dest)).toBe(true);
    expect(readFileSync(dest).toString()).toBe("new-bytes");
  });

  // The 0.2.7 DMG shipped its bridge and core as 0644. Windows has no execute
  // bit to carry, so this only means something on macOS and Linux.
  it.skipIf(process.platform === "win32")(
    "carries the executable bit, including onto a destination it skips",
    () => {
      const dir = mkdtempSync(join(tmpdir(), "stage-mode-"));
      const source = join(dir, "in.bin");
      const dest = join(dir, "out.bin");
      writeFileSync(source, Buffer.from("binary"));
      chmodSync(source, 0o755);
      expect(stageUnchangedSkip(source, dest)).toBe(true);
      expect(statSync(dest).mode & 0o777).toBe(0o755);

      // What an older copy left behind: the right bytes with the wrong mode.
      chmodSync(dest, 0o644);
      const stamped = statSync(dest).mtimeMs;
      expect(stageUnchangedSkip(source, dest)).toBe(false);
      expect(statSync(dest).mode & 0o777).toBe(0o755);
      expect(statSync(dest).mtimeMs).toBe(stamped);
    },
  );
});
