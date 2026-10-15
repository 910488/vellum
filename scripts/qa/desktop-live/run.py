"""Drive Codex Desktop through Vellum and judge each step from both sides.

Codex Desktop executes the turns (queued via the codex-desktop bridge skill);
observe.py reads what Vellum recorded for that thread; each step adds its own
objective check (a marker in the reply, a file on disk, a test run).

  python -I -X utf8 run.py preflight
  python -I -X utf8 run.py round --model gpt-6.1-sol [--only S1,S2] [--alt-model gpt-6-luna]
  python -I -X utf8 run.py probe --model vlm-...        # one-line ping on another model
  python -I -X utf8 run.py observe --thread <tid> [--baseline <file>]

One round = one thread in the Desktop project "Vellum QA", every step in order.
Output lands in qa/runs/desktop-live-<stamp>/ (git-ignored).
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import secrets
import subprocess
import sys
import textwrap
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
BRIDGE = Path.home() / ".claude" / "skills" / "codex-desktop" / "codex_bridge.py"
OBSERVE = HERE / "observe.py"
PROJECT = os.environ.get("VELLUM_QA_PROJECT", "Vellum QA")
PROJECT_ROOT = Path(os.environ.get("VELLUM_QA_PROJECT_ROOT", r"E:\code_E\vellum-qa\workspace"))
PREFIX = "（這則訊息來自 Claude，協調者，正在對 Vellum 做功能測試。請照字面執行，不要做額外的事。）\n\n"
PY = [sys.executable, "-I", "-X", "utf8"]
ROUTED = ("vellum", "vellum-official")


def bridge():
    spec = importlib.util.spec_from_file_location("codex_bridge", BRIDGE)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def sh(args: list, timeout: float | None = None, check: bool = True, cwd: Path | None = None) -> subprocess.CompletedProcess:
    r = subprocess.run([str(a) for a in args], capture_output=True, text=True, encoding="utf-8", errors="replace",
                       timeout=timeout, cwd=cwd)
    if check and r.returncode != 0:
        raise SystemExit(f"command failed ({r.returncode}): {' '.join(map(str, args))}\n{r.stdout[-800:]}\n{r.stderr[-800:]}")
    return r


def norm(m: str | None) -> str:
    return re.sub(r"[^a-z0-9]", "", re.sub(r"^vlm-[0-9a-f]{10}-", "", (m or "").lower()))


# ---------- workspace ----------

CALC = '''def add(a, b):
    return a - b


def mul(a, b):
    return a * b
'''

TEST = '''import unittest

from calc import add, mul


class CalcTest(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_mul(self):
        self.assertEqual(mul(2, 3), 6)


if __name__ == "__main__":
    unittest.main()
'''


def make_workspace(run: Path) -> tuple[Path, str]:
    """A fresh git repo under the project root, so the thread's cwd stays inside the project."""
    marker = f"VQA-{secrets.token_hex(4).upper()}"
    ws = PROJECT_ROOT / run.name
    ws.mkdir(parents=True)
    (ws / "calc.py").write_text(CALC, encoding="utf-8")
    (ws / "test_calc.py").write_text(TEST, encoding="utf-8")
    (ws / "notes.txt").write_text(f"secret marker: {marker}\n", encoding="utf-8")
    (ws / ".gitignore").write_text("__pycache__/\n", encoding="utf-8")
    sh(["git", "-C", ws, "init", "-q"])
    sh(["git", "-C", ws, "add", "-A"])
    sh(["git", "-C", ws, "-c", "user.name=qa", "-c", "user.email=qa@localhost", "commit", "-qm", "fixture"])
    return ws, marker


def unit_tests(ws: Path) -> bool:
    return subprocess.run([sys.executable, "-m", "unittest", "-q"], cwd=ws, capture_output=True, timeout=120).returncode == 0


# ---------- Desktop thread control ----------

def project_id(srv) -> str:
    projects = srv.call("project/list", {"limit": 500})["data"]
    for p in projects:
        if p["name"] == PROJECT:
            return p["id"]
    return srv.call("project/create", {"idempotencyKey": f"vellum-qa:{PROJECT}", "name": PROJECT,
                                       "roots": [{"path": str(PROJECT_ROOT)}]})["project"]["id"]


