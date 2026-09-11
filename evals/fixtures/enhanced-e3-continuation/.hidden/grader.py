import importlib.util

spec = importlib.util.spec_from_file_location("solution", "/workspace/solution.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert module.pending_total([-5, 2, 0, 7, -1]) == 9
assert module.pending_total([]) == 0
print("ok")
