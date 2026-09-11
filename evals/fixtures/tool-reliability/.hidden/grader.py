import pathlib
import sys

sys.path.insert(0, str(pathlib.Path("/workspace")))
from solution import unique_sorted

assert unique_sorted([3, 1, 2, 1]) == [1, 2, 3]
assert unique_sorted([]) == []
print("PASS")
