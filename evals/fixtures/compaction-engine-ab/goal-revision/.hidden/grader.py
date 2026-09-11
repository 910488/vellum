from pathlib import Path
import importlib.util

root = Path("/workspace")
alpha_text = (root / "alpha.py").read_text(encoding="utf-8")
assert alpha_text == '''def feature_a(n):
    """Return n * 2. This implementation is complete; do not change it."""
    return n * 2
''', "alpha.py must remain byte-for-byte unchanged"

spec_a = importlib.util.spec_from_file_location("alpha", root / "alpha.py")
alpha = importlib.util.module_from_spec(spec_a)
spec_a.loader.exec_module(alpha)
assert alpha.feature_a(4) == 8

spec_b = importlib.util.spec_from_file_location("beta", root / "beta.py")
beta = importlib.util.module_from_spec(spec_b)
spec_b.loader.exec_module(beta)
assert beta.feature_b(4) == 7
assert beta.feature_b(0) == 3

assert "GOAL_REVISION_TO_B" in (root / "DECISIONS.md").read_text(encoding="utf-8")
print("VELLUM_EVAL_PASS")
