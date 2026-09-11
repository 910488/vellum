#!/usr/bin/env python3
"""Run and score one paired 12-case Enhanced Harness quick round."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
OUTPUT_ROOT = ROOT / "target" / "vellum-evals"
STATE_ROOT = OUTPUT_ROOT / "enhanced-campaign-state"
CONFIG: dict[str, dict[str, str]] = {
    "E1": {"candidate": "E1", "suite": "evals/manifests/enhanced-quick-e1.json", "target": "e1-exact-tool-replay", "event": "enhanced.tool.duplicate_suppressed"},
    "E2": {"candidate": "E2", "suite": "evals/manifests/enhanced-quick-e2.json", "target": "e2-overflow-retry", "event": "enhanced.context.overflow_retry"},
    "E3": {"candidate": "E3", "suite": "evals/manifests/enhanced-quick-e3.json", "target": "e3-pending-plan-stop", "event": "enhanced.continuation.allowed"},
}


def read_results(run_id: str) -> list[dict[str, Any]]:
    latest: dict[str, dict[str, Any]] = {}
    path = OUTPUT_ROOT / run_id / "results.jsonl"
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.strip():
            item = json.loads(line)
            latest[item["caseId"]] = item
    return list(latest.values())


def sha256(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def command(profile: str, suite: Path, models: list[str], seed: int, phase: str, module: str) -> list[str]:
    args = [
        "cargo", "run", "-p", "vellum-eval", "--", "matrix",
        "--suite", str(suite), "--models", ",".join(models),
        "--repeat", "3" if phase == "holdout" else "1",
        "--seed", str(seed), "--schedule", "round-robin",
        "--executor", "windows-sandbox", "--ablation-profile", profile,
        "--compaction-engine", "codex-local-v0-150",
        "--max-wall-seconds", "5400" if phase == "holdout" else "1800",
        "--max-total-tokens", "1500000" if phase == "holdout" else "500000",
    ]
    if phase == "holdout":
        args.extend(["--tag", f"holdout-{module}"])
    return args


def execute_arm(profile: str, suite: Path, models: list[str], seed: int, binary: Path, phase: str, module: str) -> str:
    before = {path.name for path in OUTPUT_ROOT.iterdir() if path.is_dir()}
    env = os.environ.copy()
    env["VELLUM_ENHANCED_CODEX"] = str(binary.resolve())
    subprocess.run(command(profile, suite, models, seed, phase, module), cwd=ROOT, env=env, check=True)
    candidates = [path for path in OUTPUT_ROOT.iterdir() if path.is_dir() and path.name not in before and (path / "run.json").is_file()]
    if len(candidates) != 1:
        raise RuntimeError(f"expected one new eval run, found {[p.name for p in candidates]}")
    return candidates[0].name


def prepare_suite(suite: Path) -> None:
    subprocess.run(
        ["cargo", "run", "-p", "vellum-eval", "--", "prepare", "--suite", str(suite)],
        cwd=ROOT,
        check=True,
    )


def median(values: list[float]) -> float:
    ordered = sorted(values)
    if not ordered:
        return 0.0
    middle = len(ordered) // 2
    return ordered[middle] if len(ordered) % 2 else (ordered[middle - 1] + ordered[middle]) / 2


def usage(result: dict[str, Any]) -> tuple[int, int]:
    metrics = result.get("metrics", {})
    return (
        int(metrics.get("inputTokens", 0)) + int(metrics.get("outputTokens", 0)),
        int(metrics.get("cachedTokens", 0)),
    )


def fully_qualified(result: dict[str, Any]) -> bool:
    layers = result.get("layers", {})
    return all((
        result.get("passed", False),
        result.get("taskCorrectnessPassed", False),
        result.get("agentCompleted", False),
        result.get("protocolQualified", False),
        result.get("performanceQualified", False),
        layers.get("runtimeAttribution", False),
        layers.get("mechanismExercised", False),
        not result.get("harnessExpectationMisses"),
        not result.get("protocolViolations"),
    ))


def unique_report_path(path: Path) -> Path:
    if not path.exists():
        return path
    for version in range(2, 1000):
        candidate = path.with_name(f"{path.stem}-revision-{version}{path.suffix}")
        if not candidate.exists():
            return candidate
    raise RuntimeError(f"could not allocate versioned report path beside {path}")


def write_campaign_state(path: Path, state: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(state, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def score(module: str, baseline_run: str, candidate_run: str, target_override: str | None = None, expected_cases: int = 6) -> dict[str, Any]:
    cfg = CONFIG[module]
    arms = {"baseline": read_results(baseline_run), "candidate": read_results(candidate_run)}
    failures: list[str] = []
    for arm, expected_profile in (("baseline", "E0"), ("candidate", cfg["candidate"])):
        results = arms[arm]
        if len(results) != expected_cases:
            failures.append(f"{arm}: expected {expected_cases} cases, observed {len(results)}")
        for result in results:
            runtime = result.get("metrics", {}).get("enhancedRuntime") or {}
            if runtime.get("featureProfile") != expected_profile:
                failures.append(
                    f"{arm}:{result.get('caseId')}: observed profile "
                    f"{runtime.get('featureProfile')!r}, expected {expected_profile}"
                )
            if not result.get("layers", {}).get("runtimeAttribution"):
                failures.append(f"{arm}:{result.get('caseId')}: runtime attribution failed")

    def keyed(rows: list[dict[str, Any]]) -> dict[tuple[str, str, int], dict[str, Any]]:
        return {(r["taskId"], r["model"], r["repetition"]): r for r in rows}

    base, cand = keyed(arms["baseline"]), keyed(arms["candidate"])
    if set(base) != set(cand):
        failures.append("paired case keys differ between arms")
    pairs: list[dict[str, Any]] = []
    target_event_total = 0
    control_cost_by_model: dict[str, dict[str, list[float]]] = {
        model: {"tokens": [], "duration": []} for model in {key[1] for key in base}
    }
    conversions = 0
    regressions = 0
    both_success_target_cost_improvements: list[float] = []
    for key in sorted(set(base) & set(cand)):
        b, c = base[key], cand[key]
        target = key[0] == (target_override or cfg["target"])
        event_count = c.get("metrics", {}).get("enhancedEventCounts", {}).get(cfg["event"], 0)
        b_qualified, c_qualified = fully_qualified(b), fully_qualified(c)
        b_tokens, b_cached = usage(b)
        c_tokens, c_cached = usage(c)
        conversions += int(not b_qualified and c_qualified)
        regressions += int(b_qualified and not c_qualified)
        if target:
            target_event_total += event_count
            if not c_qualified:
                failures.append(f"candidate target did not fully qualify: {key}")
            if event_count < 1:
                failures.append(f"candidate target did not emit {cfg['event']}: {key}")
            if b_qualified and c_qualified and b_tokens:
                both_success_target_cost_improvements.append((b_tokens - c_tokens) * 100.0 / b_tokens)
        else:
            if b_qualified and not c_qualified:
                failures.append(f"control qualification regression: {key}")
            if b_qualified and c_qualified:
                if b_tokens:
                    control_cost_by_model[key[1]]["tokens"].append((c_tokens - b_tokens) * 100.0 / b_tokens)
                if b.get("durationMs"):
                    control_cost_by_model[key[1]]["duration"].append(
                        (c["durationMs"] - b["durationMs"]) * 100.0 / b["durationMs"]
                    )
        if not c.get("protocolQualified"):
            failures.append(f"candidate protocol qualification failed: {key}")
        pairs.append({
            "taskId": key[0], "model": key[1], "repetition": key[2], "target": target,
            "baselineCorrect": b.get("taskCorrectnessPassed", False),
            "candidateCorrect": c.get("taskCorrectnessPassed", False),
            "baselineQualified": b_qualified, "candidateQualified": c_qualified,
            "candidateMechanismEvents": event_count,
            "baselineTokens": b_tokens, "candidateTokens": c_tokens,
            "baselineCachedTokens": b_cached, "candidateCachedTokens": c_cached,
            "baselineDurationMs": b.get("durationMs", 0),
            "candidateDurationMs": c.get("durationMs", 0),
            "candidateHttpFaults": c.get("httpFaults", []),
        })
    control_cost = {}
    for model, values in sorted(control_cost_by_model.items()):
        token_delta = median(values["tokens"])
        duration_delta = median(values["duration"])
        control_cost[model] = {
            "successfulPairCount": len(values["tokens"]),
            "medianTokenDeltaPercent": token_delta,
            "medianDurationDeltaPercent": duration_delta,
        }
        if token_delta > 10:
            failures.append(f"{model}: control median token regression {token_delta:.2f}% exceeds 10%")
        if duration_delta > 20:
            failures.append(f"{model}: control median duration regression {duration_delta:.2f}% exceeds 20%")
    target_cost_improvement = median(both_success_target_cost_improvements)
    net_benefit = conversions > 0 or (
        bool(both_success_target_cost_improvements) and target_cost_improvement >= 15
    )
    if not net_benefit:
        failures.append("no E0-fail to candidate-pass conversion or >=15% successful target cost improvement")
    return {
        "schemaVersion": 2, "scoringVersion": "enhanced-paired-v2", "module": module, "baselineRun": baseline_run,
        "candidateRun": candidate_run, "expectedMechanismEvent": cfg["event"],
        "targetMechanismEventCount": target_event_total,
        "successConversions": conversions, "regressions": regressions,
        "targetBothSuccessMedianCostImprovementPercent": target_cost_improvement,
        "controlCostByModel": control_cost,
        "sampleSize": len(pairs),
        "passed": not failures, "failures": failures, "pairs": pairs,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--module", choices=sorted(CONFIG), required=True)
    parser.add_argument("--round", type=int, required=True)
    parser.add_argument("--phase", choices=("quick", "holdout"), default="quick")
    parser.add_argument("--models", default="vlm-beb60d2887-qwen,vlm-c1f84e8c6d-omen-alpha")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--baseline-run")
    parser.add_argument("--candidate-run")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if not 1 <= args.round <= 3:
        parser.error("--round must be 1, 2, or 3")
    models = [value.strip() for value in args.models.split(",") if value.strip()]
    if len(models) != 2:
        parser.error("--models must contain exactly two model ids")
    cfg = CONFIG[args.module]
    suite = ROOT / ("evals/manifests/enhanced-holdout-pool.json" if args.phase == "holdout" else cfg["suite"])
    seed = int(
        hashlib.sha256(f"{args.module}:{args.phase}:{args.round}".encode()).hexdigest()[:8],
        16,
    )
    order = ["E0", cfg["candidate"]] if args.round % 2 else [cfg["candidate"], "E0"]
    if args.dry_run:
        for profile in order:
            print(subprocess.list2cmdline(command(profile, suite, models, seed, args.phase, args.module)))
        return 0

    state_path = STATE_ROOT / f"{args.module.lower()}-{args.phase}-round-{args.round}.json"
    state: dict[str, Any] = {
        "schemaVersion": 1,
        "pid": os.getpid(),
        "module": args.module,
        "phase": args.phase,
        "round": args.round,
        "status": "preparing",
        "exitCode": None,
        "gitCommit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "suiteSha256": sha256(suite),
        "runIds": {},
    }
    if args.binary and args.binary.is_file():
        state["enhancedBinarySha256"] = sha256(args.binary)
    write_campaign_state(state_path, state)
    try:
        baseline_run, candidate_run = args.baseline_run, args.candidate_run
        if not (baseline_run and candidate_run):
            if not args.binary or not args.binary.is_file():
                parser.error("--binary is required for execution and must exist")
            prepare_suite(suite)
            observed: dict[str, str] = {}
            for profile in order:
                state["status"] = f"running:{profile}"
                write_campaign_state(state_path, state)
                observed[profile] = execute_arm(profile, suite, models, seed, args.binary, args.phase, args.module)
                state["runIds"][profile] = observed[profile]
                write_campaign_state(state_path, state)
            baseline_run, candidate_run = observed["E0"], observed[cfg["candidate"]]
        else:
            state["runIds"] = {"E0": baseline_run, cfg["candidate"]: candidate_run}

        state["status"] = "scoring"
        write_campaign_state(state_path, state)
        target_override = f"holdout-{args.module.lower()}-" + {
            "E1": "replayed-charge", "E2": "overflow-normalize", "E3": "pending-product"
        }[args.module] if args.phase == "holdout" else None
        report = score(
            args.module, baseline_run, candidate_run, target_override,
            expected_cases=18 if args.phase == "holdout" else 6,
        )
        report.update({
            "round": args.round, "phase": args.phase, "seed": seed, "pairOrder": order,
            "suiteSha256": state["suiteSha256"], "gitCommit": state["gitCommit"],
        })
        if "enhancedBinarySha256" in state:
            report["enhancedBinarySha256"] = state["enhancedBinarySha256"]
        output = args.report or unique_report_path(
            OUTPUT_ROOT / candidate_run / f"enhanced-quick-{args.module.lower()}-round-{args.round}-score-v2.json"
        )
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        exit_code = 0 if report["passed"] else 1
        state.update({"status": "complete" if exit_code == 0 else "gate_failed", "exitCode": exit_code, "report": str(output)})
        write_campaign_state(state_path, state)
        print(output)
        print("PASS" if report["passed"] else "FAIL")
        return exit_code
    except BaseException as error:
        state.update({"status": "failed", "exitCode": 1, "error": f"{type(error).__name__}: {error}"})
        write_campaign_state(state_path, state)
        raise


if __name__ == "__main__":
    raise SystemExit(main())
