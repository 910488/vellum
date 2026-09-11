import importlib.util

spec = importlib.util.spec_from_file_location("solution", "/workspace/solution.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert module.positive_product([-1, 2, 0, 3]) == 6
assert module.positive_product([-2, 0]) == 1
print("ok")
