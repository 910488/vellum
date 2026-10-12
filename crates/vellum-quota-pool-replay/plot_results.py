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
    parser.add_argument("--replay-exe")
    parser.add_argument("--input-json", type=Path)
    parser.add_argument("--database")
    parser.add_argument("--model")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.input_json:
        results = json.loads(args.input_json.read_text(encoding="utf-8"))
    else:
        if not args.replay_exe or not args.database:
            parser.error("provide --input-json or both --replay-exe and --database")
        command = [args.replay_exe, args.database]
        if args.model:
            command.append(args.model)
        results = json.loads(subprocess.check_output(command))
    args.output.mkdir(parents=True, exist_ok=True)
    # Contains only aggregate counters and hourly dispersion, no identities,
    # request times, per-request token counts, or original account hashes.
    (args.output / "results.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
    rows = results["results"]
    default_lambda = results.get("default_lambda", 0.2)
    fig, ax = plt.subplots(figsize=(8, 5), constrained_layout=True)
    for row in rows:
        label = ("801ccfd (0.1)" if row["strategy"] == "review08" else row["strategy"]) if row["lambda"] is None else f'{row["strategy"]} ({row["lambda"]})'
        selected = row["strategy"] == "full" and row["lambda"] == default_lambda
        ax.scatter(row["effective_tokens"] / 1e6, row["time_mean_sd"], marker="D" if selected else "o",
                   s=80 if selected else 40, color="#7554a6" if selected else "#687c8b")
        ax.annotate(label, (row["effective_tokens"] / 1e6, row["time_mean_sd"]), xytext=({0.05:(-12,24),0.1:(6,-22),0.2:(6,-10),1.0:(-80,8)}.get(row["lambda"],(6,8)) if row["strategy"]=="full" else ((6,-16) if row["strategy"] == "previous_placement" else (6,8))), textcoords="offset points", fontsize=9)
    frontier = []
    for row in sorted((r for r in rows if r["strategy"] == "full"), key=lambda r: r["effective_tokens"]):
        if not frontier or row["time_mean_sd"] < frontier[-1]["time_mean_sd"]:
            frontier.append(row)
    ax.plot([r["effective_tokens"] / 1e6 for r in frontier], [r["time_mean_sd"] for r in frontier], "--", color="#7554a6", label="Sampled full-router Pareto frontier")
    ax.set(xlabel="Total effective input tokens (millions)", ylabel="Time-weighted mean weekly used SD (points)", title="Cache cost vs quota balance (synthetic quota model)")
    ax.grid(alpha=.2)
    ax.legend(loc="best", fontsize=8)
    fig.savefig(args.output / "pareto.png", dpi=160)
    plt.close(fig)
    fig, ax = plt.subplots(figsize=(8, 4), constrained_layout=True)
    for row in rows:
        if row["strategy"] != "full" or row["lambda"] == default_lambda:
            ax.step([p["elapsed_hours"] for p in row["timeline"]], [p["used_std"] for p in row["timeline"]], label=("801ccfd (0.1)" if row["strategy"] == "review08" else row["strategy"] + (f" ({row['lambda']})" if row["lambda"] else "")), where="post", linewidth=1.2)
    ax.set(xlabel="Hours since first replayed request", ylabel="Normalized weekly used SD (points)", title="Quota dispersion over time")
    ax.grid(alpha=.2)
    ax.legend(fontsize=8)
    fig.savefig(args.output / "dispersion.png", dpi=160)
    plt.close(fig)
    if results.get("suite"):
        import numpy as np
        families = ["equal", "heterogeneous", "unequal_start", "external_burst"]
        fig, axes = plt.subplots(2, 2, figsize=(11, 7), constrained_layout=True)
        pareto, paxes = plt.subplots(2, 2, figsize=(11, 7), constrained_layout=True)
        for family, ax, pax in zip(families, axes.flat, paxes.flat):
            cases = [c for c in results["suite"] if c["scenario"] == family]
            time = np.linspace(0, max(r["timeline"][-1]["elapsed_hours"] for c in cases for r in c["results"]), 1000)
            for kind, lam, label in [("legacy",None,"legacy"),("review08",None,"801ccfd (0.1)"),("full",default_lambda,f"new ({default_lambda})")]:
                selected = [next(r for r in c["results"] if r["strategy"]==kind and (lam is None or r["lambda"]==lam)) for c in cases]
                curves=[]
                for r in selected:
                    hours=np.array([v["elapsed_hours"] for v in r["timeline"]])
                    sd=np.array([v["used_std"] for v in r["timeline"]])
                    curves.append(sd[np.maximum(0,np.searchsorted(hours,time,side="right")-1)])
                mean=np.mean(curves,axis=0); spread=np.std(curves,axis=0,ddof=1)
                line=ax.plot(time,mean,label=label,linewidth=1.1)[0]
                ax.fill_between(time,np.maximum(0,mean-spread),mean+spread,color=line.get_color(),alpha=.08)
            for lam in sorted({r["lambda"] for c in cases for r in c["results"] if r["strategy"]=="full"}):
                selected=[next(r for r in c["results"] if r["strategy"]=="full" and r["lambda"]==lam) for c in cases]
                x=np.mean([r["effective_tokens"] for r in selected])/1e6
                y=np.mean([r["time_mean_sd"] for r in selected])
                pax.scatter(x,y,marker="D" if lam==default_lambda else "o",color="#7554a6")
                pax.annotate(str(lam),(x,y),xytext=({0.05:(4,-12),0.1:(4,8),0.2:(4,12),1.0:(-20,8)}.get(lam,(4,8))),textcoords="offset points",fontsize=8)
            old=[next(r for r in c["results"] if r["strategy"]=="legacy") for c in cases]
            pax.axvline(np.mean([r["effective_tokens"] for r in old])*1.005/1e6,color="#ad6344",linestyle="--",label="legacy +0.5% cost")
            pax.axhline(np.mean([r["time_mean_sd"] for r in old]),color="#687c8b",linestyle=":",label="legacy mean SD")
            ax.set(title=f"{family} ({len(cases)} seeds)",xlabel="Elapsed hours",ylabel="Weekly SD (points)")
            pax.set(title=family,xlabel="Effective input (M)",ylabel="Time mean SD (points)")
            ax.grid(alpha=.2);pax.grid(alpha=.2)
            ax.legend(fontsize=7);pax.legend(fontsize=7)
        fig.suptitle("Primary scenario trajectories: seed mean with sample SD band")
        fig.savefig(args.output / "scenario-dispersion.png",dpi=160);plt.close(fig)
        pareto.savefig(args.output / "scenario-pareto.png",dpi=160);plt.close(pareto)
    print(f'{results["requests"]} requests; short-gap low reuse {results["low_reuse_percent"]:.2f}%')
    for row in rows:
        print({k: v for k, v in row.items() if k not in ["timeline","diagnostics","quota_events","calibration_snapshots"]})

if __name__ == "__main__":
    main()
