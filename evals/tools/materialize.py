#!/usr/bin/env python3
"""Materialize public benchmark records without exposing hidden tests to the agent."""

from __future__ import annotations

import argparse
import ast
import gzip
import json
import shutil
from pathlib import Path


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def load_humaneval(path: Path, task_id: str) -> dict:
    with gzip.open(path, "rt", encoding="utf-8") as stream:
        for line in stream:
            row = json.loads(line)
            if str(row["task_id"]) == task_id:
                return row
    raise KeyError(task_id)


def load_mbpp(path: Path, task_id: str) -> dict:
    value = json.loads(path.read_text(encoding="utf-8"))
    rows = value if isinstance(value, list) else value.get("data") or value.get("tasks") or []
    for row in rows:
        if str(row["task_id"]) == task_id:
            return row
    raise KeyError(task_id)


def mbpp_public_stubs(row: dict, tests: list[str]) -> str:
    """Expose the benchmark's public API without exposing its implementation."""
    source = str(row.get("code") or "")
    try:
        module = ast.parse(source)
        tested_names = {
            node.func.id
            for test in tests
            for node in ast.walk(ast.parse(str(test)))
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
        }
        functions = [
            node
            for node in module.body
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name in tested_names
        ]
    except (SyntaxError, TypeError):
        functions = []

    stubs: list[str] = []
    for function in functions:
        prefix = "async def" if isinstance(function, ast.AsyncFunctionDef) else "def"
        arguments = ast.unparse(function.args)
        stubs.append(
            f"{prefix} {function.name}({arguments}):\n"
            '    """Implement this public benchmark API."""\n'
            "    raise NotImplementedError\n"
        )
    return "\n".join(stubs)


def humaneval(dataset_root: Path, task_id: str, output: Path) -> None:
    row = load_humaneval(dataset_root / "HumanEval.jsonl.gz", task_id)
    workspace = output / "workspace"
    hidden = output / "hidden"
    starter = row["prompt"].rstrip() + "\n    raise NotImplementedError\n"
    write(workspace / "solution.py", starter)
    write(
        workspace / "TASK.md",
        f"# {task_id}\n\nImplement the function documented in `solution.py`.\n"
        "Do not change the public function name or signature.\n",
    )
    grader = (
        "import os, sys\n"
        "sys.path.insert(0, os.environ.get('VELLUM_EVAL_WORKSPACE', '/workspace'))\n"
        "from solution import *\n\n"
        + row["test"].rstrip()
        + f"\n\ncheck({row['entry_point']})\nprint('VELLUM_EVAL_PASS')\n"
    )
    write(hidden / "grader.py", grader)


def mbpp(dataset_root: Path, task_id: str, output: Path, multi_file: bool) -> None:
    row = load_mbpp(dataset_root / "sanitized-mbpp.json", task_id)
    prompt = str(row.get("prompt") or row.get("text") or "")
    tests = row.get("test_list") or row.get("tests") or []
    if isinstance(tests, str):
        tests = [tests]
    public_stubs = mbpp_public_stubs(row, tests)
    workspace = output / "workspace"
    hidden = output / "hidden"
    if multi_file:
        write(workspace / "src" / "__init__.py", "from .solution import *\n")
        write(
            workspace / "src" / "solution.py",
            '"""Implement the public API described in TASK.md."""\n\n'
            + public_stubs
            + ("\n" if public_stubs else "")
            + "# You may add helpers.py and import it here.\n",
        )
        task_text = (
            f"# MBPP {task_id}\n\n{prompt.strip()}\n\n"
            "Implement the public API in `src/solution.py`. Keep reusable logic in "
            "`src/helpers.py` and preserve the exports from `src/__init__.py`.\n"
        )
        prefix = (
            "import os, sys\n"
            "sys.path.insert(0, os.environ.get('VELLUM_EVAL_WORKSPACE', '/workspace'))\n"
            "from src.solution import *\n\n"
        )
    else:
        write(
            workspace / "solution.py",
            '"""Implement the public API described in TASK.md."""\n\n'
            + public_stubs,
        )
        task_text = f"# MBPP {task_id}\n\n{prompt.strip()}\n"
        prefix = (
            "import os, sys\n"
            "sys.path.insert(0, os.environ.get('VELLUM_EVAL_WORKSPACE', '/workspace'))\n"
            "from solution import *\n\n"
        )
    write(workspace / "TASK.md", task_text)
    grader = prefix + "\n".join(str(test) for test in tests) + "\nprint('VELLUM_EVAL_PASS')\n"
    write(hidden / "grader.py", grader)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset-root", type=Path, required=True)
    parser.add_argument("--kind", choices=["humaneval", "mbpp"], required=True)
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--multi-file", action="store_true")
    args = parser.parse_args()
    if args.output.exists():
        shutil.rmtree(args.output)
    args.output.mkdir(parents=True)
    if args.kind == "humaneval":
        humaneval(args.dataset_root, args.task_id, args.output)
    else:
        mbpp(args.dataset_root, args.task_id, args.output, args.multi_file)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
