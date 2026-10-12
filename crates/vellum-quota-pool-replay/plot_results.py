"""Plot aggregate replay JSON; never exports raw usage rows.

python plot_results.py --replay-exe target/debug/vellum-quota-pool-replay.exe \
    --database file:C:/path/usage.sqlite3?mode=ro --output qa/balanced-quota
"""
import argparse
import json
from pathlib import Path
import subprocess
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--replay-exe", required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--model")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    command = [args.replay_exe, args.database]
    if args.model:
        command.append(args.model)
    results = json.loads(subprocess.check_output(command))
    args.output.mkdir(parents=True, exist_ok=True)
    # Contains only aggregate counters and hourly dispersion, no identities,
    # request times, per-request token counts, or original account hashes.
    (args.output / "results.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
    rows = results["results"]
    fig, ax = plt.subplots(figsize=(8, 5), constrained_layout=True)
    for row in rows:
        label = row["strategy"] if row["lambda"] is None else f'lambda={row["lambda"]}'
        selected = row["lambda"] == 0.1
        ax.scatter(row["effective_tokens"] / 1e6, row["used_std"], marker="D" if selected else "o",
                   s=80 if selected else 40, color="#7554a6" if selected else "#687c8b")
        ax.annotate(label, (row["effective_tokens"] / 1e6, row["used_std"]), xytext=(6, 8), textcoords="offset points", fontsize=9)
    frontier = []
    for row in sorted((r for r in rows if r["strategy"] == "full"), key=lambda r: r["effective_tokens"]):
        if not frontier or row["used_std"] < frontier[-1]["used_std"]:
            frontier.append(row)
    ax.plot([r["effective_tokens"] / 1e6 for r in frontier], [r["used_std"] for r in frontier], "--", color="#7554a6", label="Sampled full-router Pareto frontier")
    ax.set(xlabel="Total effective input tokens (millions)", ylabel="Final normalized weekly used SD (points)", title="Cache cost vs quota balance (synthetic quota model)")
    ax.grid(alpha=.2)
    ax.legend(loc="best", fontsize=8)
    fig.savefig(args.output / "pareto.png", dpi=160)
    plt.close(fig)
    fig, ax = plt.subplots(figsize=(8, 4), constrained_layout=True)
    for row in rows:
        if row["strategy"] != "full" or row["lambda"] == .1:
            ax.plot([p["elapsed_hours"] for p in row["timeline"]], [p["used_std"] for p in row["timeline"]], label=row["strategy"] + (" (0.1)" if row["lambda"] else ""))
    ax.set(xlabel="Hours since first replayed request", ylabel="Normalized weekly used SD (points)", title="Quota dispersion over time")
    ax.grid(alpha=.2)
    ax.legend(fontsize=8)
    fig.savefig(args.output / "dispersion.png", dpi=160)
    plt.close(fig)
    print(f'{results["requests"]} requests; short-gap low reuse {results["low_reuse_percent"]:.2f}%')
    for row in rows:
        print({k: v for k, v in row.items() if k != "timeline"})

if __name__ == "__main__":
    main()
