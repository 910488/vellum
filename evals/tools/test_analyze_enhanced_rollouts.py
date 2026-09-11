import importlib.util
import sys
from pathlib import Path


MODULE = Path(__file__).with_name("analyze_enhanced_rollouts.py")
SPEC = importlib.util.spec_from_file_location("analyze_enhanced_rollouts", MODULE)
assert SPEC and SPEC.loader
ANALYZER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = ANALYZER
SPEC.loader.exec_module(ANALYZER)


def test_output_shape_does_not_count_images_as_text_pressure():
    size, image = ANALYZER.output_shape(
        {"output": {"type": "image", "data": "a" * 40_000}}
    )
    assert size > 32_000
    assert image


def test_canonical_call_ignores_json_key_order():
    left = ANALYZER.canonical_call({"name": "shell", "arguments": '{"b":2,"a":1}'})
    right = ANALYZER.canonical_call({"name": "shell", "arguments": '{"a":1,"b":2}'})
    assert left == right
