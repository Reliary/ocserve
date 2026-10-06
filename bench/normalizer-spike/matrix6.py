#!/usr/bin/env python3
"""Phase 1 — 6-cell decision matrix: {raw,norm} x {bun,node,deno}.

Memory axis (the gap): 50 ms RSS sampler -> warm-up peak, post-load,
post-burst (500 mixed triggers), settled at t+60s / t+180s after last
activity. Plus boot/load/trigger medians as cross-check.

Pre-registered: STRESS.md (M1 norm <= raw+10%, M2 burst recovery <=20%,
M3 all loads succeed). 2 rounds/cell, rotated; anomalies re-run x3.

Usage: python3 matrix6.py [rounds]     (default 2)
"""
import json
import os
import statistics
import subprocess
import sys
import threading
import time

HOME = os.path.expanduser("~")
REFINE = os.path.expanduser("~/src/refine")
HOST = f"{REFINE}/crates/refine-plugin/src/host/host.mjs"
DENO = "/home/linuxbrew/.linuxbrew/bin/deno"
BUN = f"{HOME}/.bun/bin/bun"
MC = (f"{HOME}/.cache/opencode/packages/@cortexkit/"
      "opencode-magic-context@latest/node_modules/"
      "@cortexkit/opencode-magic-context/dist")
CA = (f"{HOME}/.cache/opencode/packages/@iam-brain/"
      "opencode-codex-auth@latest/node_modules/"
      "@iam-brain/opencode-codex-auth/dist")
REL = f"{HOME}/src/reliary8/opencode-plugin/dist"

SPAWNS = {
    "bun": [BUN, "--smol", HOST],
    "node": ["node", "--max-old-space-size=128", "--max-semi-space-size=2", HOST],
    "deno": [DENO, "run", "-A", "--unstable-ffi", "--node-modules-dir=manual", HOST],
}
RAW_MAGIC = {"bun": f"{MC}/index.js", "node": f"{MC}/index.js",
             "deno": f"{MC}/index.deno.js"}
INPUT = {"directory": REFINE, "projectID": "global",
         "serverUrl": "http://127.0.0.1:9"}

XFORM_CONV = {"messages": [
    {"role": "user" if i % 2 == 0 else "assistant",
     "content": f"[{i}] " + ("lorem ipsum dolor sit amet " * 12)[:280]}
    for i in range(40)]}  # ~30KB conversation for the burst


def entries(runtime, norm):
    if norm:
        return [("codex-auth", f"{CA}/index.normalized.mjs"),
                ("magic-context", f"{MC}/index.normalized.mjs"),
                ("reliary8", f"{REL}/index.normalized.mjs")]
    return [("codex-auth", f"{CA}/index.js"),
            ("magic-context", RAW_MAGIC[runtime]),
            ("reliary8", f"{REL}/index.js")]


def rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception:
        return 0
    return 0


class Sampler(threading.Thread):
    def __init__(self, pid, interval=0.05):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval = interval
        self.peak = 0
        self._stop = threading.Event()

    def run(self):
        while not self._stop.is_set():
            kb = rss_kb(self.pid)
            if kb > self.peak:
                self.peak = kb
            self._stop.wait(self.interval)

    def stop(self):
        self._stop.set()
        self.join(timeout=2)


