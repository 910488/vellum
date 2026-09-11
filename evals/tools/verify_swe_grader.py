#!/usr/bin/env python3
"""Prove one SWE-bench grader: unpatched FAIL, gold PASS, no network.

The official hidden eval script is mounted at /hidden and is not present in
the workspace. The gold patch stays outside both mounts. The container runs
with --network none.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import types

import pandas as pd


def run(
    args: list[str], timeout: int, cwd: Path | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=str(cwd) if cwd else None,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )


def load_eval_script(record: dict) -> str:
    if os.name == "nt" and "resource" not in sys.modules:
        resource = types.ModuleType("resource")
        resource.RLIMIT_NOFILE = 7
        resource.setrlimit = lambda *_args, **_kwargs: None
        sys.modules["resource"] = resource
    from swebench.harness.test_spec.test_spec import make_test_spec

    return make_test_spec(record).eval_script.replace("\r\n", "\n").replace("\r", "\n")


def copy_testbed(image: str, destination: Path) -> None:
    container = subprocess.run(
        ["docker", "create", image, "/bin/true"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ).stdout.strip()
    try:
        destination.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["docker", "cp", f"{container}:/testbed/.", str(destination)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
    finally:
        subprocess.run(["docker", "rm", "-f", container], check=False)


def docker_path(path: Path) -> str:
    resolved = path.resolve()
    if os.name != "nt":
        return str(resolved)
    return resolved.as_posix()


def parse_pytest_statuses(output: str) -> dict[str, str]:
    statuses: dict[str, str] = {}
    for line in output.splitlines():
        if line.startswith("PASSED ") or line.startswith("FAILED "):
            status, rest = line.split(" ", 1)
            name = rest.split(" ", 1)[0].strip()
            if name:
                statuses[name] = status
    return statuses


def fail_to_pass_names(record: dict) -> list[str]:
    raw = record.get("FAIL_TO_PASS", [])
    if isinstance(raw, str):
        raw = json.loads(raw)
    return [str(item) for item in raw]


def require_f2p(statuses: dict[str, str], names: list[str], expect_pass: bool) -> None:
    if not names:
        raise SystemExit("record has no FAIL_TO_PASS tests")
    missing = [name for name in names if name not in statuses]
    if missing:
        raise SystemExit(f"hidden grader did not report FAIL_TO_PASS tests: {missing}")
    if expect_pass:
        failed = [name for name in names if statuses[name] != "PASSED"]
        if failed:
            raise SystemExit(f"gold FAIL_TO_PASS still failing: {failed}")
        return
    passed = [name for name in names if statuses[name] == "PASSED"]
    if passed:
        raise SystemExit(f"unpatched baseline unexpectedly passed FAIL_TO_PASS: {passed}")


def grade(image: str, workspace: Path, hidden: Path, timeout: int) -> str:
    gold = workspace / "gold.patch"
    if gold.exists():
        raise SystemExit("gold.patch leaked into the workspace")
    if (workspace / "hidden").exists() or (workspace / "eval.sh").exists():
        raise SystemExit("hidden grader leaked into the workspace")
    completed = run(
        [
            "docker",
            "run",
            "--rm",
            "--network",
            "none",
            "--platform",
            "linux/amd64",
            "-v",
            f"{docker_path(workspace)}:/testbed",
            "-v",
            f"{docker_path(hidden)}:/hidden:ro",
            "-w",
            "/testbed",
            image,
            "bash",
            "/hidden/eval.sh",
        ],
        timeout=timeout,
    )
    print(completed.stdout[-4000:])
    return completed.stdout


def apply_gold(workspace: Path, patch: str) -> None:
    patch_file = workspace.parent / "gold.patch"
    patch_file.write_text(patch.replace("\r\n", "\n"), encoding="utf-8", newline="\n")
    completed = run(
        ["git", "-C", str(workspace), "apply", "--verbose", str(patch_file)],
        timeout=60,
    )
    if completed.returncode != 0:
        completed = run(
            ["git", "-C", str(workspace), "apply", "--verbose", "-p1", str(patch_file)],
            timeout=60,
        )
    if completed.returncode != 0:
        raise SystemExit(f"gold patch failed to apply:\n{completed.stdout}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--instance", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()

    frame = pd.read_parquet(args.dataset).set_index("instance_id", drop=False)
    record = frame.loc[args.instance].to_dict()
    f2p = fail_to_pass_names(record)
    eval_script = load_eval_script(record)
    eval_script = eval_script.replace(
        "python -m pip install .", "python -m pip install --no-build-isolation ."
    )

    with tempfile.TemporaryDirectory(prefix="vellum-swe-verify-") as temporary:
        root = Path(temporary)
        hidden = root / "hidden"
        hidden.mkdir()
        (hidden / "eval.sh").write_bytes(eval_script.encode("utf-8"))

        baseline = root / "baseline"
        copy_testbed(args.image, baseline)
        print("== baseline (unpatched) ==")
        baseline_out = grade(args.image, baseline, hidden, args.timeout)
        require_f2p(parse_pytest_statuses(baseline_out), f2p, expect_pass=False)
        print("baseline FAIL on FAIL_TO_PASS")

        gold_ws = root / "gold"
        shutil.copytree(baseline, gold_ws, symlinks=True)
        apply_gold(gold_ws, str(record["patch"]))
        print("== gold (official patch) ==")
        gold_out = grade(args.image, gold_ws, hidden, args.timeout)
        require_f2p(parse_pytest_statuses(gold_out), f2p, expect_pass=True)
        print("gold PASS on FAIL_TO_PASS")


if __name__ == "__main__":
    main()
