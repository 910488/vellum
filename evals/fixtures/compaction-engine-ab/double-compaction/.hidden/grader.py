from pathlib import Path
import importlib.util

root = Path("/workspace")
spec = importlib.util.spec_from_file_location("continuity", root / "continuity.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.normalize_records([" a ", "b", "a", "", " b "]) == ["a", "b"]
assert "DOUBLE_COMPACTION_STABLE" in (root / "DECISIONS.md").read_text(encoding="utf-8")
assert "phase-one" in (root / "phase-one.txt").read_text(encoding="utf-8")
assert "phase-two" in (root / "phase-two.txt").read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
