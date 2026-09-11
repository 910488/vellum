from pathlib import Path
import json

root = Path("/workspace")
payload = json.loads((root / "data.json").read_text(encoding="utf-8"))
expected = 1
for value in payload["values"]:
    expected *= int(value)

# Windows PowerShell 5.1 writes a UTF-8 BOM for `Set-Content -Encoding utf8`.
# The task grades the preserved integer tool result, not the editor's UTF-8
# signature choice, so decode that standard Windows representation exactly.
result = (root / "RESULT.txt").read_text(encoding="utf-8-sig").strip()
assert result == str(expected), f"RESULT.txt must keep the prior tool product {expected}, got {result!r}"
assert "TOOL_RESULTS_PRESERVED" in (root / "DECISIONS.md").read_text(encoding="utf-8")
assert "phase-one" in (root / "phase-one.txt").read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
