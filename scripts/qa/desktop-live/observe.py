"""Observe what Vellum did for one Codex Desktop thread.

The Desktop thread id is the join key: Codex writes it into the rollout file,
and Vellum writes it into request_usage.conversation_identity and
agent_attribution_json. Everything here is read-only.

  python -I -X utf8 observe.py snapshot --out <dir>/baseline.json
  python -I -X utf8 observe.py report --thread <tid> --baseline <dir>/baseline.json --out <dir>

Never reads credential files, auth.json, or the proxy boundary key.
"""

from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import os
import re
import sqlite3
import sys
import time
from collections import Counter
from pathlib import Path

LOCAL = Path(os.environ["LOCALAPPDATA"])
DATA = LOCAL / "Vellum"
LOG = LOCAL / "com.vellum.desktop" / "logs" / "Vellum.log"
CODEX = Path.home() / ".codex"
ROUTED_PROVIDERS = {"vellum", "vellum-official"}
REVIEW_MODELS = {"codex-auto-review"}


def ro(name: str) -> sqlite3.Connection:
    con = sqlite3.connect(f"file:{DATA / name}?mode=ro", uri=True, timeout=5)
    con.row_factory = sqlite3.Row
    return con


def boot() -> dict:
    try:
        return json.loads((DATA / "boot.json").read_text(encoding="utf-8"))
    except OSError:
        return {}


def snapshot(out: Path) -> None:
    u = ro("usage.sqlite3")
    h = ro("history.sqlite3")
    snap = {
        "at": round(time.time(), 3),
        "boot": boot(),
        "request_usage_max_id": u.execute("select coalesce(max(id),0) from request_usage").fetchone()[0],
        "diagnostic_events_max_id": u.execute("select coalesce(max(id),0) from diagnostic_events").fetchone()[0],
        "subagent_events_max_id": u.execute("select coalesce(max(id),0) from subagent_events").fetchone()[0],
        "compaction_journal_count": h.execute("select count(*) from compaction_journal").fetchone()[0],
        "log_size": LOG.stat().st_size if LOG.exists() else 0,
        # Rotation renames Vellum.log into an archive, so the file id finds it again.
        "log_ino": LOG.stat().st_ino if LOG.exists() else 0,
    }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(snap, indent=1), encoding="utf-8")
    print(json.dumps(snap, indent=1))


def find_rollout(tid: str) -> Path | None:
    hits = glob.glob(str(CODEX / "sessions" / "*" / "*" / "*" / f"rollout-*-{tid}.jsonl"))
    return Path(max(hits, key=os.path.getmtime)) if hits else None


def read_rollout(path: Path) -> dict:
    meta, contexts, usage, errors, compactions = None, [], [], [], 0
    last_total = None
    turn_events = []
    calls, outputs = {}, set()
    items = Counter()
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            o = json.loads(line)
        except json.JSONDecodeError:
            continue
        t, p = o.get("type"), o.get("payload") or {}
        pt = p.get("type")
        if t == "session_meta" and meta is None:
            meta = {k: p.get(k) for k in ("id", "timestamp", "originator", "cli_version", "source", "model_provider", "cwd")}
        elif t == "turn_context":
            contexts.append({"ts": o.get("timestamp"), "turn_id": p.get("turn_id"), "model": p.get("model"), "effort": p.get("effort"),
                             "cwd": p.get("cwd"), "sandbox": (p.get("sandbox_policy") or {}).get("type")})
        elif t == "compacted":
            compactions += 1
        elif t == "event_msg":
            ts = o.get("timestamp")
            if pt in ("task_started", "task_complete", "turn_aborted"):
                turn_events.append({"ts": ts, "kind": pt})
            if pt == "task_complete" and p.get("error"):
                errors.append({"ts": ts, "kind": "task_complete.error", "detail": str(p["error"])[:300]})
            elif pt == "turn_aborted":
                errors.append({"ts": ts, "kind": "turn_aborted", "detail": str(p.get("reason"))[:300]})
            elif pt in ("error", "stream_error"):
                errors.append({"ts": ts, "kind": pt, "detail": str(p.get("message"))[:300]})
            elif pt == "context_compacted":
                compactions += 1
            elif pt == "token_count":
                info = p.get("info") or {}
                last = info.get("last_token_usage") or {}
                total = (info.get("total_token_usage") or {}).get("total_tokens")
                # Codex re-emits token_count without a new response (turn start,
                # after compaction); only a moved running total is a response.
                if last and total != last_total:
                    last_total = total
                    usage.append({"ts": o.get("timestamp"), "input": last.get("input_tokens"),
                                  "cached": last.get("cached_input_tokens"), "output": last.get("output_tokens")})
        elif t == "response_item":
            items[pt] += 1
            if pt in ("function_call", "custom_tool_call", "local_shell_call"):
                calls[p.get("call_id")] = p.get("name") or pt
            elif pt in ("function_call_output", "custom_tool_call_output", "local_shell_call_output"):
                outputs.add(p.get("call_id"))
    return {
        "path": str(path), "meta": meta, "turn_contexts": contexts, "token_counts": usage,
        "turn_events": turn_events,
        "errors": errors, "compactions": compactions,
        "tool_calls": len(calls), "response_items": dict(items),
        "orphan_tool_calls": [{"call_id": k, "name": v} for k, v in calls.items() if k not in outputs],
    }


