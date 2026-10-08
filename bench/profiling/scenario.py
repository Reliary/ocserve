#!/usr/bin/env python3
"""Phase-0 profiling scenario (bytehound over glibc, throwaway instance).

Isolated fake HOME (no plugins, fake provider), one session seeded with
~500 msgs of history, then mixed load: concurrent prompt_async + /message
storms. SIGTERM → bytehound dump (memory-profiling_*.dat in CWD).
Usage: python3 bh-scenario.py [duration_s]
"""
import http.server
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.request
import glob
import socket

DUR = int(sys.argv[1]) if len(sys.argv) > 1 else 40
ROOT = "/tmp/opencode/bh-run"
# No baked-in home paths (pre-commit banned-string rule): binary resolved
# relative to this file (repo layout), profiler lib via env override.
_REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OCSERVE = os.environ.get("OCSERVE_BIN", os.path.join(_REPO, "target/debug/ocserve"))
BH_SO = os.environ.get("BYTEHOUND_LIB", "/tmp/opencode/bytehound/target/release/libbytehound.so")

# ---- fake provider (SSE, final-answer only) ----
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        body = (
            'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n'
            'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
            'data: {"choices":[],"usage":{"prompt_tokens":400,'
            '"completion_tokens":5,"total_tokens":405}}\n\n'
            "data: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(f"provider :{port}", flush=True)

# ---- fake HOME ----
shutil.rmtree(ROOT, ignore_errors=True)
home = os.path.join(ROOT, "home")
data = os.path.join(ROOT, "data")
work = os.path.join(ROOT, "work")
for d in (home, data, work):
    os.makedirs(d, exist_ok=True)
cfgdir = os.path.join(home, ".config/opencode")
cachedir = os.path.join(home, ".cache/opencode")
statedir = os.path.join(home, ".local/state/opencode")
authdir = os.path.join(home, ".local/share/opencode")
for d in (cfgdir, cachedir, statedir, authdir):
    os.makedirs(d, exist_ok=True)
json.dump(
    {"provider": {"fake": {"options": {"baseURL": f"http://127.0.0.1:{port}/v1"}}}},
    open(f"{cfgdir}/opencode.json", "w"),
)
json.dump({"fake": {"type": "api", "key": "k"}}, open(f"{authdir}/auth.json", "w"))
json.dump(
    {
        "fake": {
            "id": "fake",
            "npm": "@ai-sdk/openai-compatible",
            "api": f"http://127.0.0.1:{port}/v1",
            "name": "Fake",
            "env": [],
            "models": {
                "m": {
                    "id": "m",
                    "name": "m",
                    "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.1, "cache_write": 0.2},
                    "limit": {"context": 128000, "output": 8000},
                    "tool_call": True,
                }
            },
        }
    },
    open(f"{cachedir}/models.json", "w"),
)
json.dump(
    {"recent": [{"providerID": "fake", "modelID": "m"}], "favorite": [], "variant": "default"},
    open(f"{statedir}/model.json", "w"),
)
os.chdir(work)  # bytehound dat lands in CWD

env = dict(os.environ)
env.update(
    {
        "HOME": home,
        "OCSERVE_DATA_DIR": data,
        "OCSERVE_LEGACY_SYNC": "0",
        "LD_PRELOAD": BH_SO,
        "MEMORY_PROFILER_LOG": "warn",
    }
)
proc = subprocess.Popen([OCSERVE, "serve", "--port", "14999"], env=env)
print(f"ocserve pid {proc.pid}", flush=True)

BASE = "http://127.0.0.1:14999"


def req(method, path, body=None, timeout=30):
    r = urllib.request.Request(
        BASE + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
    )
    if body is not None:
        r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            return resp.status, json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:200]


# wait ready
for _ in range(100):
    try:
        if req("GET", "/global/health")[0] == 200:
            break
    except Exception:
        pass
    time.sleep(0.2)
else:
    print("BOOT FAILED")
    proc.kill()
    sys.exit(2)
print("healthy", flush=True)

# ---- seed: session + 500 msgs × ~2KB parts (~1MB history) ----
st, s = req("POST", "/session", {"directory": work})
sid = s["id"]
db = sqlite3.connect(f"file:{data}/ocserve.db?mode=ro", uri=True)  # exists post-boot
db.close()
db = sqlite3.connect(f"{data}/ocserve.db")
now = int(time.time() * 1000)
db.execute("BEGIN")
for i in range(500):
    role = "user" if i % 2 == 0 else "assistant"
    mid = f"msg_seed{i:04d}"
    info = json.dumps(
        {
            "id": mid,
            "sessionID": sid,
            "role": role,
            "time": {"created": now + i},
            **({"model": {"id": "m", "providerID": "fake", "variant": "default"}} if role == "user" else {}),
        }
    )
    db.execute(
        "INSERT OR IGNORE INTO msg (id, session_id, role, seq, time_created, info) VALUES (?,?,?,?,?,?)",
        (mid, sid, role, i + 1, now + i, info),
    )
    text = json.dumps(
        {"id": f"prt_seed{i:04d}", "sessionID": sid, "messageID": mid, "type": "text",
         "text": f"seed line {i} " + ("x" * 2000)}
    )
    db.execute(
        "INSERT OR IGNORE INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) "
        "VALUES (?,?,?,?, 'text', ?, ?, NULL)",
        (f"prt_seed{i:04d}", mid, sid, 1, len(text), text),
    )
db.commit()
db.close()
print(f"seeded {sid} (500 msgs)", flush=True)

# ---- mixed load for DUR seconds ----
stop = threading.Event()
counts = {"prompts": 0, "messages": 0, "errors": 0}


def prompt_loop(n):
    while not stop.is_set():
        st, _ = req(
            "POST",
            f"/session/{sid}/prompt_async",
            {
                "messageID": f"msg_p{n}_{int(time.time()*1000)}",
                "parts": [{"type": "text", "text": "say ok"}],
                "model": {"providerID": "fake", "modelID": "m"},
                "agent": "build",
            },
            timeout=10,
        )
        counts["prompts" if st == 204 else "errors"] += 1
        time.sleep(0.3)


def message_loop(n):
    while not stop.is_set():
        try:
            st, _ = req("GET", f"/session/{sid}/message", timeout=20)
            counts["messages" if st == 200 else "errors"] += 1
        except Exception:
            counts["errors"] += 1
        time.sleep(0.5)


threads = [threading.Thread(target=prompt_loop, args=(i,), daemon=True) for i in range(4)]
threads += [threading.Thread(target=message_loop, args=(i,), daemon=True) for i in range(3)]
for t in threads:
    t.start()
t0 = time.time()
while time.time() - t0 < DUR:
    time.sleep(5)
    rss = "?"
    try:
        for line in open(f"/proc/{proc.pid}/status"):
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1]) // 1024
    except Exception:
        pass
    print(f"t+{int(time.time()-t0):3d}s rss={rss}MB {counts}", flush=True)
stop.set()
time.sleep(1)
for t in threads:
    t.join(timeout=3)

# ---- stop → bytehound dump ----
proc.send_signal(signal.SIGTERM)
try:
    proc.wait(timeout=20)
except subprocess.TimeoutExpired:
    proc.kill()
time.sleep(2)
dats = glob.glob(os.path.join(work, "memory-profiling_*.dat")) + glob.glob(
    os.path.join(work, "*.dat")
)
print("dats:", dats, flush=True)
srv.shutdown()
sys.exit(0 if dats else 3)
