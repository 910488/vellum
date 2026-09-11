from pathlib import Path

marker = Path("side-effect.log")
with marker.open("a", encoding="utf-8") as handle:
    handle.write("executed\n")
print("SIDE_EFFECT_OK")
