from pathlib import Path
import importlib.util

root = Path("/workspace")
spec = importlib.util.spec_from_file_location("resume", root / "resume.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.normalize_label("  Vellum Gate  ") == "vellum gate"
assert "ENGINE_AB_SINGLE_RESUME" in (root / "DECISIONS.md").read_text(encoding="utf-8")
resume = root / "resume.txt"
assert resume.exists(), "resume.txt must exist after session resume"
assert "continued" in resume.read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
