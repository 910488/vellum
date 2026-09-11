import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { OFFLINE_COMMANDS } from "../scripts/qa/lib/commands.mjs";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const runner = path.join(repo, "scripts", "qa", "run.mjs");

function tmp() {
  return mkdtempSync(path.join(tmpdir(), "vellum-qa-"));
}

function runQa(args: string[], env: NodeJS.ProcessEnv = process.env) {
  return spawnSync(process.execPath, [runner, ...args], {
    cwd: repo,
    encoding: "utf8",
    env,
  });
}

describe("shipped pnpm qa runner", () => {
  it("fails non-zero when --lane is missing", () => {
    const result = runQa([]);
    expect(result.status).toBe(2);
    expect(result.stderr).toMatch(/missing --lane/);
  });

  it("fails non-zero on an unknown lane", () => {
    const result = runQa(["--lane", "banana"]);
    expect(result.status).toBe(2);
    expect(result.stderr).toMatch(/unknown lane/);
  });

  it("offline dry-run lists the real typecheck / vitest / cargo / replay / enhanced-gate commands", () => {
    const outDir = tmp();
    const result = runQa(["--lane", "offline", "--dry-run", "--out-dir", outDir]);
    expect(result.status).toBe(0);
    const plan = JSON.parse(readFileSync(path.join(outDir, "dry-run.json"), "utf8"));
    const ids = (plan.commands as { id: string; argv: string[] }[]).map((item) => item.id);
    expect(ids).toEqual(OFFLINE_COMMANDS.map((item) => item.id));
    const joined = (plan.commands as { argv: string[] }[])
      .map((item) => item.argv.join(" "))
      .join("\n");
    expect(joined).toContain("pnpm typecheck");
    expect(joined).toContain("pnpm test");
    expect(joined).toContain("cargo test -p vellum-proxy-runtime --lib");
    expect(joined).toContain("cargo run -p vellum-eval --bin vellum-eval -- protocol-replay");
    expect(joined).toContain("scripts/enhanced-gate.mjs status");
    expect(existsSync(path.join(outDir, "report.json"))).toBe(true);
    expect(existsSync(path.join(outDir, "report.html"))).toBe(true);
  });

  it("inject timeout records FAIL/timeout and exits non-zero", () => {
    const outDir = tmp();
    const result = runQa(["--inject", "timeout", "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].id).toBe("inject.timeout");
    expect(report.cases[0].verdict).toBe("FAIL");
    expect(report.cases[0].reason).toBe("timeout");
  });

  it("inject budget-stop records NOT_RUN/budget-stop and exits non-zero", () => {
    const outDir = tmp();
    const result = runQa(["--inject", "budget-stop", "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe("NOT_RUN");
    expect(report.cases[0].reason).toBe("budget-stop");
  });

  it("inject missing-credentials records BLOCKED and exits non-zero", () => {
    const outDir = tmp();
    const env = { ...process.env };
    delete env.VELLUM_QA_INJECT_TOKEN;
    const result = runQa(["--inject", "missing-credentials", "--out-dir", outDir], env);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe("BLOCKED");
    expect(report.cases[0].reason).toBe("missing-credentials");
  });

  it("inject wrong-evidence-type records FAIL/missing-evidence for a log dumped as PNG/JSON", () => {
    const outDir = tmp();
    const result = runQa(["--inject", "wrong-evidence-type", "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe("FAIL");
    expect(report.cases[0].reason).toBe("missing-evidence");
    expect(report.cases[0].detail).toMatch(/PNG|JSON/i);
  });

  it("inject missing-evidence records FAIL/missing-evidence and exits non-zero", () => {
    const outDir = tmp();
    const result = runQa(["--inject", "missing-evidence", "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe("FAIL");
    expect(report.cases[0].reason).toBe("missing-evidence");
  });

  it("inject cleanup-failure records FAIL/cleanup-failure and exits non-zero", () => {
    const outDir = tmp();
    const result = runQa(["--inject", "cleanup-failure", "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe("FAIL");
    expect(report.cases[0].reason).toBe("cleanup-failure");
  });
});

describe("pnpm qa script wiring", () => {
  it("package.json exposes qa as the shipped entry", () => {
    const pkg = JSON.parse(readFileSync(path.join(repo, "package.json"), "utf8"));
    expect(pkg.scripts.qa).toBe("node scripts/qa/run.mjs");
  });

  it("pnpm qa -- --lane banana fails the same way as the node entry", () => {
    const result = spawnSync("pnpm", ["qa", "--", "--lane", "banana"], {
      cwd: repo,
      encoding: "utf8",
      shell: true,
    });
    expect(result.status).toBe(2);
    expect(`${result.stdout}\n${result.stderr}`).toMatch(/unknown lane/);
  });
});
