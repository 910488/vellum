import pathlib
import sys

sys.path.insert(0, str(pathlib.Path("/workspace")))
from solution import chunk_text

assert chunk_text("abcdef", 2) == ["ab", "cd", "ef"]
assert chunk_text("", 4) == []
print("PASS")
