import importlib.util

spec = importlib.util.spec_from_file_location("solution", "/workspace/solution.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert module.normalize("  Hello   WORLD ") == "hello-world"
print("ok")
