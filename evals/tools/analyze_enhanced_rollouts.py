#!/usr/bin/env python3
"""Content-free Enhanced Harness evidence extractor.

The report deliberately contains counts, hashes, byte sizes, and rollout line
numbers only. Prompts, tool arguments, outputs, and final answers never leave
the source Codex home.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import hashlib
import json
import sqlite3
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Iterable


@dataclass
class ModelEvidence:
    threads: int = 0
    turns: int = 0
    tool_calls: int = 0
    same_call_id_replays: int = 0
    identical_call_streaks: int = 0
    tool_outputs: int = 0
    text_output_bytes: int = 0
    text_outputs_over_8k: int = 0
    text_outputs_over_32k: int = 0
    image_outputs: int = 0
    compactions: int = 0
    plan_updates: int = 0
    natural_stops_with_pending_plan: int = 0
    findings: list[dict[str, Any]] = field(default_factory=list)


def sha256(value: str) -> str:
    return "sha256:" + hashlib.sha256(value.encode("utf-8")).hexdigest()


def parse_time(value: str) -> int:
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return int(parsed.timestamp())


def json_lines(path: Path) -> Iterable[tuple[int, dict[str, Any]]]:
    with path.open("r", encoding="utf-8") as source:
        for line_number, line in enumerate(source, start=1):
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                yield line_number, value


def output_shape(payload: dict[str, Any]) -> tuple[int, bool]:
    output = payload.get("output", "")
    if isinstance(output, str):
        return len(output.encode("utf-8")), False
    encoded = json.dumps(output, ensure_ascii=False, separators=(",", ":"))
    lowered = encoded[:2048].lower()
    image = any(marker in lowered for marker in ("image_url", '"type":"image"', "data:image/"))
    return len(encoded.encode("utf-8")), image


def canonical_call(payload: dict[str, Any]) -> tuple[str, str]:
    name = str(payload.get("name", ""))
    arguments = payload.get("arguments", payload.get("input", ""))
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except json.JSONDecodeError:
            pass
    canonical = json.dumps(arguments, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return name, sha256(canonical)


def analyze_rollout(path: Path, initial_model: str, evidence: dict[str, ModelEvidence]) -> None:
    model = initial_model
    seen_ids: set[str] = set()
    previous_call: tuple[str, str] | None = None
    streak = 0
    pending_plan: bool | None = None
    active = evidence[model]
    active.threads += 1

    for line_number, item in json_lines(path):
        kind = item.get("type")
        payload = item.get("payload") if isinstance(item.get("payload"), dict) else {}
        payload_type = payload.get("type")
        if kind == "turn_context":
            candidate = payload.get("model")
            if isinstance(candidate, str) and candidate.startswith("vlm-"):
                model = candidate
                active = evidence[model]
            active.turns += 1
            previous_call = None
            streak = 0
            pending_plan = None
            continue
        if not model.startswith("vlm-"):
            continue
        if kind == "compacted":
            active.compactions += 1
            continue
        if kind == "response_item" and payload_type in ("function_call", "custom_tool_call"):
            active.tool_calls += 1
            call_id = str(payload.get("call_id", ""))
            if call_id and call_id in seen_ids:
                active.same_call_id_replays += 1
                active.findings.append({"kind": "same_call_id", "line": line_number})
            if call_id:
                seen_ids.add(call_id)
            call_key = canonical_call(payload)
            streak = streak + 1 if call_key == previous_call else 1
            previous_call = call_key
            if streak == 3:
                active.identical_call_streaks += 1
                active.findings.append(
                    {"kind": "identical_call_streak", "line": line_number, "tool": call_key[0]}
                )
            if call_key[0].endswith("update_plan"):
                active.plan_updates += 1
                arguments = payload.get("arguments", "")
                try:
                    decoded = json.loads(arguments) if isinstance(arguments, str) else arguments
                except json.JSONDecodeError:
                    decoded = {}
                pending_plan = any(
                    row.get("status") in ("pending", "in_progress")
                    for row in decoded.get("plan", [])
                    if isinstance(row, dict)
                )
            continue
        if kind == "response_item" and payload_type in (
            "function_call_output",
            "custom_tool_call_output",
        ):
            active.tool_outputs += 1
            size, image = output_shape(payload)
            if image:
                active.image_outputs += 1
                continue
            active.text_output_bytes += size
            active.text_outputs_over_8k += int(size > 8 * 1024)
            active.text_outputs_over_32k += int(size > 32 * 1024)
            if size > 32 * 1024:
                active.findings.append({"kind": "large_text_output", "line": line_number, "bytes": size})
            continue
        if kind == "event_msg" and payload_type == "task_complete" and pending_plan:
            active.natural_stops_with_pending_plan += 1
            active.findings.append({"kind": "pending_plan_at_stop", "line": line_number})


def analyze(codex_home: Path, since: int, model_prefixes: tuple[str, ...]) -> dict[str, Any]:
    state = codex_home / "state_5.sqlite"
    connection = sqlite3.connect(f"file:{state.as_posix()}?mode=ro", uri=True)
    rows = connection.execute(
        "SELECT id, model, rollout_path FROM threads WHERE updated_at >= ? ORDER BY updated_at",
        (since,),
    ).fetchall()
    connection.close()
    evidence: dict[str, ModelEvidence] = collections.defaultdict(ModelEvidence)
    visited: set[Path] = set()
    for _thread_id, model, rollout_path in rows:
        path = Path(rollout_path)
        if path in visited or not path.is_file():
            continue
        visited.add(path)
        initial_model = model or ""
        if model_prefixes and not any(initial_model.startswith(prefix) for prefix in model_prefixes):
            # A rollout may switch into a requested model, so still inspect it.
            pass
        analyze_rollout(path, initial_model, evidence)
    selected = {
        model: asdict(row)
        for model, row in sorted(evidence.items())
        if model.startswith("vlm-")
        and (not model_prefixes or any(model.startswith(prefix) for prefix in model_prefixes))
    }
    return {
        "schemaVersion": 1,
        "source": "content-free-codex-rollout-analysis",
        "sinceEpochSeconds": since,
        "rolloutsInspected": len(visited),
        "models": selected,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex-home", type=Path, required=True)
    parser.add_argument("--since", required=True, help="ISO-8601 timestamp")
    parser.add_argument("--model-prefix", action="append", default=[])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = analyze(args.codex_home, parse_time(args.since), tuple(args.model_prefix))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
