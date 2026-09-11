import { writeFileSync } from "node:fs";
import path from "node:path";
import { runProcess } from "./commands.mjs";
import { writeLog } from "./execute.mjs";
import { assertionAfterAction, candidateGateIsNotLiveTask } from "./drivers.mjs";
import { assertPinnedModel } from "./models.mjs";
import { refuseNewestMtimePicker } from "./installer.mjs";

export function injectSpecs(kind, outDir) {
  if (kind === "timeout") {
    return [
      {
        id: "inject.timeout",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        timeoutMs: 250,
        evidence: ["logs/timeout.txt"],
        async run({ evidenceDir }) {
          writeLog(evidenceDir, "logs/timeout.txt", "starting sleep that should time out\n");
          const result = await runProcess(
            ["node", "-e", "setTimeout(() => {}, 30_000)"],
            { timeoutMs: 200, shell: false },
          );
          if (result.timedOut) {
            const error = new Error("timeout after 250ms");
            error.code = "TIMEOUT";
            throw error;
          }
          return { detail: "process exited before timeout", verdict: "FAIL", reason: "error" };
        },
      },
    ];
  }

  if (kind === "budget-stop") {
    return [
      {
        id: "inject.budget-stop",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        model: "grok-4.6",
        estimate: 50_000,
        evidence: ["logs/budget.txt"],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "logs/budget.txt", "should not run\n");
          return { verdict: "PASS", detail: "command ran despite empty budget" };
        },
      },
    ];
  }

  if (kind === "missing-credentials") {
    return [
      {
        id: "inject.missing-credentials",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        requiresCredentials: ["VELLUM_QA_INJECT_TOKEN"],
        evidence: ["logs/creds.txt"],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "logs/creds.txt", "should not run without credentials\n");
          return { verdict: "PASS" };
        },
      },
    ];
  }

  if (kind === "missing-evidence") {
    return [
      {
        id: "inject.missing-evidence",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["must-exist.txt"],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "logs/ran.txt", "ran without writing must-exist.txt\n");
          return { verdict: "PASS", detail: "intentionally omitted required evidence" };
        },
      },
    ];
  }

  if (kind === "wrong-evidence-type") {
    return [
      {
        id: "inject.wrong-evidence-type",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["screenshots/shot.png", "status.json"],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "screenshots/shot.png", "==> Build Linux amd64 Agent\n");
          writeLog(evidenceDir, "status.json", "not json at all\n");
          return { verdict: "PASS", detail: "wrote a log dump where a PNG and JSON were required" };
        },
      },
    ];
  }

  if (kind === "png-header-only") {
    return [
      {
        id: "inject.png-header-only",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["screenshots/shot.png"],
        run({ evidenceDir }) {
          const full = path.join(evidenceDir, "screenshots/shot.png");
          writeLog(evidenceDir, "screenshots/.keep", "");
          writeFileSync(full, Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
          return { verdict: "PASS", detail: "wrote PNG magic without image payload" };
        },
      },
    ];
  }

  if (kind === "json-semantic-fail") {
    return [
      {
        id: "inject.json-semantic-fail",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["status.json"],
        jsonFile: "status.json",
        jsonKeys: ["proxyRunning"],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "status.json", `${JSON.stringify({ ok: true })}\n`);
          return { verdict: "PASS", detail: "JSON parses but is missing proxyRunning" };
        },
      },
    ];
  }

  if (kind === "stale-evidence") {
    return [
      {
        id: "inject.stale-evidence",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["events.json"],
        runStartedAt: new Date(Date.now() + 60_000).toISOString(),
        run({ evidenceDir }) {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ observedAt: "2020-01-01T00:00:00.000Z" }),
          );
          return { verdict: "PASS", detail: "evidence timestamp is before the run" };
        },
      },
    ];
  }

  if (kind === "wrong-model") {
    return [
      {
        id: "inject.wrong-model",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["events.json"],
        run({ evidenceDir }) {
          const check = assertPinnedModel("gpt-4o-mini", "opencode-mimo-2.5");
          writeLog(evidenceDir, "events.json", JSON.stringify(check));
          if (!check.ok) return { verdict: "FAIL", reason: check.reason, detail: check.detail };
          return { verdict: "PASS" };
        },
      },
    ];
  }

  if (kind === "button-state-unchanged") {
    return [
      {
        id: "inject.button-state-unchanged",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["events.json"],
        run({ evidenceDir }) {
          const check = assertionAfterAction({
            before: { proxyRunning: false },
            after: { proxyRunning: false },
            verify: { kind: "field-changed", field: "proxyRunning" },
          });
          writeLog(evidenceDir, "events.json", JSON.stringify(check));
          return {
            verdict: check.ok ? "PASS" : "FAIL",
            reason: check.reason,
            detail: check.detail,
          };
        },
      },
    ];
  }

  if (kind === "candidate-as-live") {
    return [
      {
        id: "inject.candidate-as-live",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["events.json"],
        run({ evidenceDir }) {
          const impersonating = candidateGateIsNotLiveTask("candidate-provider-gate");
          writeLog(evidenceDir, "events.json", JSON.stringify({ impersonating }));
          return impersonating
            ? {
                verdict: "NOT_RUN",
                reason: "case-driver-unavailable",
                detail: "candidate provider gate must not count as a real Enhanced/Desktop task",
              }
            : { verdict: "PASS" };
        },
      },
    ];
  }

  if (kind === "wrong-installer") {
    return [
      {
        id: "inject.wrong-installer",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["events.json"],
        run({ evidenceDir }) {
          const picker = refuseNewestMtimePicker();
          writeLog(evidenceDir, "events.json", JSON.stringify(picker));
          return {
            verdict: "FAIL",
            reason: "wrong-installer",
            detail: picker.reason,
          };
        },
      },
    ];
  }

  if (kind === "cleanup-failure") {
    return [
      {
        id: "inject.cleanup-failure",
        domain: "runner",
        surface: "runner",
        lane: "inject",
        necessary: true,
        evidence: ["logs/cleanup.txt"],
        cleanupHooks: [
          async () => {
            throw new Error(`cleanup exploded for ${outDir}`);
          },
        ],
        run({ evidenceDir }) {
          writeLog(evidenceDir, "logs/cleanup.txt", "main step ok\n");
          writeFileSync(path.join(evidenceDir, "ok.txt"), "ok\n");
          return { verdict: "PASS", detail: "main step ok" };
        },
      },
    ];
  }

  throw new Error(`unknown inject ${kind}`);
}

export function injectBudgetSeed(kind, budget) {
  if (kind === "budget-stop") {
    budget.record("grok-4.6", { total: 100_000 });
    budget.stop("grok-4.6", "budget-stop");
  }
}
