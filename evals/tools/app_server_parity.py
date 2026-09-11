"""Drive one deterministic Codex app-server turn for Desktop parity capture.

The caller supplies an ephemeral Vellum gateway URL and one-time token. No
Provider credential or user CODEX_HOME enters this process.
"""

import argparse
import json
import os
import subprocess
import time
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--catalog", required=True)
    parser.add_argument("--codex-home", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--base-url", required=True)
    args = parser.parse_args()

    codex_home = Path(args.codex_home)
    workspace = Path(args.workspace)
    codex_home.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "PARITY_MARKER.txt").write_text(
        "VELLUM_DESKTOP_PARITY_MARKER\n", encoding="utf-8"
    )

    env = os.environ.copy()
    env["CODEX_HOME"] = str(codex_home)
    token = env.pop("VELLUM_EVAL_TOKEN", None)
    if not token:
        raise RuntimeError("VELLUM_EVAL_TOKEN is required")
    env["OPENAI_API_KEY"] = token
    command = [
        args.codex,
        "app-server",
        "--stdio",
        "--strict-config",
        "-c",
        'model_provider="vellum_eval"',
        "-c",
        'model_providers.vellum_eval.name="OpenAI"',
        "-c",
        f'model_providers.vellum_eval.base_url="{args.base_url}"',
        "-c",
        'model_providers.vellum_eval.env_key="OPENAI_API_KEY"',
        "-c",
        'model_providers.vellum_eval.wire_api="responses"',
        "-c",
        f'model_catalog_json="{Path(args.catalog).as_posix()}"',
    ]
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        env=env,
    )

    def send(value: dict) -> None:
        assert process.stdin is not None
        process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
        process.stdin.flush()

    def receive_until(predicate, timeout: float = 180.0):
        assert process.stdout is not None
        deadline = time.monotonic() + timeout
        observed = []
        while time.monotonic() < deadline:
            line = process.stdout.readline()
            if not line:
                if process.poll() is not None:
                    stderr = process.stderr.read() if process.stderr else ""
                    raise RuntimeError(
                        f"app-server exited {process.returncode}: {stderr}"
                    )
                continue
            value = json.loads(line)
            observed.append(value)
            if predicate(value):
                return value, observed
        raise TimeoutError("app-server response timeout")

    try:
        send(
            {
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "vellum-eval",
                        "title": "Vellum Desktop parity",
                        "version": "1",
                    }
                },
            }
        )
        receive_until(lambda value: value.get("id") == 1)
        send({"method": "initialized", "params": {}})
        send(
            {
                "id": 2,
                "method": "thread/start",
                "params": {
                    "model": args.model,
                    "modelProvider": "vellum_eval",
                    "cwd": str(workspace),
                    "approvalPolicy": "never",
                    "sandbox": "workspace-write",
                    "ephemeral": True,
                },
            }
        )
        thread_response, _ = receive_until(lambda value: value.get("id") == 2)
        if "result" not in thread_response:
            raise RuntimeError(f"thread/start failed: {thread_response}")
        thread_id = thread_response["result"]["thread"]["id"]
        send(
            {
                "id": 3,
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [
                        {
                            "type": "text",
                            "text": (
                                "Use the native shell tool exactly once to read "
                                "PARITY_MARKER.txt, then reply with only its value."
                            ),
                        }
                    ],
                },
            }
        )
        _, events = receive_until(
            lambda value: value.get("method") == "turn/completed"
        )
        print(
            json.dumps(
                {
                    "ok": True,
                    "threadId": thread_id,
                    "eventMethods": [
                        value.get("method")
                        for value in events
                        if value.get("method")
                    ],
                },
                ensure_ascii=False,
            )
        )
        return 0
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


if __name__ == "__main__":
    raise SystemExit(main())