def norm_model(m: str | None) -> str:
    m = re.sub(r"^vlm-[0-9a-f]{10}-", "", m or "")
    return re.sub(r"[^a-z0-9]", "", m.lower())


def usage_rows(tid: str, since_id: int) -> list[dict]:
    u = ro("usage.sqlite3")
    rows = u.execute(
        "select * from request_usage where id > ? and (conversation_identity like ? or agent_attribution_json like ?) order by id",
        (since_id, f"%{tid}%", f'%"sessionId":"{tid}"%'),
    ).fetchall()
    out = []
    for r in rows:
        d = dict(r)
        try:
            d["attribution"] = json.loads(d.pop("agent_attribution_json") or "null")
        except json.JSONDecodeError:
            d["attribution"] = None
        for k in ("stage_times_ms", "stream_quality"):
            d.pop(k, None)
        out.append(d)
    return out


def events_for(request_ids: list[str], since_id: int, since_at: int) -> dict:
    u = ro("usage.sqlite3")
    kinds = Counter()
    account_switches = []
    if request_ids:
        q = ",".join("?" * len(request_ids))
        for r in u.execute(f"select kind from diagnostic_events where id > ? and request_id in ({q})", (since_id, *request_ids)):
            kinds[r["kind"]] += 1
    for r in u.execute("select kind, payload, created_at from diagnostic_events where id > ? and created_at >= ?", (since_id, since_at)):
        if r["kind"] == "official_account_selected":
            account_switches.append(r["created_at"])
        kinds["window:" + r["kind"]] += 1
    return {"by_kind": dict(kinds), "account_switches": account_switches}


def subagents(since_id: int, request_ids: list[str]) -> list[dict]:
    if not request_ids:
        return []
    u = ro("usage.sqlite3")
    q = ",".join("?" * len(request_ids))
    return [dict(r) for r in u.execute(
        f"select kind, call_id, parent_request_id, child_request_id, child_model, link_method, link_confidence, created_at "
        f"from subagent_events where id > ? and (parent_request_id in ({q}) or child_request_id in ({q}))",
        (since_id, *request_ids, *request_ids))]


def history_for(tid: str, since_at: int) -> dict:
    h = ro("history.sqlite3")
    rh = h.execute("select count(*), max(context_tokens) from response_history where conversation_key like ? and created_at >= ?",
                   (f"%{tid}%", since_at)).fetchone()
    realms = [dict(r) for r in h.execute(
        "select rr.route_id, rr.model, rr.model_family, count(*) n from response_history rh join response_realm rr using(response_id) "
        "where rh.conversation_key like ? and rh.created_at >= ? group by 1,2,3", (f"%{tid}%", since_at))]
    journal = [{"id_len": len(r[0]), "author": "vellum" if r[0].startswith("cmp_vellum_") else "provider", "created_at": r[1]}
               for r in h.execute("select head_response_id, created_at from compaction_journal where created_at >= ?", (since_at,))]
    return {"responses": rh[0], "max_context_tokens": rh[1], "realms": realms, "compaction_journal": journal}


def log_files() -> list[Path]:
    """Active log and its rotated archives, oldest first."""
    files = [p for p in LOG.parent.glob("*.log") if p.is_file()]
    return sorted(files, key=lambda p: (p.stat().st_mtime, p == LOG))


