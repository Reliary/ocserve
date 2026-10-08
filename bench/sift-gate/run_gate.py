#!/usr/bin/env python3
"""Arc49-style interleaved battery for sift-at-boundary (P1c gate).

Criteria are pre-registered in bench/sift-gate/README.md (written before any
run). This driver only executes: 3 tasks x 3 seeds x {baseline, gate},
interleaved, one fresh ocserve process per run (cwd = pristine fixture copy,
throwaway data dir), opencode-go/mimo-v2.6-flash both conditions.

Usage (repo root):  python3 bench/sift-gate/run_gate.py
Output: bench/sift-gate/results/runs.jsonl + verdict printed + verdict.json
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import statistics
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

OCSERVE = os.environ.get("OCSERVE_BIN", "target/debug/ocserve")
if not os.path.isabs(OCSERVE):
    # boot() sets cwd=workdir — resolve the binary NOW, not at spawn time
    OCSERVE = os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        OCSERVE,
    )
MODEL = {"providerID": "opencode-go", "modelID": "mimo-v2.6-flash"}
BASE_PORT = 4930
RUN_TIMEOUT = 600.0  # multi-turn rule: >=300s
ROOT = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(ROOT))
WORK_ROOT = "/tmp/siftgate/runs"
RESULTS = os.path.join(ROOT, "results")

TASKS = ["t1", "t2", "t3"]
SEEDS = [1, 2, 3]
T1_FROZEN = [
    "test_validate_empty",
    "test_validate_missing_key",
    "test_validate_none_value",
    "test_user_age_none",
    "test_table_short_row",
]

PROMPTS = {
    "t1": (
        "Run the test suite in this directory with `python3 -m unittest -v`. "
        "All failures are bugs in parser.py, validator.py and formatter.py. "
        "Fix them by editing ONLY those source files — NEVER edit "
        "test_pipeline.py. Re-run the suite until it is green. Do not ask "
        "questions; proceed."
    ),
    "t2": (
        "Run `python3 t2_dump.py` in this directory (run it directly — do "
        "not pipe, filter, or redirect its output) and read the full report. "
        "Then reply with exactly one line of the form "
        "total=<N> RETRY=<N> ERROR=<N> using the counts from the report's "
        "summary. Do not ask questions; proceed."
    ),
    "t3": (
        "Run `python3 app.py data.jsonl` in this directory — it crashes. "
        "Fix app.py (NEVER edit data.jsonl) until it prints the report "
        "cleanly with exit code 0. Do not ask questions; proceed."
    ),
}


def http(method: str, url: str, body=None, timeout=10):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url, data=data, method=method,
        headers={"Content-Type": "application/json"} if data else {},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read()
        return resp.status, (json.loads(raw) if raw else None)


def sha256(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def fixture_hash(task: str) -> str:
    """Immutable inputs that must NOT be edited by the model."""
    if task == "t1":
        return sha256(os.path.join(ROOT, "fixtures/t1/test_pipeline.py"))
    if task == "t3":
        return sha256(os.path.join(ROOT, "fixtures/t3/data.jsonl"))
    return ""


def prepare_workdir(task: str, run_id: str) -> str:
    wd = os.path.join(WORK_ROOT, run_id)
    shutil.rmtree(wd, ignore_errors=True)
    os.makedirs(wd)
    fx = os.path.join(ROOT, "fixtures")
    if task in ("t1", "t3"):
        for name in os.listdir(os.path.join(fx, task)):
            shutil.copy(os.path.join(fx, task, name), wd)
    else:
        shutil.copy(os.path.join(fx, "t2_dump.py"), wd)
    return wd


def boot(port: int, cwd: str, data_dir: str, sift: bool):
    env = {k: v for k, v in os.environ.items() if k != "OCSERVE_SIFT"}
    env["OCSERVE_DATA_DIR"] = data_dir
    env["OCSERVE_LEGACY_SYNC"] = "0"
    env["PATH"] = os.path.expanduser("~/.local/bin") + ":" + env.get("PATH", "")
    if sift:
        env["OCSERVE_SIFT"] = "auto"
    log = open(os.path.join(data_dir, "serve.log"), "w")
    proc = subprocess.Popen(
        [OCSERVE, "serve", "--port", str(port)],
        cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT,
        start_new_session=True,  # own group → stop() can killpg the tree
    )
    return proc, log


def wait_health(port: int, proc, timeout=25.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return False
        try:
            http("GET", f"http://127.0.0.1:{port}/global/health", timeout=2)
            return True
        except Exception:
            time.sleep(0.25)
    return False


def stop(proc, log):
    if proc.poll() is None:
        # kill the whole GROUP — terminate() alone left ocserve's plugin-host
        # node child orphaned (census 2026-10-04: 34 hosts / 717MB wasted).
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            proc.terminate()
        try:
            proc.wait(timeout=5)
        except Exception:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError, OSError):
                proc.kill()
    try:
        log.close()
    except Exception:
        pass


class Responder(threading.Thread):
    """Auto-answer permission asks (edit=ask) and reject question-tool asks."""

    def __init__(self, port: int):
        super().__init__(daemon=True)
        self.port = port
        self.stop_evt = threading.Event()
        self.replied = {"always": 0, "reject": 0, "questions": 0}

    def run(self):
        while not self.stop_evt.is_set():
            try:
                _, perms = http("GET", f"http://127.0.0.1:{self.port}/permission", timeout=3)
                for p in perms or []:
                    pid = p.get("id")
                    if not pid:
                        continue
                    action = "reject" if p.get("action") == "doom_loop" else "always"
                    try:
                        http(
                            "POST",
                            f"http://127.0.0.1:{self.port}/permission/{pid}/reply",
                            {"reply": action}, timeout=3,
                        )
                        self.replied[action] += 1
                    except Exception:
                        pass
            except Exception:
                pass
            try:
                _, qs = http("GET", f"http://127.0.0.1:{self.port}/question", timeout=3)
                for q in qs or []:
                    qid = q.get("id")
                    if qid:
                        try:
                            http("POST", f"http://127.0.0.1:{self.port}/question/{qid}/reject",
                                 timeout=3)
                            self.replied["questions"] += 1
                        except Exception:
                            pass
            except Exception:
                pass
            self.stop_evt.wait(0.2)


def wait_done(port: int, sid: str, deadline: float):
    """Wait for a final assistant message (finish not tool-calls)."""
    while time.time() < deadline:
        try:
            _, msgs = http("GET", f"http://127.0.0.1:{port}/session/{sid}/message?limit=300",
                           timeout=5)
            items = msgs if isinstance(msgs, list) else msgs.get("messages", [])
            final_text = []
            done = False
            for m in items:
                info = m.get("info", m)
                if info.get("role") != "assistant":
                    continue
                fin = info.get("finish")
                if fin and fin != "tool-calls":
                    done = True
                    for p in m.get("parts", []):
                        if p.get("type") == "text":
                            final_text.append(p.get("text", ""))
            if done:
                return True, " ".join(final_text)
        except Exception:
            pass
        time.sleep(0.3)
    return False, ""


def parse_metrics(text: str) -> dict:
    out = {"compressed": 0, "raw": 0, "bytes_in": 0, "bytes_out": 0}
    for line in text.splitlines():
        m = re.match(r'ocserve_sift_total\{outcome="(\w+)"\} (\d+)', line)
        if m:
            out[m.group(1)] = int(m.group(2))
        m = re.match(r'ocserve_sift_bytes_total\{direction="(\w+)"\} (\d+)', line)
        if m:
            out["bytes_" + ("in" if m.group(1) == "in" else "out")] = int(m.group(2))
    return out


def score(task: str, workdir: str, final_text: str) -> tuple[float, dict]:
    checks = {}
    if task == "t1":
        if fixture_hash("t1") != sha256(os.path.join(workdir, "test_pipeline.py")):
            return 0.0, {"tampered": True}
        try:
            r = subprocess.run(
                ["python3", "-m", "unittest", "-v"],
                cwd=workdir, capture_output=True, text=True, timeout=120,
            )
            out = r.stdout + r.stderr
        except Exception as e:
            return 0.0, {"scorer_error": str(e)}
        fails = set(re.findall(r"(test_\w+) \(.*?\) \.\.\. (?:FAIL|ERROR)", out))
        for t in T1_FROZEN:
            checks[t] = t not in fails
        return sum(checks.values()) / len(checks), checks
    if task == "t2":
        checks["total=2500"] = "total=2500" in final_text
        checks["RETRY=26"] = "RETRY=26" in final_text
        checks["ERROR=11"] = "ERROR=11" in final_text
        return sum(checks.values()) / len(checks), checks
    if task == "t3":
        if fixture_hash("t3") != sha256(os.path.join(workdir, "data.jsonl")):
            return 0.0, {"tampered": True}
        try:
            r = subprocess.run(
                ["python3", "app.py", "data.jsonl"],
                cwd=workdir, capture_output=True, text=True, timeout=60,
            )
            out = r.stdout + r.stderr
            checks["exit0"] = r.returncode == 0
        except Exception as e:
            return 0.0, {"scorer_error": str(e)}
        checks["records=1500"] = "records=1500" in out
        checks["FAIL=300"] = "FAIL=300" in out
        checks["OK=1200"] = "OK=1200" in out
        return sum(checks.values()) / len(checks), checks
    raise ValueError(task)


def fetch_metrics_text(port: int) -> str:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=5) as r:
            return r.read().decode("utf-8", "replace")
    except Exception:
        return ""


def run_one(task: str, seed: int, cond: str, port: int, attempt: int) -> dict:
    run_id = f"{task}-s{seed}-{cond}-a{attempt}"
    workdir = prepare_workdir(task, run_id)
    data_dir = os.path.join(workdir, ".ocserve-data")
    os.makedirs(data_dir, exist_ok=True)
    row = {
        "run": run_id, "task": task, "seed": seed, "cond": cond,
        "attempt": attempt, "port": port, "infra": False, "died": False,
        "score": 0.0, "checks": {}, "wc": None, "wc_input": None,
        "wc_output": None, "compressed": 0, "raw": 0,
        "bytes_in": 0, "bytes_out": 0, "note": "", "final_text": "",
        "duration_s": None, "perm": {},
    }
    proc, log = boot(port, workdir, data_dir, sift=(cond == "gate"))
    t0 = time.time()
    responder = None
    try:
        if not wait_health(port, proc):
            row["infra"] = True
            row["note"] = "boot failed/timeout"
            if proc.poll() is not None:
                row["died"] = True
            return row
        responder = Responder(port)
        responder.start()
        _, sess = http("POST", f"http://127.0.0.1:{port}/session",
                       {"title": run_id, "directory": workdir})
        sid = sess["id"]
        st, _ = http(
            "POST", f"http://127.0.0.1:{port}/session/{sid}/prompt_async",
            {
                "messageId": f"msg_{uuid.uuid4().hex[:24]}",
                "parts": [{"type": "text", "text": PROMPTS[task]}],
                "model": MODEL,
                "agent": "build",
            },
        )
        if st != 204:
            row["infra"] = True
            row["note"] = f"prompt_async status {st}"
            return row
        done, final_text = wait_done(port, sid, time.time() + RUN_TIMEOUT)
        row["final_text"] = final_text[-400:]
        if not done:
            row["infra"] = True
            row["note"] = "prompt timeout (no final assistant)"
            if proc.poll() is not None:
                row["died"] = True
            return row
        mtext = fetch_metrics_text(port)
        row.update(parse_metrics(mtext))
        _, sess = http("GET", f"http://127.0.0.1:{port}/session/{sid}")
        toks = (sess or {}).get("tokens", {})
        row["wc_input"] = int(toks.get("input", 0))
        row["wc_output"] = int(toks.get("output", 0))
        row["wc"] = row["wc_input"] + 4 * row["wc_output"]
        s, checks = score(task, workdir, final_text)
        row["score"] = s
        row["checks"] = checks
        row["duration_s"] = round(time.time() - t0, 1)
        if responder:
            row["perm"] = dict(responder.replied)
        return row
    except urllib.error.HTTPError as e:
        row["infra"] = True
        row["note"] = f"http {e.code}: {e.reason}"
        return row
    except Exception as e:
        row["infra"] = True
        row["note"] = f"error: {e}"
        if proc.poll() is not None:
            row["died"] = True
        return row
    finally:
        if responder:
            responder.stop_evt.set()
        stop(proc, log)


def main() -> int:
    if not os.path.exists(OCSERVE):
        print(f"missing binary {OCSERVE} (cargo build -p ocserve-cli)", file=sys.stderr)
        return 2
    os.makedirs(RESULTS, exist_ok=True)
    os.makedirs(WORK_ROOT, exist_ok=True)
    rows = []
    port = BASE_PORT
    # interleaved: baseline/gate back-to-back, first-mover alternates
    for ti, task in enumerate(TASKS):
        for seed in SEEDS:
            order = ["baseline", "gate"] if (ti + seed) % 2 else ["gate", "baseline"]
            for cond in order:
                port += 1
                print(f"[{len(rows)+1:2d}/18] {task} s{seed} {cond} ...", flush=True)
                row = run_one(task, seed, cond, port, attempt=1)
                if row["infra"]:
                    print(f"    infra: {row['note']} -> retry", flush=True)
                    port += 1
                    retry = run_one(task, seed, cond, port, attempt=2)
                    rows.append(row)  # keep failed attempt visible
                    row = retry
                print(
                    f"    score={row['score']:.2f} wc={row['wc']} "
                    f"compressed={row['compressed']} infra={row['infra']} "
                    f"note={row['note']!r} {row.get('duration_s')}s",
                    flush=True,
                )
                rows.append(row)
                with open(os.path.join(RESULTS, "runs.jsonl"), "a") as f:
                    f.write(json.dumps(row) + "\n")

    # last attempt per (task, seed, cond) = the run of record
    final_map = {}
    for r in rows:
        k = (r["task"], r["seed"], r["cond"])
        if k not in final_map or r["attempt"] > final_map[k]["attempt"]:
            final_map[k] = r
    final = list(final_map.values())

    base = [r for r in final if r["cond"] == "baseline" and not r["infra"]]
    gate = [r for r in final if r["cond"] == "gate" and not r["infra"]]
    infra_runs = [r for r in final if r["infra"]]

    wc_b = sum(r["wc"] for r in base) if base else None
    wc_g = sum(r["wc"] for r in gate) if gate else None
    scores_b = sorted(r["score"] for r in base)
    scores_g = sorted(r["score"] for r in gate)
    med_b = statistics.median(scores_b) if scores_b else None
    med_g = statistics.median(scores_g) if scores_g else None
    engaged = sum(1 for r in gate if r["compressed"] > 0)

    criteria = {}
    if len(gate) >= 1:
        criteria["engagement"] = engaged >= (2 * len(gate) + 2) // 3
    else:
        criteria["engagement"] = False
    criteria["cost"] = bool(wc_b and wc_g is not None and wc_g <= 0.85 * wc_b)
    criteria["score"] = bool(
        med_b is not None and med_g is not None and med_g >= med_b
    )
    criteria["no_panics"] = not any(r["died"] for r in final)
    verdict = "PASS" if all(criteria.values()) else "FAIL"
    if not gate or not base:
        verdict = "INVALID"

    paired = {}
    for r in final:
        if not r["infra"]:
            paired.setdefault((r["task"], r["seed"]), {})[r["cond"]] = r
    pair_ratios = []
    for k, v in paired.items():
        if "baseline" in v and "gate" in v and v["baseline"]["wc"]:
            pair_ratios.append({
                "task": k[0], "seed": k[1],
                "ratio": round(v["gate"]["wc"] / v["baseline"]["wc"], 4),
            })

    out = {
        "verdict": verdict,
        "criteria": criteria,
        "wc_baseline_total": wc_b,
        "wc_gate_total": wc_g,
        "wc_drop_pct": (round(100 * (1 - wc_g / wc_b), 1)
                        if wc_b and wc_g is not None else None),
        "score_median_baseline": med_b,
        "score_median_gate": med_g,
        "engaged_gate_runs": f"{engaged}/{len(gate)}",
        "infra_runs": len(infra_runs),
        "paired_ratios": pair_ratios,
        "runs": rows,
    }
    with open(os.path.join(RESULTS, "verdict.json"), "w") as f:
        json.dump(out, f, indent=1)
    print(json.dumps({k: v for k, v in out.items() if k != "runs"}, indent=1))
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
