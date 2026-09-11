from pathlib import Path
import importlib.util

root = Path("/workspace")
spec = importlib.util.spec_from_file_location("continuity", root / "continuity.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

assert module.normalize_records([" a ", "b", "a", "", " b "]) == ["a", "b"]
for marker in ("phase-one", "phase-two"):
    marker_path = root / f"{marker}.txt"
    if marker_path.exists():
        assert marker in marker_path.read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
