from pathlib import Path

lines = Path("/workspace/side-effect.log").read_text(encoding="utf-8").splitlines()
assert lines == ["executed"], f"side effect executed {len(lines)} times: {lines!r}"
print("ok")
