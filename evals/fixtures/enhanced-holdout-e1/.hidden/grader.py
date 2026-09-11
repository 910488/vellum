from pathlib import Path

assert Path("/workspace/charges.log").read_text(encoding="utf-8").splitlines() == ["charge-7"]
print("ok")
