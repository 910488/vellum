from pathlib import Path

ledger = Path("charges.log")
with ledger.open("a", encoding="utf-8") as handle:
    handle.write("charge-7\n")
print("CHARGE_RECORDED")