def legacy_project_id(b, app_pid: str) -> str:
    """Desktop's sidebar and cwd resolution still read the legacy `local-projects` table.

    A project created through `project/create` exists only on the app-server side, so
    Desktop would treat the thread as projectless and run it in C:\\. Desktop must be closed.
    """
    import shutil, uuid
    state = b.load_state()
    for lid, p in state.get("local-projects", {}).items():
        if p.get("name") == PROJECT:
            return lid
    os.makedirs(b.BACKUP_DIR, exist_ok=True)
    shutil.copy2(b.STATE_JSON, os.path.join(b.BACKUP_DIR, f"codex-global-state.{int(time.time())}.pre-project.json"))
    lid, now = str(uuid.uuid4()), int(time.time() * 1000)
    state.setdefault("local-projects", {})[lid] = {"id": lid, "name": PROJECT, "rootPaths": [str(PROJECT_ROOT)],
                                                   "createdAt": now, "updatedAt": now}
    state.setdefault("project-order", []).append(lid)
    host = next(iter(state.get("app-server-project-id-by-legacy-project-id-by-host", {})), None)
    if host:
        state["app-server-project-id-by-legacy-project-id-by-host"][host][lid] = app_pid
    tmp = b.STATE_JSON + ".qa-tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, ensure_ascii=False, separators=(",", ":"))
    os.replace(tmp, b.STATE_JSON)
    return lid


def start_private_thread(b, ws: Path, model: str, effort: str, project: str | None) -> str:
    srv = b.AppServer(str(ws))
    try:
        params = {"cwd": str(ws), "model": model, "config": {"model_reasoning_effort": effort},
                  "approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
                  "permissions": b.WORKSPACE_PROFILE, "threadSource": "user"}
        if project:
            params["projectId"] = project
        tid = srv.call("thread/start", params)["thread"]["id"]
        srv.run_turn(tid, str(ws), "（協調者）建立 session。這一輪只回覆「就緒」，不要執行任何指令。", model, effort)
    finally:
        srv.close()
    return tid


OWN_THREADS: set[str] = set()


def new_desktop_thread(ws: Path, model: str, effort: str, msg: Path) -> str:
    """Thread assigned to the QA project in Desktop's own state, first message queued for Desktop.

    Desktop runs queued turns on its composer's model and effort; `model` here only
    covers the private first turn. Closes and reopens Desktop once (refuses if busy).
    """
    b = bridge()
    srv = b.AppServer(str(ws))
    try:
        app_pid = project_id(srv)
    finally:
        srv.close()
    tid = start_private_thread(b, ws, model, effort, app_pid)
    OWN_THREADS.add(tid)
    # Threads this runner created are never a reason to refuse; anyone else's are.
    b.close_desktop(False, set(OWN_THREADS))
    lid = legacy_project_id(b, app_pid)
    b.patch_state(tid, lid)
    send(ws, tid, msg)
    return tid


def private_turn(ws: Path, tid: str, text: str, model: str, effort: str, timeout: float) -> str:
    """One turn in a private app-server: same Codex, same config.toml, same Vellum proxy; no Desktop."""
    b = bridge()
    srv = b.AppServer(str(ws))
    try:
        srv.call("thread/resume", {"threadId": tid, "cwd": str(ws), "approvalPolicy": "on-request",
                                   "approvalsReviewer": "auto_review", "permissions": b.WORKSPACE_PROFILE})
        srv.run_turn(tid, str(ws), PREFIX + text, model, effort, timeout=timeout)
    finally:
        srv.close()
    return last_reply(tid)


def private_compact(ws: Path, tid: str, timeout: float = 900) -> str:
    b = bridge()
    srv = b.AppServer(str(ws))
    try:
        srv.call("thread/resume", {"threadId": tid, "cwd": str(ws)})
        srv.call("thread/compact/start", {"threadId": tid})
        end = time.time() + timeout
        while time.time() < end:
            try:
                m = srv.msgs.get(timeout=1)
            except Exception:
                continue
            if m.get("method") in ("turn/completed", "thread/compacted"):
                return m["method"]
        raise TimeoutError("compaction did not finish")
    finally:
        srv.close()


