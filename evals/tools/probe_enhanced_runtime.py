#!/usr/bin/env python3
"""Probe a built Enhanced Codex App Server for observed session features."""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import tempfile
import threading
import time
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--profile", default="E5")
    parser.add_argument("--commit", required=True)
    parser.add_argument("--runtime-digest", required=True)
    args = parser.parse_args()

    env = os.environ.copy()
    env.update(
        {
            "VELLUM_EXECUTION_PLANE": "enhanced-codex",
            "VELLUM_RUNTIME_DIGEST": args.runtime_digest,
            "VELLUM_ENHANCED_COMMIT": args.commit,
            "VELLUM_ENHANCED_ABLATION_PROFILE": args.profile,
            "NO_COLOR": "1",
        }
    )
    with tempfile.TemporaryDirectory(prefix="vellum-enhanced-probe-") as home:
        env["CODEX_HOME"] = home
        event_log = Path(home) / "enhanced-events.jsonl"
        env["VELLUM_ENHANCED_EVENT_LOG"] = str(event_log.resolve())
        process = subprocess.Popen(
            [str(args.binary.resolve()), "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
        )
        assert process.stdin and process.stdout
        messages: queue.Queue[dict[str, object]] = queue.Queue()

        def read_stdout() -> None:
            for line in process.stdout:
                try:
                    messages.put(json.loads(line))
                except json.JSONDecodeError:
                    continue

        threading.Thread(target=read_stdout, daemon=True).start()

        def send(value: dict[str, object]) -> None:
            process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
            process.stdin.flush()

        send(
            {
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "vellum-runtime-probe",
                        "title": "Vellum Runtime Probe",
                        "version": "1",
                    }
                },
            }
        )
        send({"method": "initialized"})
        send({"id": 2, "method": "thread/start", "params": {"model": "qwen"}})

        observed: list[dict[str, object]] = []
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                message = messages.get(timeout=0.25)
            except queue.Empty:
                if process.poll() is not None:
                    break
                continue
            method = message.get("method")
            if method in {"vellum/enhancedRuntimeIdentity", "vellum/enhancedEvent"}:
                observed.append(message)
            if any(
                item.get("method") == "vellum/enhancedEvent"
                and isinstance(item.get("params"), dict)
                and item["params"].get("name") == "enhanced.session.features_applied"
                for item in observed
            ):
                break

        process.stdin.close()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=5)

        journal: list[dict[str, object]] = []
        if event_log.exists():
            for line in event_log.read_text(encoding="utf-8").splitlines():
                try:
                    journal.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

    identity = next(
        (item for item in observed if item.get("method") == "vellum/enhancedRuntimeIdentity"),
        None,
    )
    applied = next(
        (
            item
            for item in observed
            if item.get("method") == "vellum/enhancedEvent"
            and isinstance(item.get("params"), dict)
            and item["params"].get("name") == "enhanced.session.features_applied"
        ),
        None,
    )
    journal_identity = next(
        (item for item in journal if item.get("method") == "vellum/enhancedRuntimeIdentity"),
        None,
    )
    journal_applied = next(
        (
            item
            for item in journal
            if item.get("method") == "vellum/enhancedEvent"
            and isinstance(item.get("params"), dict)
            and item["params"].get("name") == "enhanced.session.features_applied"
        ),
        None,
    )
    report = {
        "identity": identity,
        "sessionFeaturesApplied": applied,
        "journalIdentity": journal_identity,
        "journalSessionFeaturesApplied": journal_applied,
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if identity and applied and journal_identity and journal_applied else 1


if __name__ == "__main__":
    raise SystemExit(main())
