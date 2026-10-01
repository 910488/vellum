import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { ORIGINAL_GATE, REPAIRED_GATE, ORIGINAL_DISABLE, REPAIRED_DISABLE, ORIGINAL_HEADER_HASH, REPAIRED_HEADER_HASH, patchExecutableIntegrity, patchRenderer, sha256, patchArchiveCopy, inspectArchive, verifyCopy } from "./codex-steer-repair.mjs";

test("exhausted login allows composer submission while retaining the subscription", () => {
  let calls = 0;
  const context = { rP: {}, q: () => { calls++; return true; }, bt: "local" };
  assert.equal(vm.runInNewContext(`{ let ${ORIGINAL_GATE}; fn }`, context), true);
  assert.equal(vm.runInNewContext(`{ let ${REPAIRED_GATE}; fn }`, context), false);
  assert.equal(calls, 2);
});

test("only composer gate changes, preserving bytes and other checks", () => {
  const original = Buffer.from(`before;${ORIGINAL_GATE};let ${ORIGINAL_DISABLE};after;`);
  const patched = patchRenderer(original, sha256(original));
  assert.equal(patched.length, original.length);
  assert.equal(patched.toString(), original.toString().replace(ORIGINAL_GATE, REPAIRED_GATE)
    .replace(ORIGINAL_DISABLE, REPAIRED_DISABLE));
  assert.throws(() => patchRenderer(original), /Unsupported/);
  const duplicated = Buffer.concat([original, original]);
  assert.throws(() => patchRenderer(duplicated, sha256(duplicated)), /exactly once/);
});

test("Luna Reserve quota gate releases while other submit restrictions remain effective", () => {
  const context = { je: false, St: false, rt: false, it: false, vt: false,
    Ot: false, rn: null, wt: true, fn: false };
  const blocked = (expression) => vm.runInNewContext(`{ let ${expression}; Gn }`, context);
  assert.equal(blocked(ORIGINAL_DISABLE), true);
  assert.equal(blocked(REPAIRED_DISABLE), false);
  for (const key of ["je", "St", "vt", "Ot"]) {
    context[key] = true;
    assert.equal(blocked(REPAIRED_DISABLE), true);
    context[key] = false;
  }
  context.rn = { isLoading: true };
  assert.equal(blocked(REPAIRED_DISABLE), true);
});

test("refuses in-place writes and existing destinations", () => {
  assert.throws(() => patchArchiveCopy("app.asar", "app.asar"), /installed/);
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "vellum-steer-test-"));
  try {
    const existing = path.join(directory, "existing.asar");
    fs.writeFileSync(existing, "keep");
    assert.throws(() => patchArchiveCopy("missing.asar", existing), /already exists/);
    assert.equal(fs.readFileSync(existing, "utf8"), "keep");
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test("native integrity update preserves executable length and refuses ambiguous metadata", () => {
  const original = Buffer.from(`prefix${ORIGINAL_HEADER_HASH}suffix`);
  const patched = patchExecutableIntegrity(original);
  assert.equal(patched.toString(), `prefix${REPAIRED_HEADER_HASH}suffix`);
  assert.equal(patched.length, original.length);
  assert.throws(() => patchExecutableIntegrity(Buffer.from("unknown")), /Unsupported/);
  assert.throws(() => patchExecutableIntegrity(Buffer.concat([original, original])), /Unsupported/);
});

test("prepared native copy matches the installed archive and the minimal executable update", {
  skip: !process.env.VELLUM_STEER_REPAIR_SOURCE || !process.env.VELLUM_STEER_REPAIR_COPY,
}, () => {
  const source = process.env.VELLUM_STEER_REPAIR_SOURCE;
  const copy = process.env.VELLUM_STEER_REPAIR_COPY;
  const original = inspectArchive(path.join(source, "resources", "app.asar"));
  const repaired = inspectArchive(path.join(copy, "resources", "app.asar"));
  assert.equal(sha256(original.rawHeader), ORIGINAL_HEADER_HASH);
  assert.equal(sha256(repaired.rawHeader), REPAIRED_HEADER_HASH);
  assert.deepEqual(repaired.renderer, patchRenderer(original.renderer));
  assert.equal(original.offset, repaired.offset);
  original.entry.integrity = repaired.entry.integrity;
  assert.deepEqual(original.header, repaired.header);
  const originalExe = fs.readFileSync(path.join(source, "ChatGPT.exe"));
  assert.deepEqual(fs.readFileSync(path.join(copy, "ChatGPT.exe")), patchExecutableIntegrity(originalExe));
  assert.doesNotThrow(() => verifyCopy(copy));
});

test("launcher refuses disabled pooling and validates an enabled launch without starting Desktop", {
  skip: process.platform !== "win32" || !process.env.VELLUM_STEER_REPAIR_COPY,
}, () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "vellum-steer-launch-test-"));
  const launcher = fileURLToPath(new URL("./launch-codex-steer-repair.ps1", import.meta.url));
  const bridge = spawnSync("pwsh", ["-NoProfile", "-Command",
    "[Environment]::GetEnvironmentVariable('CODEX_CLI_PATH', 'User')"], { encoding: "utf8" }).stdout.trim();
  const write = (name, value) => fs.writeFileSync(path.join(directory, name), JSON.stringify(value), "utf8");
  const check = () => spawnSync("pwsh", ["-NoProfile", "-File", launcher,
    "-RepairDirectory", process.env.VELLUM_STEER_REPAIR_COPY, "-DataRoot", directory, "-CheckOnly"], { encoding: "utf8" });
  try {
    fs.mkdirSync(path.join(directory, "enhanced-runtime"));
    write("enhanced-runtime/env-lease.json", { appliedValue: bridge, launchId: "launcher-test" });
    write("enhanced-runtime/launch-manifest.json", { launchId: "launcher-test" });
    write("codex_oauth_accounts.json", { quota_pool: { enabled: false } });
    const disabled = check();
    assert.notEqual(disabled.status, 0);
    assert.match(disabled.stderr, /Enable the Vellum quota pool first/);
    write("codex_oauth_accounts.json", { quota_pool: { enabled: true } });
    const enabled = check();
    assert.equal(enabled.status, 0, enabled.stderr);
    assert.match(enabled.stdout, /prerequisites are satisfied/);
  } finally { fs.rmSync(directory, { recursive: true }); }
});