def log_window(since_size: int, since_ino: int = 0) -> dict:
    if not LOG.exists():
        return {"missing": True}
    files = log_files()
    start = next((i for i, p in enumerate(files) if since_ino and p.stat().st_ino == since_ino), None)
    rotated = start is not None and files[start] != LOG
    if start is None:
        # Baseline predates file ids, or its archive was already pruned.
        size = LOG.stat().st_size
        rotated = bool(since_ino) or since_size > size
        files, start, since_size = [LOG], 0, since_size if since_size <= size and not since_ino else 0
    text = ""
    for i, path in enumerate(files[start:]):
        with path.open("rb") as f:
            f.seek(since_size if i == 0 else 0)
            text += f.read().decode("utf-8", "replace")
    lines = text.splitlines()
    bad = [l[:300] for l in lines if re.search(r"\]\[(ERROR|WARN)\]|panic", l)]
    statuses = Counter(m.group(1) for l in lines for m in [re.search(r"upstream headers .*status=(\d+)", l)] if m)
    return {"lines": len(lines), "rotated": rotated, "files": len(files) - start, "warn_error": bad[:40], "warn_error_count": len(bad),
            "upstream_status": dict(statuses)}


def check(name: str, verdict: str, detail: str) -> dict:
    return {"check": name, "verdict": verdict, "detail": detail}


