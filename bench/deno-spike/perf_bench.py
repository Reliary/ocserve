#!/usr/bin/env python3
"""Interleaved performance A/B/C: bun vs node vs deno plugin hosts.

Fresh process per run; runtimes round-robined per round to control for
system drift (interleave rule). Workloads per run:

  boot   spawn -> pong                          (module graph eval)
  load   3 real plugins -> hook sets            (parse + link + exec)
  chat   100x chat.message RTT, small payload   (per-turn hot path)
  xform  50x experimental.chat.messages.transform with ~120KB conversation
         (fires once per prompt — the realistic pipe payload)
  event  20x event delivery                      (fire-and-forget fanout)
  cpu    utime+stime of the host over the whole run

Usage: python3 perf_bench.py [rounds]     (default 5)
Output: per-op latencies + aggregates to stdout AND perf-results.jsonl
"""
import json
import os
import statistics
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
REFINE = os.path.expanduser("~/src/refine")
HOST = f"{REFINE}/crates/refine-plugin/src/host/host.mjs"
DENO = "/home/linuxbrew/.linuxbrew/bin/deno"
BUN = os.path.expanduser("~/.bun/bin/bun")
MC_ORIG = (f"{HOME}/.cache/opencode/packages/@cortexkit/"
           "opencode-magic-context@latest/node_modules/"
           "@cortexkit/opencode-magic-context/dist/index.js")
MC_DENO = MC_ORIG[:-len("index.js")] + "index.deno.js"
CODEX = (f"{HOME}/.cache/opencode/packages/@iam-brain/"
         "opencode-codex-auth@latest/node_modules/"
         "@iam-brain/opencode-codex-auth/dist/index.js")
RELIARY = f"{HOME}/src/reliary8/opencode-plugin/dist/index.js"

RUNTIMES = {
    "bun": [BUN, "--smol", HOST],
    "node": ["node", "--max-old-space-size=128", "--max-semi-space-size=2", HOST],
    "deno": [DENO, "run", "-A", "--unstable-ffi",
             "--node-modules-dir=manual", HOST],
}

INPUT = {"directory": REFINE, "projectID": "global",
         "serverUrl": "http://127.0.0.1:9"}


def big_conversation(n_msgs=120):
    """~120KB realistic accumulated conversation for the transform hook."""
    msgs = []
    for i in range(n_msgs):
        role = "user" if i % 2 == 0 else "assistant"
        msgs.append({
            "role": role,
            "content": f"[{i}] " + ("lorem ipsum dolor sit amet " * 18)[:640],
        })
    return {"messages": msgs}


class Host:
    def __init__(self, runtime):
        self.proc = subprocess.Popen(
            RUNTIMES[runtime], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            env={**os.environ,
                 "DENO_DIR": os.path.expanduser("~/.cache/deno")})
        self._id = 0

    def _ticks(self):
        """utime+stime in clock ticks from /proc/pid/stat."""
        try:
            with open(f"/proc/{self.proc.pid}/stat") as f:
                parts = f.read().rsplit(")", 1)[1].split()
            return int(parts[11]) + int(parts[12])  # utime, stime
        except Exception:
            return -1

    def rpc(self, method, params=None, timeout=90):
        self._id += 1
        rid = self._id
        t0 = time.perf_counter()
        self.proc.stdin.write(json.dumps(
            {"id": rid, "method": method, "params": params}) + "\n")
        self.proc.stdin.flush()
        deadline = time.perf_counter() + timeout
        while time.perf_counter() < deadline:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError(f"eof from host during {method}")
            msg = json.loads(line)
            if msg.get("id") == rid:
                dt = (time.perf_counter() - t0) * 1000
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result"), dt
        raise TimeoutError(method)

    def close(self):
        try:
            self.proc.stdin.write(
                json.dumps({"id": 99999, "method": "dispose"}) + "\n")
            self.proc.stdin.flush()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def run_once(runtime):
    out = {"runtime": runtime}
    h = Host(runtime)
    ticks0 = h._ticks()

    _, dt = h.rpc("ping")
    out["boot_ms"] = round(dt, 2)

    entries = {"codex-auth": CODEX, "reliary8": RELIARY}
    entries["magic-context"] = MC_DENO if runtime == "deno" else MC_ORIG
    loads = {}
    for spec, entry in entries.items():
        res, dt = h.rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
        loads[spec] = {"ms": round(dt, 2), "hooks": len(res["hooks"])}
    out["load"] = loads
    out["load_total_ms"] = round(sum(v["ms"] for v in loads.values()), 2)

    # warmup (JIT/graph)
    for _ in range(10):
        h.rpc("trigger", {"name": "chat.message",
                          "input": {"sessionID": "b",
                                    "message": {"role": "user"}},
                          "output": {}})

    chat = [h.rpc("trigger", {"name": "chat.message",
                              "input": {"sessionID": "b", "message":
                                        {"role": "user", "parts":
                                         [{"type": "text", "text": "hi"}]}},
                              "output": {}})[1] for _ in range(100)]

    conv = big_conversation()
    xform = [h.rpc("trigger", {"name": "experimental.chat.messages.transform",
                               "input": {}, "output": dict(conv)})[1]
             for _ in range(50)]

    event = [h.rpc("event", {"event": {"id": f"e{i}",
                                       "type": "session.created",
                                       "properties": {}}})[1]
             for i in range(20)]

    ticks1 = h._ticks()
    h.close()

    def agg(v):
        sv = sorted(v)
        return {"p50": round(statistics.median(sv), 2),
                "p95": round(sv[int(len(sv) * 0.95) - 1], 2),
                "max": round(sv[-1], 2)}
    out["chat_ms"] = agg(chat)
    out["xform_ms"] = agg(xform)
    out["event_ms"] = agg(event)
    if ticks0 >= 0 and ticks1 >= ticks0:
        out["cpu_s"] = round((ticks1 - ticks0) / 100.0, 2)
    return out


def main():
    rounds = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    order = list(RUNTIMES)
    results = []
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                           "perf-results.jsonl"), "a") as f:
        for r in range(rounds):
            order = order[1:] + order[:1]  # rotate — interleave
            for rt in order:
                res = run_once(rt)
                res["round"] = r
                results.append(res)
                f.write(json.dumps(res) + "\n")
                f.flush()
                print(f"r{r} {rt:5} boot={res['boot_ms']:7.2f} "
                      f"load={res['load_total_ms']:7.2f} "
                      f"chat p50={res['chat_ms']['p50']:6.2f} "
                      f"p95={res['chat_ms']['p95']:6.2f} "
                      f"xform p50={res['xform_ms']['p50']:6.2f} "
                      f"cpu={res.get('cpu_s', -1):5.2f}s", flush=True)

    print("\n=== aggregates (median of runs) ===")
    for rt in RUNTIMES:
        rs = [x for x in results if x["runtime"] == rt]
        def med(path):
            vals = []
            for x in rs:
                v = x
                for k in path:
                    v = v[k]
                vals.append(v)
            return round(statistics.median(vals), 2)
        print(f"{rt:5} boot={med(['boot_ms']):7.2f}ms "
              f"load={med(['load_total_ms']):7.2f}ms "
              f"chat p50={med(['chat_ms','p50']):6.2f} "
              f"p95={med(['chat_ms','p95']):6.2f} "
              f"xform p50={med(['xform_ms','p50']):6.2f} "
              f"p95={med(['xform_ms','p95']):6.2f} "
              f"cpu={med(['cpu_s']):5.2f}s")


if __name__ == "__main__":
    main()