def last_reply(tid: str) -> str:
    b = bridge()
    msg = ""
    for j in b.rollout_events(tid):
        p = j.get("payload", {})
        if j["type"] == "event_msg" and p.get("type") == "task_complete":
            msg = p.get("last_agent_message") or ""
    return msg


def send(ws: Path, tid: str, msg: Path) -> None:
    b = bridge()
    r = subprocess.run([b.codex_exe(), "queue", "--thread", tid, "--message", msg.read_text(encoding="utf-8")],
                       cwd=ws, stdin=subprocess.DEVNULL, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if r.returncode != 0:
        raise SystemExit(f"codex queue failed: {r.stderr.strip()[-500:]}")
    b.open_thread(tid)


def wait(tid: str, timeout: float) -> str:
    r = sh(PY + [BRIDGE, "wait", "--thread", tid, "--timeout", timeout, "--interval", 8], timeout=timeout + 120, check=False)
    return r.stdout.strip() if r.returncode == 0 else "<<timeout>>"


# ---------- observation + verdict ----------

class Round:
    """One thread. `private` turns run in a private app-server instead of Desktop."""

    def __init__(self, run: Path, ws: Path, tid: str | None, model: str, effort: str, marker: str, private: bool = False):
        self.run, self.ws, self.tid, self.model, self.effort, self.marker, self.private = run, ws, tid, model, effort, marker, private
        self.results: list[dict] = []
        self.base: Path | None = None

    def snapshot(self, name: str) -> None:
        self.base = self.run / f"baseline-{name}.json"
        sh(PY + [OBSERVE, "snapshot", "--out", self.base])

    def msg(self, name: str, body: str) -> Path:
        p = self.run / f"msg-{name}.md"
        p.write_text(PREFIX + textwrap.dedent(body).strip() + "\n", encoding="utf-8")
        return p

    def ask(self, name: str, body: str, timeout: float, model: str | None = None) -> str:
        self.snapshot(name)
        if self.private:
            if self.tid is None:
                self.tid = start_private_thread(bridge(), self.ws, self.model, self.effort, None)
            return private_turn(self.ws, self.tid, textwrap.dedent(body).strip(), model or self.model, self.effort, timeout)
        if self.tid is None:
            self.tid = new_desktop_thread(self.ws, self.model, self.effort, self.msg(name, body))
            (self.run / "thread.txt").write_text(f"{self.tid}\n{self.ws}\n", encoding="utf-8")
            print(f"desktop thread {self.tid}", flush=True)
        else:
            send(self.ws, self.tid, self.msg(name, body))
        return wait(self.tid, timeout)

    def observe(self, name: str, tid: str | None = None) -> dict:
        tid = tid or self.tid
        out = self.run / f"obs-{name}"
        sh(PY + [OBSERVE, "report", "--thread", tid, "--out", out] + (["--baseline", self.base] if self.base else []),
           check=False, timeout=300)
        return json.loads((out / f"evidence-{tid}.json").read_text(encoding="utf-8"))

    def judge(self, name: str, ev: dict, own: list[tuple[str, bool, str]], reply: str, model: str | None = None) -> dict:
        model = model or self.model
        ran = (ev["rollout"]["turn_contexts"] or [{}])[-1].get("model")
        checks = [{"check": "judged-turn-model", "verdict": "PASS" if norm(ran) == norm(model) else "BLOCKED",
                   "detail": f"asked={model} desktop ran={ran}"}]
        cwd = (ev["rollout"]["turn_contexts"] or [{}])[-1].get("cwd") or ""
        checks.append({"check": "judged-turn-cwd", "verdict": "PASS" if Path(cwd) == self.ws else "BLOCKED",
                       "detail": f"cwd={cwd}"})
        checks += [{"check": c, "verdict": "PASS" if ok else "FAIL", "detail": d} for c, ok, d in own] + ev["checks"]
        v = "FAIL" if any(c["verdict"] == "FAIL" for c in checks) or reply == "<<timeout>>" else "PASS"
        if any(c["verdict"] == "BLOCKED" for c in checks[:2]):
            v = "BLOCKED"  # the turn did not run where/on what we are testing; nothing else is evidence about it
        rec = {"scenario": name, "thread": self.tid, "path": "app-server" if self.private else "desktop", "verdict": v, "reply": reply[:2000], "checks": checks}
        (self.run / f"verdict-{name}.json").write_text(json.dumps(rec, indent=1, ensure_ascii=False), encoding="utf-8")
        print(f"\n=== {name}: {v}", flush=True)
        for c in checks:
            if c["verdict"] != "INFO" and not (c["verdict"] == "PASS" and c["check"].startswith("judged")):
                print(f"  [{c['verdict']}] {c['check']}: {c['detail'][:240]}", flush=True)
        self.results.append(rec)
        return rec


def root_rows(ev: dict) -> list[dict]:
    return [r for r in ev["vellum"]["rows"] if (r.get("attribution") or {}).get("depth", 0) == 0
            and r["model"] != "codex-auto-review"]


# ---------- steps ----------

def s1(r: Round, a):
    tok = f"S1-{secrets.token_hex(3).upper()}"
    reply = r.ask("S1", f"不要呼叫任何工具。只回覆這一串字：{tok}", a.timeout)  # creates the Desktop thread
    r.judge("S1-plain", r.observe("S1"), [("reply-token", tok in reply, reply[:120])], reply)


def s2(r: Round, a):
    reply = r.ask("S2", "讀取工作目錄的 notes.txt，回覆裡面 secret marker 的值，只回覆那個值。", a.timeout)
    ev = r.observe("S2")
    r.judge("S2-read-file", ev, [("reply-marker", r.marker in reply, reply[:120]),
                                 ("used-a-tool", ev["rollout"]["tool_calls"] > 0, "")], reply)


def s3(r: Round, a):
    reply = r.ask("S3", """
        calc.py 的 add 有 bug。只修正 add 這個函式，不要改其他檔案。
        改完執行 `python -m unittest -q`，把最後一行輸出原樣回覆給我。
    """, a.timeout)
    ev = r.observe("S3")
    diff = sh(["git", "-C", r.ws, "status", "--porcelain"]).stdout.split("\n")
    changed = sorted(l[3:] for l in diff if l.strip())
    r.judge("S3-edit-and-test", ev, [
        ("only-calc-changed", changed == ["calc.py"], f"changed={changed}"),
        ("tests-pass-on-disk", unit_tests(r.ws), ""),
        ("reply-shows-ok", "OK" in reply, reply[-160:]),
    ], reply)


def s4(r: Round, a):
    reply = r.ask("S4", "不要呼叫任何工具，只憑這個對話的記憶回答：你剛才修了哪個函式？notes.txt 的 marker 是什麼？用「函式名,marker」的格式回覆。", a.timeout)
    ev = r.observe("S4")
    rows = root_rows(ev)
    first = rows[0] if rows else {}
    idents = {x["conversation_identity"] for x in rows}
    r.judge("S4-continue", ev, [
        ("reply-recalls", "add" in reply and r.marker in reply, reply[:160]),
        ("same-conversation-identity", len(idents) == 1 and r.tid in next(iter(idents), ""), f"{idents}"),
        ("first-request-cache-hit", bool(first.get("cached_input_tokens")),
         f"cached={first.get('cached_input_tokens')} input={first.get('input_tokens')} conn={first.get('connection_id')}"),
    ], reply)


def s5(r: Round, a):
    """Compaction, on the app-server path (Desktop's /compact is not scriptable): read, compact, recall."""
    p = Round(r.run, r.ws, None, a.model, a.effort, r.marker, private=True)
    reply = p.ask("S5a", "讀取工作目錄的 notes.txt，只回覆 secret marker 的值。", a.timeout)
    p.judge("S5a-read-before-compact", p.observe("S5a"), [("reply-marker", r.marker in reply, reply[:120])], reply)
    p.snapshot("S5")
    how = private_compact(r.ws, p.tid)
    ev = p.observe("S5")
    journal = ev["vellum"]["history"]["compaction_journal"]
    p.judge("S5-compact", ev, [
        # Official compaction is a Codex-issued summary turn, accounted for by
        # usage-matches-rollout; Vellum's journal only records its own compactions.
        ("rollout-compacted", ev["rollout"]["compactions"] > 0,
         f"compactions={ev['rollout']['compactions']} via={how} vellum_journal={journal}"),
    ], "", model=(ev["rollout"]["turn_contexts"] or [{}])[-1].get("model"))
    reply = p.ask("S5b", "不要呼叫任何工具，只憑記憶回答：notes.txt 的 marker 是什麼？只回覆 marker。", a.timeout)
    p.judge("S5b-recall-after-compact", p.observe("S5b"), [("reply-marker", r.marker in reply, reply[:160])], reply)
    r.results += p.results


def s6(r: Round, a):
    """Switch model mid-thread and back on the app-server path; each segment routes on its own model."""
    if not a.alt_model:
        print("\n=== S6-model-switch: NOT_RUN (no --alt-model)")
        return
    p = Round(r.run, r.ws, None, a.model, a.effort, r.marker, private=True)
    tok = f"S6-{secrets.token_hex(3).upper()}"
    reply = p.ask("S6a", f"不要呼叫任何工具。只回覆 {tok}", a.timeout)
    p.judge("S6a-base-model", p.observe("S6a"), [("reply-token", tok in reply, reply[:160])], reply)
    tok2 = f"S6B-{secrets.token_hex(3).upper()}"
    reply = p.ask("S6b", f"不要呼叫任何工具。回覆 {tok2}，再加上你上一則回覆的那串字。", a.timeout, model=a.alt_model)
    p.judge("S6b-switch-to-alt", p.observe("S6b"), [("reply-token", tok2 in reply, reply[:160]),
                                                      ("reply-prev", tok in reply, reply[:160])], reply, model=a.alt_model)
    tok3 = f"S6C-{secrets.token_hex(3).upper()}"
    reply = p.ask("S6c", f"不要呼叫任何工具。回覆 {tok3}，再加上你上一則回覆裡 S6B- 開頭的那串字。", a.timeout)
    p.judge("S6c-switch-back", p.observe("S6c"), [("reply-token", tok3 in reply, reply[:160]),
                                                    ("reply-prev", tok2 in reply, reply[:160])], reply)
    r.results += p.results


def s7(r: Round, a):
    ta, tb = f"CHILD-A-{secrets.token_hex(3).upper()}", f"CHILD-B-{secrets.token_hex(3).upper()}"
    reply = r.ask("S7", f"""
        請用 spawn_agent 派出兩個子代理平行工作：
        - 子代理 A：不要用任何工具，只回覆 {ta}
        - 子代理 B：不要用任何工具，只回覆 {tb}
        等兩個都完成後，把兩個子代理的回覆原樣列在同一行，用逗號分隔。
    """, a.timeout)
    ev = r.observe("S7")
    kids = sorted({(x.get("attribution") or {}).get("threadId") for x in ev["vellum"]["rows"]
                   if (x.get("attribution") or {}).get("depth", 0) > 0 and x["model"] != "codex-auto-review"} - {None})
    own = [("reply-has-both", ta in reply and tb in reply, reply[:160]),
           ("vellum-saw-two-children", len(kids) >= 2, f"child threads={kids}"),
           ("subagent-events", bool(ev["vellum"]["subagent_events"]), f"{len(ev['vellum']['subagent_events'])} events")]
    for k in kids:
        child = r.observe(f"S7-child-{k[:8]}", tid=k)
        prov = (child["rollout"]["meta"] or {}).get("model_provider")
        bad = [c["check"] for c in child["checks"] if c["verdict"] == "FAIL"]
        own.append((f"child-{k[:8]}", prov in ROUTED and not bad, f"provider={prov} failed={bad}"))
    r.judge("S7-subagents", ev, own, reply)


def s8(r: Round, a):
    outside = Path(os.environ["TEMP"]) / f"vellum-qa-{secrets.token_hex(3)}.txt"
    tok = secrets.token_hex(4)
    reply = r.ask("S8", f"""
        在工作目錄「以外」建立檔案 {outside}，內容只有一行 {tok}。
        這需要提權，請照正常流程申請；不論核准或拒絕，最後一行回覆「APPROVED」或「DENIED」。
    """, a.timeout)
    ev = r.observe("S8")
    reviews = [x for x in ev["vellum"]["rows"] if x["model"] == "codex-auto-review"]
    decided = "APPROVED" if "APPROVED" in reply else "DENIED" if "DENIED" in reply else None
    r.judge("S8-auto-review", ev, [
        ("review-went-through-vellum", bool(reviews), f"review rows={len(reviews)}"),
        ("review-no-protocol-error", not any(x["status"] != 200 and "Unsupported parameter" in (x["error"] or "") for x in reviews),
         f"{[(x['status'], (x['error'] or '')[:70]) for x in reviews if x['status'] != 200]}"),
        ("decision-reported", decided is not None, reply[-160:]),
        ("disk-matches-decision", decided is None or (decided == "APPROVED") == outside.exists(), f"exists={outside.exists()}"),
    ], reply)


def s9(r: Round, a):
    reply = r.ask("S9", "用網路搜尋查 Python 3.13.0 的正式發布日期。回覆格式：YYYY-MM-DD 加一個來源網址。", a.timeout)
    ev = r.observe("S9")
    items = ev["rollout"]["response_items"]
    r.judge("S9-web-search", ev, [("web-search-call", items.get("web_search_call", 0) > 0, f"items={items}"),
                                  ("reply-date", "2024-10-07" in reply, reply[:160])], reply)


DESKTOP_STEPS = ("S1", "S2", "S3", "S4", "S7", "S8", "S9")  # need Desktop closed once to join the QA project
STEPS = {"S1": s1, "S2": s2, "S3": s3, "S4": s4, "S5": s5, "S6": s6, "S7": s7, "S8": s8, "S9": s9}


# ---------- commands ----------

def run_dir(arg: str | None) -> Path:
    d = Path(arg) if arg else REPO / "qa" / "runs" / f"desktop-live-{time.strftime('%Y%m%d-%H%M%S')}"
    d.mkdir(parents=True, exist_ok=True)
    return d


def boot_count() -> int | None:
    try:
        return json.loads((Path(os.environ["LOCALAPPDATA"]) / "Vellum" / "boot.json").read_text(encoding="utf-8")).get("bootCount")
    except OSError:
        return None


def cmd_preflight(a) -> int:
    ok = True
    procs = sh(["powershell", "-NoProfile", "-Command",
                "Get-Process vellum-proxy-desktop,ChatGPT -ErrorAction SilentlyContinue | Select-Object -ExpandProperty ProcessName"],
               check=False).stdout.split()
    for name in ("vellum-proxy-desktop", "ChatGPT"):
        ok &= name in procs
        print(f"[{'PASS' if name in procs else 'FAIL'}] process {name} running")
    cfg = (Path.home() / ".codex" / "config.toml").read_text(encoding="utf-8")
    first = next((l for l in cfg.splitlines() if l.startswith("model_provider")), "")
    routed = '"vellum"' in first
    ok &= routed
    print(f"[{'PASS' if routed else 'FAIL'}] config.toml {first.strip() or 'model_provider missing'}")
    port = sh(["powershell", "-NoProfile", "-Command",
               "(Get-NetTCPConnection -State Listen -LocalPort 15721 -ErrorAction SilentlyContinue | Measure-Object).Count"],
              check=False).stdout.strip()
    ok &= port not in ("", "0")
    print(f"[{'PASS' if port not in ('', '0') else 'FAIL'}] proxy listening on 127.0.0.1:15721")
    ok &= BRIDGE.exists()
    print(f"[{'PASS' if BRIDGE.exists() else 'FAIL'}] bridge {BRIDGE}")
    print(f"[INFO] Vellum bootCount={boot_count()}  project={PROJECT} root={PROJECT_ROOT}")
    return 0 if ok else 1


def summary(run: Path, results: list[dict], boot_before: int | None) -> None:
    lines = [f"bootCount {boot_before} -> {boot_count()}", "", "| step | verdict | failed / warned checks |", "|---|---|---|"]
    for r in results:
        bad = ", ".join(f"{c['check']}({c['verdict']})" for c in r["checks"] if c["verdict"] in ("FAIL", "WARN", "BLOCKED", "NOT_RUN"))
        lines.append(f"| {r['scenario']} | {r['verdict']} | {bad} |")
    text = "\n".join(lines) + "\n"
    with (run / "SUMMARY.md").open("a", encoding="utf-8") as f:
        f.write(text + "\n")
    print("\n" + text + f"\nrun dir: {run}")


def cmd_round(a) -> int:
    run = run_dir(a.run_dir)
    ws, marker = make_workspace(run)
    boot_before = boot_count()
    print(f"workspace {ws}", flush=True)
    r = Round(run, ws, None, a.model, a.effort, marker)
    steps = a.only.split(",") if a.only else list(STEPS)
    if r.tid is None and any(x in steps for x in DESKTOP_STEPS[1:]) and "S1" not in steps:
        steps = ["S1"] + steps  # S1 creates the Desktop thread
    for step in steps:
        skip = None
        if step in DESKTOP_STEPS and not a.allow_close_desktop:
            skip = "Desktop path closes Desktop once; rerun with --allow-close-desktop when it is not in use"
        elif step in DESKTOP_STEPS[1:] and r.tid is None:
            skip = "S1 did not create the Desktop thread"
        if skip:
            r.results.append({"scenario": step, "thread": None, "verdict": "NOT_RUN", "reply": "",
                              "checks": [{"check": "runner", "verdict": "NOT_RUN", "detail": skip}]})
            print(f"\n=== {step}: NOT_RUN ({skip})", flush=True)
            continue
        try:
            STEPS[step](r, a)
        except BaseException as e:  # keep going: one broken step must not hide the rest
            if isinstance(e, KeyboardInterrupt):
                raise
            r.results.append({"scenario": step, "thread": r.tid, "verdict": "FAIL", "reply": "",
                              "checks": [{"check": "runner", "verdict": "FAIL", "detail": repr(e)[:300]}]})
            print(f"\n=== {step}: FAIL (runner) {e!r}", flush=True)
    summary(run, r.results, boot_before)
    return 0 if all(x["verdict"] == "PASS" for x in r.results) else 1


def cmd_probe(a) -> int:
    """App-server path: a model other than Desktop's composer choice can only be driven here."""
    run = run_dir(a.run_dir)
    ws, marker = make_workspace(run)
    p = Round(run, ws, None, a.model, a.effort, marker, private=True)
    tok = f"PONG-{secrets.token_hex(3).upper()}"
    reply = p.ask("probe", f"不要呼叫任何工具。只回覆這一串字：{tok}", a.timeout)
    ev = p.observe("probe")
    routes = sorted({x["route_id"] for x in root_rows(ev)})
    rec = p.judge(f"probe-{a.model}", ev, [("reply-token", tok in reply, reply[:120]),
                                           ("vellum-saw-request", bool(routes), f"routes={routes}")], reply)
    summary(run, p.results, None)
    return 0 if rec["verdict"] == "PASS" else 1


def cmd_observe(a) -> int:
    run = run_dir(a.run_dir)
    args = PY + [OBSERVE, "report", "--thread", a.thread, "--out", run / "obs-manual"]
    if a.baseline:
        args += ["--baseline", a.baseline]
    return subprocess.run(args).returncode


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("preflight")
    for name in ("round", "probe"):
        p = sub.add_parser(name)
        p.add_argument("--model", required=True)
        p.add_argument("--effort", default="low")
        p.add_argument("--timeout", type=float, default=900)
        p.add_argument("--run-dir")
        if name == "round":
            p.add_argument("--only", help="comma-separated steps, e.g. S1,S2,S3")
            p.add_argument("--alt-model", help="second model for S6 (switch mid-thread and back)")
            p.add_argument("--allow-close-desktop", action="store_true",
                           help="let S1 close and reopen Codex Desktop; without it only the app-server steps run")
    p = sub.add_parser("observe")
    p.add_argument("--thread", required=True)
    p.add_argument("--baseline")
    p.add_argument("--run-dir")
    a = ap.parse_args()
    return {"preflight": cmd_preflight, "round": cmd_round, "probe": cmd_probe, "observe": cmd_observe}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
