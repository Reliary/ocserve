#!/usr/bin/env python3
"""OOM-reload lab (K-OOM-RESILIENCE Phase B1): leak-vs-fragmentation verdict
for the hourly-catalog-reload RSS step, plus glibc-tunables A/B.

Isolated fake HOME (no MCP/plugin children — bytehound stays in refine),
FULL-SIZE real models catalog as fixtures (a tiny fixture would make the
profile lie), forced real-content reloads by swapping fixture content.
A2's reconcile INFO log ("hot-reload ok … rss X → Y") is the step metric.

Usage:
  python3 oom_reload.py [plain|arena2|arena1] [n_reloads] [profile]
  profile=1 → LD_PRELOAD bytehound (attribution run → memory-profiling_*.dat)

Verdict logic (REPORT):
  step sizes  plain vs tunables → zero-code unit-Environment fix possible?
  bytehound only_leaked groups   → LEAK (named holder) vs fragmentation
                                   (freed-but-resident churn).
"""
import glob
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.request

VARIANT = sys.argv[1] if len(sys.argv) > 1 else "plain"
RELOADS = int(sys.argv[2]) if len(sys.argv) > 2 else 3
PROFILE = (sys.argv[3] if len(sys.argv) > 3 else "0") == "1"

ROOT = "/tmp/opencode/oom-lab"
_REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
REFINE = os.environ.get("REFINE_BIN", os.path.join(_REPO, "target/debug/refine"))
BH_SO = os.environ.get(
    "BYTEHOUND_LIB", "/tmp/opencode/bytehound/target/release/libbytehound.so"
)
REAL_CATALOG = os.path.expanduser("~/.cache/opencode/models.json")
PORT = 14998


def rss_mb(pid):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) // 1024
    except Exception:
        pass
    return -1


# ---- isolated fake HOME ----
shutil.rmtree(ROOT, ignore_errors=True)
home = os.path.join(ROOT, "home")
data = os.path.join(ROOT, "data")
work = os.path.join(ROOT, "work")
for d in (
    home,
    data,
    work,
    os.path.join(home, ".config/opencode"),
    os.path.join(home, ".cache/opencode"),
    os.path.join(home, ".local/state/opencode"),
    os.path.join(home, ".local/share/opencode"),
):
    os.makedirs(d, exist_ok=True)
# minimal config → no MCP/plugin children (keeps bytehound on refine alone)
json.dump({}, open(os.path.join(home, ".config/opencode/opencode.json"), "w"))
json.dump(
    {"recent": [], "favorite": [], "variant": "default"},
    open(os.path.join(home, ".local/state/opencode/model.json"), "w"),
)

# FULL-SIZE fixtures: real catalog bytes ×2, one field mutated (content must
# differ or watch's hash-skip correctly refuses to reload).
with open(REAL_CATALOG, "rb") as f:
    cat_a = f.read()
cat_b = cat_a.replace(b'"Big Pickle"', b'"Big PickleX"', 1)
if cat_a == cat_b:
    raise SystemExit("fixture mutation failed (Big Pickle not found)")
fixture = os.path.join(home, ".cache/opencode/models.json")
open(fixture, "wb").write(cat_a)
print(f"catalog fixture: {len(cat_a)} bytes (b: {len(cat_b)})", flush=True)

env = dict(os.environ)
env.update(
    {
        "HOME": home,
        "REFINE_DATA_DIR": data,
        "REFINE_LEGACY_SYNC": "0",
        "OPENCODE_DISABLE_MODELS_FETCH": "1",  # never touch the real cache
        "MEMORY_PROFILER_LOG": "warn",
    }
)
if PROFILE:
    env["LD_PRELOAD"] = BH_SO
if VARIANT == "arena2":
    env["MALLOC_ARENA_MAX"] = "2"
elif VARIANT == "arena1":
    env["MALLOC_ARENA_MAX"] = "1"

log_path = os.path.join(work, "lab.log")
logf = open(log_path, "w")
os.chdir(work)  # bytehound dat lands in CWD
proc = subprocess.Popen(
    [REFINE, "serve", "--port", str(PORT)], env=env, stdout=logf, stderr=logf
)
print(f"variant={VARIANT} profile={PROFILE} pid={proc.pid}", flush=True)

BASE = f"http://127.0.0.1:{PORT}"


def health_ok():
    try:
        with urllib.request.urlopen(BASE + "/global/health", timeout=3) as r:
            return r.status == 200
    except Exception:
        return False


for _ in range(150):
    if health_ok():
        break
    time.sleep(0.2)
else:
    print("BOOT FAILED:\n" + open(log_path).read()[-2000:])
    proc.kill()
    sys.exit(2)
print("healthy", flush=True)


def count_reloads():
    try:
        return open(log_path).read().count("hot-reload ok")
    except Exception:
        return 0


def force_reload(i):
    """Swap fixture content (real bytes alternate) → watch hash-differs →
    full reload (the production trigger shape)."""
    open(fixture, "wb").write(cat_b if i % 2 == 0 else cat_a)
    for _ in range(60):  # ≤30s: poll2s + reload up to ~4.5s
        if count_reloads() >= i + 1:
            return True
        time.sleep(0.5)
    return False


time.sleep(3)  # settle post-boot
steps = []
for i in range(RELOADS):
    before = rss_mb(proc.pid)
    ok = force_reload(i)
    time.sleep(2)
    after = rss_mb(proc.pid)
    delta = after - before if before >= 0 and after >= 0 else -9999
    steps.append(delta)
    print(
        f"reload {i+1}/{RELOADS}: {'ok' if ok else 'TIMEOUT'} "
        f"rss {before} → {after} MB (Δ{delta:+d})",
        flush=True,
    )

final = rss_mb(proc.pid)
print("STEPS:", steps, flush=True)
print(f"final rss={final}MB", flush=True)

# ---- stop → bytehound dump ----
proc.send_signal(signal.SIGTERM)
try:
    proc.wait(timeout=25)
except subprocess.TimeoutExpired:
    proc.kill()
time.sleep(2)
logf.close()
dats = glob.glob(os.path.join(work, "memory-profiling_*.dat"))
reload_lines = [
    l for l in open(log_path) if "hot-reload ok" in l or "rss" in l and "→" in l
]
print("A2 reload lines:", flush=True)
for l in reload_lines:
    print("  " + l.strip(), flush=True)
print("dats:", dats, flush=True)
sys.exit(0 if dats or not PROFILE else 3)