def report(tid: str, baseline: dict, out: Path) -> int:
    rollout_path = find_rollout(tid)
    if not rollout_path:
        print(f"rollout for {tid} not found", file=sys.stderr)
        return 2
    ro_ = read_rollout(rollout_path)
    since_at = baseline.get("at") or 0
    if not since_at and (ro_["meta"] or {}).get("timestamp"):
        since_at = int(dt.datetime.fromisoformat(ro_["meta"]["timestamp"].replace("Z", "+00:00")).timestamp())
    # Judge only what happened after the snapshot, on both sides.
    def after(ts: str | None) -> bool:
        return not ts or dt.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() >= since_at
    ro_["token_counts"] = [u for u in ro_["token_counts"] if after(u["ts"])]
    ro_["turn_contexts"] = [c for c in ro_["turn_contexts"] if after(c.get("ts"))] or ro_["turn_contexts"][-1:]
    ro_["errors"] = [e for e in ro_["errors"] if after(e["ts"])]
    kinds = Counter(e["kind"] for e in ro_["turn_events"] if after(e["ts"]))
    ro_["turns"] = {"started": kinds["task_started"], "completed": kinds["task_complete"], "aborted": kinds["turn_aborted"]}
    rows = usage_rows(tid, baseline.get("request_usage_max_id", 0))
    root = [r for r in rows if (r["attribution"] or {}).get("depth", 0) == 0 and r["model"] not in REVIEW_MODELS]
    children = [r for r in rows if (r["attribution"] or {}).get("depth", 0) > 0 and r["model"] not in REVIEW_MODELS]
    reviews = [r for r in rows if r["model"] in REVIEW_MODELS]
    req_ids = [r["request_id"] for r in rows if r["request_id"]]
    ev = events_for(req_ids, baseline.get("diagnostic_events_max_id", 0), int(since_at))
    subs = subagents(baseline.get("subagent_events_max_id", 0), req_ids)
    hist = history_for(tid, int(since_at))
    logw = log_window(baseline.get("log_size", 0), baseline.get("log_ino", 0))
    now_boot = boot()
    checks = []

    provider = (ro_["meta"] or {}).get("model_provider")
    checks.append(check("routed-through-vellum", "PASS" if provider in ROUTED_PROVIDERS else "FAIL",
                        f"session_meta.model_provider={provider}"))

    t = ro_["turns"]
    turn_ok = t["started"] == t["completed"] + t["aborted"] and not ro_["errors"]
    checks.append(check("turns-completed", "PASS" if turn_ok else "FAIL", f"{t} errors={ro_['errors'][:5]}"))

    orphans = ro_["orphan_tool_calls"]
    checks.append(check("tool-calls-paired", "PASS" if not orphans else "FAIL",
                        f"{ro_['tool_calls']} calls, orphans={orphans[:5]}"))

    # Every model response Codex recorded must have a Vellum usage row with the same token counts.
    unmatched = list(root)
    matched, missing = [], []
    for u in ro_["token_counts"]:
        hit = next((r for r in unmatched if (r["input_tokens"], r["output_tokens"]) == (u["input"], u["output"])), None)
        if hit:
            unmatched.remove(hit)
            matched.append(hit)
        else:
            missing.append(u)
    checks.append(check("usage-matches-rollout", "PASS" if not missing else "FAIL",
                        f"rollout responses={len(ro_['token_counts'])} matched={len(matched)} unmatched_rollout={missing[:5]}"))
    # Rows Codex never recorded as a turn response: failed attempts, prewarm, title generation.
    aux = Counter(f"{r['model']} {r['status']}" for r in unmatched)
    checks.append(check("vellum-only-requests", "INFO", f"{dict(aux)}"))

    want = {norm_model(c["model"]) for c in ro_["turn_contexts"]}
    got = {norm_model(r["model"]) for r in matched}
    checks.append(check("model-matches-selection", "PASS" if got <= want else "FAIL",
                        f"desktop={sorted({c['model'] for c in ro_['turn_contexts']})} vellum={sorted({r['model'] for r in matched})}"))

    routes = Counter(r["route_id"] for r in root)
    checks.append(check("route", "INFO", dict(routes).__repr__()))

    failed = Counter(f"{r['status']} {r['error_category']} {(r['error'] or '')[:90]}"
                     for r in root + children if r["status"] != 200 or (r["outcome"] not in (None, "success")))
    checks.append(check("request-status", "PASS" if not failed else "WARN", f"non-success={dict(failed)}"))

    cold = [{"id": r["id"], "input": r["input_tokens"], "conn": r["connection_id"]}
            for i, r in enumerate(root) if i > 0 and r["input_tokens"] > 20000 and not r["cached_input_tokens"]]
    conns = len({r["connection_id"] for r in root})
    checks.append(check("cache-continuity", "PASS" if not cold else "WARN", f"connections={conns} cold_followups={cold[:6]}"))

    accounts = {r["execution_account_hash"] for r in root if r["route_id"] == "openai-official" and r["execution_account_hash"]}
    acc_ok = len(accounts) <= 1 or ev["account_switches"]
    checks.append(check("account-stable", "PASS" if acc_ok else "WARN",
                        f"accounts={len(accounts)} manual_switches={len(ev['account_switches'])}"))

    before = baseline.get("boot", {}).get("bootCount")
    if before is None:
        checks.append(check("vellum-not-restarted", "NOT_RUN", "no baseline snapshot"))
    else:
        restarted = before != now_boot.get("bootCount")
        checks.append(check("vellum-not-restarted", "FAIL" if restarted else "PASS",
                            f"bootCount {before} -> {now_boot.get('bootCount')}"))

    log_verdict = "NOT_RUN" if "log_size" not in baseline else ("PASS" if logw.get("warn_error_count", 0) == 0 else "WARN")
    checks.append(check("log-warn-error", log_verdict,
                        f"count={logw.get('warn_error_count')} upstream_status={logw.get('upstream_status')}"))

    checks.append(check("subagents", "INFO",
                        f"child rows={len(children)} child threads={len({(r['attribution'] or {}).get('threadId') for r in children})} "
                        f"subagent_events={Counter(s['kind'] for s in subs)}"))
    review_errors = Counter((r["error"] or "")[:90] for r in reviews if r["status"] != 200)
    checks.append(check("auto-review", "INFO" if not review_errors else "WARN",
                        f"rows={len(reviews)} non-200={sum(review_errors.values())} {dict(review_errors)}"))
    checks.append(check("compaction", "INFO",
                        f"rollout compactions={ro_['compactions']} journal={hist['compaction_journal']}"))

    evidence = {
        "thread": tid, "generated_at": int(time.time()), "baseline": baseline, "boot_now": now_boot,
        "rollout": ro_, "vellum": {"rows": rows, "events": ev, "subagent_events": subs, "history": hist, "log": logw},
        "checks": checks,
    }
    out.mkdir(parents=True, exist_ok=True)
    (out / f"evidence-{tid}.json").write_text(json.dumps(evidence, indent=1, ensure_ascii=False, default=str), encoding="utf-8")

    lines = [f"# Thread {tid}", "",
             f"- rollout: `{rollout_path.name}`  originator={(ro_['meta'] or {}).get('originator')}  cli={(ro_['meta'] or {}).get('cli_version')}",
             f"- window: {dt.datetime.fromtimestamp(since_at)} → {dt.datetime.now():%Y-%m-%d %H:%M:%S}", "",
             "| check | verdict | detail |", "|---|---|---|"]
    for c in checks:
        lines.append(f"| {c['check']} | {c['verdict']} | {c['detail'].replace('|', '/')} |")
    md = "\n".join(lines) + "\n"
    (out / f"report-{tid}.md").write_text(md, encoding="utf-8")
    print(md)
    return 1 if any(c["verdict"] == "FAIL" for c in checks) else 0


def main() -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("snapshot")
    s.add_argument("--out", type=Path, required=True)
    r = sub.add_parser("report")
    r.add_argument("--thread", required=True)
    r.add_argument("--baseline", type=Path)
    r.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    if a.cmd == "snapshot":
        snapshot(a.out)
        return 0
    base = json.loads(a.baseline.read_text(encoding="utf-8")) if a.baseline else {}
    return report(a.thread, base, a.out)


if __name__ == "__main__":
    sys.exit(main())
