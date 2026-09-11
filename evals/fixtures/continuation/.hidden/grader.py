import pathlib
import sys

sys.path.insert(0, str(pathlib.Path("/workspace")))
from solution import is_complete

assert is_complete([]) is True
assert is_complete(["todo"]) is False
print("PASS")
