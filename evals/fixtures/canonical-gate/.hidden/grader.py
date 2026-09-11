from pathlib import Path
import importlib.util

root = Path("/workspace")
spec = importlib.util.spec_from_file_location("gate", root / "gate.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.normalize_label("  Vellum Gate  ") == "vellum gate"
assert "CANONICAL_GATE_STABLE" in (root / "DECISIONS.md").read_text(encoding="utf-8")
resume = root / "resume.txt"
if resume.exists():
    assert "continued" in resume.read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