def run_cell(runtime, norm):
    variant = "norm" if norm else "raw"
    out = {"cell": f"{runtime}-{variant}", "runtime": runtime,
           "variant": variant}
    t_req = [0.0]

    p = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True)
    sampler = Sampler(p.pid)
    sampler.start()

    def ticks():
        try:
            with open(f"/proc/{p.pid}/stat") as f:
                parts = f.read().rsplit(")", 1)[1].split()
            return int(parts[11]) + int(parts[12])
        except Exception:
            return -1

    def rpc(method, params=None, timeout=90):
        rpc._id = getattr(rpc, "_id", 0) + 1
        rid = rpc._id
        t0 = time.perf_counter()
        p.stdin.write(json.dumps({"id": rid, "method": method,
                                  "params": params}) + "\n")
        p.stdin.flush()
        deadline = time.perf_counter() + timeout
        while time.perf_counter() < deadline:
            line = p.stdout.readline()
            if not line:
                raise RuntimeError(f"eof during {method}")
            m = json.loads(line)
            if m.get("id") == rid:
                dt = (time.perf_counter() - t0) * 1000
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result"), dt
        raise TimeoutError(method)

    try:
        t_cpu0 = ticks()
        _, dt = rpc("ping")
        out["boot_ms"] = round(dt, 2)
        loads = []
        for spec, entry in entries(runtime, norm):
            _, dt = rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
            loads.append(dt)
        out["load_ms"] = round(sum(loads), 2)
        time.sleep(0.3)
        out["rss_post_load_mb"] = round(rss_kb(p.pid) / 1024, 1)

        # warmup
        for _ in range(10):
            rpc("trigger", {"name": "chat.message",
                            "input": {"sessionID": "m", "message":
                                      {"role": "user", "parts":
                                       [{"type": "text", "text": "w"}]}},
                            "output": {}})
        # burst: 500 mixed = 250 chat + 200 xform + 50 event
        chat, xf, ev = [], [], []
        for i in range(500):
            if i % 10 == 9:
                _, dt = rpc("event", {"event": {"id": f"e{i}",
                                                "type": "session.created",
                                                "properties": {}}})
                ev.append(dt)
            elif i % 2 == 0:
                _, dt = rpc("trigger", {"name": "chat.message",
                                        "input": {"sessionID": "m", "message":
                                                  {"role": "user", "parts":
                                                   [{"type": "text", "text": f"m{i}"}]}},
                                        "output": {}})
                chat.append(dt)
            else:
                _, dt = rpc("trigger",
                            {"name": "experimental.chat.messages.transform",
                             "input": {}, "output": dict(XFORM_CONV)})
                xf.append(dt)
        t_last = time.time()
        out["burst_chat_p50"] = round(statistics.median(chat), 3)
        out["burst_xform_p50"] = round(statistics.median(xf), 3)
        out["burst_event_p50"] = round(statistics.median(ev), 3)
        time.sleep(0.3)
        out["rss_post_burst_mb"] = round(rss_kb(p.pid) / 1024, 1)

        # settled at +60 and +180 after last activity
        remain = 60 - (time.time() - t_last)
        if remain > 0:
            time.sleep(remain)
        out["rss_settled_60_mb"] = round(rss_kb(p.pid) / 1024, 1)
        remain = 180 - (time.time() - t_last)
        if remain > 0:
            time.sleep(remain)
        out["rss_settled_180_mb"] = round(rss_kb(p.pid) / 1024, 1)
        t_cpu1 = ticks()
        if t_cpu0 >= 0 and t_cpu1 >= t_cpu0:
            out["cpu_s"] = round((t_cpu1 - t_cpu0) / 100.0, 2)
    except Exception as e:
        out["fatal"] = str(e)
    finally:
        sampler.stop()
        out["warmup_peak_mb"] = round(sampler.peak / 1024, 1)
        try:
            p.stdin.write(json.dumps({"id": 99999, "method": "dispose"}) + "\n")
            p.stdin.flush()
            p.wait(timeout=10)
        except Exception:
            p.kill()
    return out


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 2
    cells = [(rt, norm) for rt in ["bun", "node", "deno"]
             for norm in [False, True]]
    results = []
    out_path = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                            "matrix-results.jsonl")
    with open(out_path, "a") as f:
        for r in range(rounds):
            cells = cells[2:] + cells[:2]  # rotate
            for rt, norm in cells:
                res = run_cell(rt, norm)
                res["round"] = r
                results.append(res)
                f.write(json.dumps(res) + "\n")
                f.flush()
                if "fatal" in res:
                    print(f"r{r} {res['cell']:12} FATAL {res['fatal'][:80]}",
                          flush=True)
                else:
                    print(f"r{r} {res['cell']:12} peak={res['warmup_peak_mb']:6.1f} "
                          f"load={res['rss_post_load_mb']:6.1f} "
                          f"burst={res['rss_post_burst_mb']:6.1f} "
                          f"s60={res['rss_settled_60_mb']:6.1f} "
                          f"s180={res['rss_settled_180_mb']:6.1f} "
                          f"cpu={res.get('cpu_s',-1):5.2f}s", flush=True)

    # ---- verdicts per STRESS.md ----
    print("\n=== M1: norm settled+180 <= raw+10% (same runtime) ===")
    m1 = True
    for rt in ["bun", "node", "deno"]:
        raw = [x for x in results if x["runtime"] == rt and x["variant"] == "raw"
               and "fatal" not in x]
        norm = [x for x in results if x["runtime"] == rt and x["variant"] == "norm"
                and "fatal" not in x]
        if not raw or not norm:
            print(f"  {rt}: MISSING DATA"); m1 = False; continue
        r180 = statistics.median([x["rss_settled_180_mb"] for x in raw])
        n180 = statistics.median([x["rss_settled_180_mb"] for x in norm])
        bound = r180 * 1.10
        ok = n180 <= bound
        m1 = m1 and ok
        print(f"  {rt}: raw {r180:.1f} -> norm {n180:.1f} MB "
              f"({'PASS' if ok else 'FAIL'} bound {bound:.1f})")

    print("\n=== M2: post-burst recovers within 20% of post-load ===")
    m2 = True
    for x in results:
        if "fatal" in x:
            continue
        limit = x["rss_post_load_mb"] * 1.20
        # use settled_60 as the recovery point (gives GC 60s)
        ok = x["rss_settled_60_mb"] <= limit
        if not ok:
            m2 = False
            print(f"  {x['cell']} r{x['round']}: load {x['rss_post_load_mb']} "
                  f"burst {x['rss_post_burst_mb']} -> s60 "
                  f"{x['rss_settled_60_mb']} > limit {limit:.1f}")
    if m2:
        print("  all runs PASS")

    print("\n=== M3: all loads succeed in every cell ===")
    m3 = all("fatal" not in x for x in results) and len(results) == rounds * 6
    print(f"  {'PASS' if m3 else 'FAIL'} ({len(results)}/{rounds*6} runs, "
          f"{sum(1 for x in results if 'fatal' in x)} fatals)")

    print(f"\nM1={m1} M2={m2} M3={m3} -> {out_path}")


if __name__ == "__main__":
    main()
