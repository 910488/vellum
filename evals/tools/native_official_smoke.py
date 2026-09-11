"""Run one official-model turn over the native Codex daemon control API.

This uses the target host's existing CODEX_HOME, account grant, injected model
catalog and loopback proxy. It deliberately connects to the same durable daemon
observed by Codex App instead of starting a competing app-server process.
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import time
from pathlib import Path


class ControlWebSocket:
    def __init__(self, path: Path, timeout: float):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(min(timeout, 5.0))
        self.socket.connect(str(path))
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            "GET / HTTP/1.1\r\n"
            "Host: localhost\r\n"
            "Connection: Upgrade\r\n"
            "Upgrade: websocket\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Key: {key}\r\n\r\n"
        )
        self.socket.sendall(request.encode("ascii"))
        response = b""
        while b"\r\n\r\n" not in response:
            response += self.socket.recv(4096)
        header = response.decode("latin-1")
        if not header.startswith("HTTP/1.1 101 "):
            raise RuntimeError(f"native control websocket rejected upgrade: {header!r}")
        accept = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
        ).decode("ascii")
        if f"Sec-WebSocket-Accept: {accept}".lower() not in header.lower():
            raise RuntimeError("native control websocket returned an invalid accept key")

    def close(self) -> None:
        self.socket.close()

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        mask = os.urandom(4)
        length = len(payload)
        if length < 126:
            header = bytes((0x80 | opcode, 0x80 | length))
        elif length <= 0xFFFF:
            header = bytes((0x80 | opcode, 0x80 | 126)) + struct.pack("!H", length)
        else:
            header = bytes((0x80 | opcode, 0x80 | 127)) + struct.pack("!Q", length)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        self.socket.sendall(header + mask + masked)

    def send_json(self, value: dict) -> None:
        self._send_frame(1, json.dumps(value, separators=(",", ":")).encode("utf-8"))

    def _read_exact(self, length: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < length:
            chunk = self.socket.recv(length - len(chunks))
            if not chunk:
                raise RuntimeError("native control websocket closed")
            chunks.extend(chunk)
        return bytes(chunks)

    def receive_json(self) -> dict:
        while True:
            first, second = self._read_exact(2)
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                length = struct.unpack("!H", self._read_exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._read_exact(8))[0]
            mask = self._read_exact(4) if second & 0x80 else None
            payload = self._read_exact(length)
            if mask:
                payload = bytes(
                    value ^ mask[index % 4] for index, value in enumerate(payload)
                )
            if opcode == 9:
                self._send_frame(10, payload)
                continue
            if opcode == 8:
                raise RuntimeError("native control websocket closed")
            if opcode == 1:
                return json.loads(payload.decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex-home", default=str(Path.home() / ".codex"))
    parser.add_argument("--model", default="gpt-5.6-luna")
    parser.add_argument("--workspace", default="/tmp")
    parser.add_argument("--timeout", type=float, default=300.0)
    args = parser.parse_args()

    deadline = time.monotonic() + args.timeout
    marker = "VELLUM_OFFICIAL_REMOTE_OK"
    observed: list[dict] = []
    websocket = ControlWebSocket(
        Path(args.codex_home) / "app-server-control" / "app-server-control.sock",
        args.timeout,
    )

    def receive_until(predicate) -> dict:
        while time.monotonic() < deadline:
            try:
                value = websocket.receive_json()
            except TimeoutError:
                continue
            observed.append(value)
            if predicate(value):
                return value
        raise TimeoutError("native control API response timeout")

    try:
        websocket.send_json(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "vellum-native-official-smoke",
                        "title": "Vellum native official smoke",
                        "version": "1",
                    },
                    "capabilities": {"experimentalApi": True},
                },
            }
        )
        initialized = receive_until(lambda value: value.get("id") == 1)
        if "error" in initialized:
            raise RuntimeError(f"initialize failed: {initialized['error']}")
        websocket.send_json(
            {"jsonrpc": "2.0", "method": "initialized", "params": None}
        )
        websocket.send_json(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "thread/start",
                "params": {
                    "model": args.model,
                    "modelProvider": "openai",
                    "cwd": args.workspace,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                    "ephemeral": True,
                },
            }
        )
        thread_response = receive_until(lambda value: value.get("id") == 2)
        if "error" in thread_response:
            raise RuntimeError(f"thread/start failed: {thread_response['error']}")
        thread_id = thread_response["result"]["thread"]["id"]
        websocket.send_json(
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [
                        {
                            "type": "text",
                            "text": f"Reply with exactly {marker} and nothing else.",
                        }
                    ],
                },
            }
        )
        turn_response = receive_until(lambda value: value.get("id") == 3)
        if "error" in turn_response:
            raise RuntimeError(f"turn/start failed: {turn_response['error']}")
        completed = receive_until(
            lambda value: value.get("method") == "turn/completed"
        )
        messages = [
            value.get("params", {}).get("item", {}).get("text", "")
            for value in observed
            if value.get("method") == "item/completed"
            and value.get("params", {}).get("item", {}).get("type")
            == "agentMessage"
        ]
        if not any(message.strip() == marker for message in messages):
            raise RuntimeError(
                f"official model did not return exact marker; messages={messages!r}"
            )
        print(
            json.dumps(
                {
                    "ok": True,
                    "model": args.model,
                    "threadId": thread_id,
                    "turnStatus": completed.get("params", {})
                    .get("turn", {})
                    .get("status"),
                    "marker": marker,
                },
                separators=(",", ":"),
            )
        )
        return 0
    finally:
        websocket.close()


if __name__ == "__main__":
    raise SystemExit(main())
