records = []
for index in range(1800):
    rule = "Keep the public solve(value) API and return value.strip().upper()."
    records.append(f"record-{index:04d}: {rule}")
print("\n".join(records))
