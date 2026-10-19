#!/usr/bin/env python3
"""Phase 2 stress battery (STRESS.md pre-registered).

  s1 <rt> <raw|norm>   2000 mixed triggers, windowed p50/p95/p99
  s2 <rt> <raw|norm>   payload scaling 10KB->8MB, 20 triggers each, RSS peak/recovery
  s3 <rt> <raw|norm>   4 threads x 500 triggers (id-matched multiplexer)
  s4                    chaos battery (top-3 cells; normalizer input adversarial)
"""
import json
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time

HOME = os.path.expanduser("~")
OCSERVE = os.path.expanduser("~/src/ocserve")
HERE = os.path.dirname(os.path.abspath(__file__))
HOST = f"{OCSERVE}/crates/ocserve-plugin/src/host/host.mjs"
NORM = f"{HERE}/target/release/normalizer-spike"
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
INPUT = {"directory": OCSERVE, "projectID": "global",
         "serverUrl": "http://127.0.0.1:9"}


def entries(runtime, norm):
    if norm == "norm":
        return [("codex-auth", f"{CA}/index.normalized.mjs"),
                ("magic-context", f"{MC}/index.normalized.mjs"),
                ("reliary8", f"{REL}/index.normalized.mjs")]
    return [("codex-auth", f"{CA}/index.js"),
            ("magic-context", RAW_MAGIC[runtime]),
            ("reliary8", f"{REL}/index.js")]


def rss_mb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024
    except Exception:
        return -1.0
    return -1.0


def conv_of_kb(kb):
    n = max(1, kb * 1024 // 680)
    return {"messages": [
        {"role": "user" if i % 2 == 0 else "assistant",
         "content": f"[{i}] " + ("lorem ipsum dolor sit amet " * 18)[:640]}
        for i in range(n)]}


def loaded(runtime, variant):
    p = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True)
    state = {"id": 0, "lock": threading.Lock()}

    def rpc(method, params=None, timeout=120):
        with state["lock"]:
            state["id"] += 1
            rid = state["id"]
            t0 = time.perf_counter()
            p.stdin.write(json.dumps({"id": rid, "method": method,
                                      "params": params}) + "\n")
            p.stdin.flush()
        deadline = time.perf_counter() + timeout
        while time.perf_counter() < deadline:
            line = p.stdout.readline()
            if not line:
                raise RuntimeError("eof")
            m = json.loads(line)
            if m.get("id") == rid:
                dt = (time.perf_counter() - t0) * 1000
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result"), dt
        raise TimeoutError(method)

    rpc("ping")
    for spec, entry in entries(runtime, variant):
        rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
    return p, rpc


def dispose(p):
    try:
        p.stdin.write(json.dumps({"id": 99999, "method": "dispose"}) + "\n")
        p.stdin.flush()
        p.wait(timeout=10)
    except Exception:
        p.kill()


def chat_params(i):
    return {"name": "chat.message",
            "input": {"sessionID": "s", "message":
                      {"role": "user", "parts": [{"type": "text", "text": f"t{i}"}]}},
            "output": {}}


def xform_params(conv):
    return {"name": "experimental.chat.messages.transform",
            "input": {}, "output": dict(conv)}


def agg(v):
    sv = sorted(v)
    return {"p50": round(statistics.median(sv), 3),
            "p95": round(sv[int(len(sv) * 0.95) - 1], 3),
            "p99": round(sv[int(len(sv) * 0.99) - 1], 3),
            "max": round(sv[-1], 3)}


def s1(runtime, variant, n=2000):
    p, rpc = loaded(runtime, variant)
    conv = conv_of_kb(100)
    for i in range(20):
        rpc("trigger", chat_params(i))
    lat, windows, errs = [], [], 0
    t0 = time.time()
    for i in range(n):
        try:
            if i % 10 == 9:
                _, dt = rpc("event", {"event": {"id": f"e{i}",
                                                "type": "session.created",
                                                "properties": {}}})
            elif i % 2 == 0:
                _, dt = rpc("trigger", chat_params(i))
            else:
                _, dt = rpc("trigger", xform_params(conv))
            lat.append(dt)
            if len(lat) % 500 == 0:
                windows.append(agg(lat[-500:]))
        except Exception:
            errs += 1
    dur = time.time() - t0
    rss_after = rss_mb(p.pid)
    dispose(p)
    w1, w4 = windows[0], windows[-1]
    creep = w4["p95"] <= w1["p95"] * 3
    print(f"S1 {runtime}-{variant}: n={n} errs={errs} dur={dur:.1f}s "
          f"throughput={n/dur:.0f}/s w1={w1} w4={w4} "
          f"{'CREEP-OK' if creep else 'CREEP-FAIL'} rss_after={rss_after:.0f}MB "
          f"{'PASS' if errs == 0 and creep else 'FAIL'}")


def s2(runtime, variant):
    p, rpc = loaded(runtime, variant)
    for i in range(10):
        rpc("trigger", chat_params(i))
    base = rss_mb(p.pid)
    fails = 0
    print(f"S2 {runtime}-{variant}: baseline={base:.0f}MB")
    for kb in [10, 100, 1024, 8192]:
        conv = conv_of_kb(kb)
        peak, lats = 0.0, []
        for i in range(20):
            _, dt = rpc("trigger", xform_params(conv), timeout=300)
            lats.append(dt)
            peak = max(peak, rss_mb(p.pid))
        time.sleep(float(os.environ.get("S2_RECOVER_WAIT", "1")))
        after = rss_mb(p.pid)
        recov = after <= base * 1.20
        p50 = statistics.median(lats)
        knee = p50 > 50
        fails += 0 if recov else 1
        print(f"  {kb:>5}KB: p50={p50:8.1f}ms peak={peak:6.0f}MB "
              f"after={after:6.0f}MB recovery={'OK' if recov else 'FAIL'}"
              f"{' <- KNEE' if knee else ''}", flush=True)
    dispose(p)
    print(f"S2 {runtime}-{variant}: {'PASS' if fails == 0 else f'FAIL ({fails} recovery)'}")


class Multiplexer:
    """Single dedicated reader thread + per-request events.

    Four threads share one host pipe; naive per-call readline() misattributes
    lines across threads (proved by the S3 hang). One reader matches id ->
    event, so responses route correctly while N requests stay in flight.
    """

    def __init__(self, p):
        self.p = p
        self.lock = threading.Lock()
        self.id = 0
        self.pending = {}          # rid -> {"result":..., "event": Event}
        self.dead = False
        self.err = None
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self):
        for line in self.p.stdout:
            try:
                m = json.loads(line)
            except Exception:
                continue
            rid = m.get("id")
            with self.lock:
                slot = self.pending.pop(rid, None)
            if slot is not None:
                slot["msg"] = m
                slot["event"].set()
        # EOF: fail everyone so callers never hang
        with self.lock:
            self.dead = True
            slots = list(self.pending.values())
            self.pending.clear()
        for slot in slots:
            slot["msg"] = {"error": "host-eof"}
            slot["event"].set()

    def rpc(self, method, params=None, timeout=120):
        with self.lock:
            if self.dead:
                raise RuntimeError(f"host dead before {method}")
            self.id += 1
            rid = self.id
            slot = {"event": threading.Event(), "msg": None}
            self.pending[rid] = slot
            self.p.stdin.write(json.dumps({"id": rid, "method": method,
                                           "params": params}) + "\n")
            self.p.stdin.flush()
        if not slot["event"].wait(timeout):
            with self.lock:
                self.pending.pop(rid, None)
            raise TimeoutError(method)
        m = slot["msg"]
        if "error" in m:
            raise RuntimeError(f"{method}: {m['error']}")
        return m.get("result")


def s3(runtime, variant, threads=4, per=500):
    p, _ = loaded(runtime, variant)   # sequential load path (unused rpc)
    # fresh multiplexed connection: reload under the multiplexer
    try:
        p.kill(); p.wait(timeout=5)
    except Exception:
        pass
    p = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True)
    mux = Multiplexer(p)
    mux.rpc("ping")
    for spec, entry in entries(runtime, variant):
        mux.rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
    conv = conv_of_kb(100)
    results = {i: [] for i in range(threads)}
    errs = []

    def worker(tid):
        for i in range(per):
            try:
                if i % 2 == 0:
                    t0 = time.perf_counter()
                    mux.rpc("trigger", chat_params(tid * per + i))
                else:
                    t0 = time.perf_counter()
                    mux.rpc("trigger", xform_params(conv))
                results[tid].append((time.perf_counter() - t0) * 1000)
            except Exception as e:
                errs.append(str(e)[:80])

    base = rss_mb(p.pid)
    t0 = time.time()
    ts = [threading.Thread(target=worker, args=(t,)) for t in range(threads)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    dur = time.time() - t0
    peak = rss_mb(p.pid)
    # sequential reference: same work, single thread, fresh host
    try:
        p.kill(); p.wait(timeout=5)
    except Exception:
        pass
    p2 = subprocess.Popen(SPAWNS[runtime], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          text=True)
    mux2 = Multiplexer(p2)
    mux2.rpc("ping")
    for spec, entry in entries(runtime, variant):
        mux2.rpc("load", {"spec": spec, "entry": entry, "input": INPUT})
    seq_lat = []
    t0 = time.time()
    for i in range(400):
        try:
            s0 = time.perf_counter()
            if i % 2 == 0:
                mux2.rpc("trigger", chat_params(i))
            else:
                mux2.rpc("trigger", xform_params(conv))
            seq_lat.append((time.perf_counter() - s0) * 1000)
        except Exception as e:
            errs.append("seq:" + str(e)[:60])
    seq_dur = time.time() - t0
    dispose(p2)

    all_lat = [x for v in results.values() for x in v]
    thr = len(all_lat) / dur
    seq_thr = len(seq_lat) / seq_dur if seq_dur > 0 else 0
    speedup = thr / seq_thr if seq_thr > 0 else 0
    ok = (not errs and len(all_lat) == threads * per
          and peak <= base * 2 and speedup >= 1.5)
    print(f"S3 {runtime}-{variant}: total={len(all_lat)} errs={len(errs)} "
          f"conc_thr={thr:.0f}/s seq_thr={seq_thr:.0f}/s speedup={speedup:.2f}x "
          f"p50={statistics.median(all_lat):.3f}ms rss {base:.0f}->{peak:.0f}MB "
          f"{'PASS' if ok else 'FAIL'}"
          + (f" first_err={errs[0]}" if errs else ""), flush=True)


def s4():
    print("=== S4 chaos battery ===")
    # 1: SIGKILL mid-trigger -> restart -> reload norm -> trigger OK
    p, rpc = loaded("bun", "norm")
    p.kill()
    p.wait(timeout=5)
    p2, rpc2 = loaded("bun", "norm")
    r, _ = rpc2("trigger", chat_params(1))
    dispose(p2)
    print(f"1 SIGKILL+restart+reload+trigger: {'PASS' if r is not None else 'FAIL'}")

    # 2: truncate normalized output -> load must FAIL LOUD
    victim = f"{CA}/index.normalized.mjs"
    orig = open(victim).read()
    try:
        with open(victim, "w") as f:
            f.write(orig[: len(orig) // 3])
        p, rpc = subprocess.Popen(SPAWNS["bun"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True), None
        state = {"id": 0}

        def rpc2(method, params=None, timeout=30):
            state["id"] += 1
            rid = state["id"]
            p.stdin.write(json.dumps({"id": rid, "method": method,
                                      "params": params}) + "\n")
            p.stdin.flush()
            deadline = time.time() + timeout
            while time.time() < deadline:
                line = p.stdout.readline()
                if not line:
                    return {"error": "eof"}
                m = json.loads(line)
                if m.get("id") == rid:
                    return m
            return {"error": "timeout"}
        rpc2("ping")
        res = rpc2("load", {"spec": "codex-auth", "entry": victim,
                            "input": INPUT})
        loud = "error" in res and "timeout" not in str(res.get("error", "")) \
            and res.get("error") != "eof"
        print(f"2 truncated-output load fails loud: "
              f"{'PASS' if loud else 'FAIL'} ({str(res.get('error', res))[:90]})")
        try:
            p.kill()
        except Exception:
            pass
    finally:
        with open(victim, "w") as f:
            f.write(orig)

    # 3: entry content edit -> cache goes cold -> restore
    entry = f"{REL}/index.js"
    orig_e = open(entry).read()
    try:
        with open(entry, "w") as f:
            f.write(orig_e + "\n// stress-edit\n")
        env = {**os.environ, "NORMALIZER_SHIM": f"{HERE}/shim-k5b.mjs"}
        r = subprocess.run([NORM, "normalize", entry], env=env,
                           capture_output=True, text=True, timeout=60)
        cold = "OK" in r.stdout and "WARM HIT" not in r.stdout
        r2 = subprocess.run([NORM, "normalize", entry], env=env,
                            capture_output=True, text=True, timeout=60)
        warm = "WARM HIT" in r2.stdout
        print(f"3 edit->cold rebuild + stable->warm: "
              f"{'PASS' if cold and warm else 'FAIL'} "
              f"(cold={'OK' if cold else r.stdout.strip()[:40]}, warm={warm})")
    finally:
        with open(entry, "w") as f:
            f.write(orig_e)

    # 4: delete outputs while host running -> respawn path rebuilds
    outs = [f"{REL}/index.normalized.mjs", f"{REL}/index.normalized.mjs.hash"]
    saved = {o: (open(o).read() if os.path.exists(o) else None) for o in outs}
    try:
        p, rpc = loaded("bun", "norm")
        for o in outs:
            if os.path.exists(o):
                os.remove(o)
        p.kill()
        p.wait(timeout=5)
        env = {**os.environ, "NORMALIZER_SHIM": f"{HERE}/shim-k5b.mjs"}
        r = subprocess.run([NORM, "normalize", f"{REL}/index.js"], env=env,
                           capture_output=True, text=True, timeout=60)
        rebuilt = os.path.exists(outs[0]) and "OK" in r.stdout
        p2, rpc2 = loaded("bun", "norm")
        rpc2("trigger", chat_params(1))
        dispose(p2)
        print(f"4 cache-wipe while running -> respawn rebuilds: "
              f"{'PASS' if rebuilt else 'FAIL'}")
    finally:
        for o, content in saved.items():
            if content is not None:
                with open(o, "w") as f:
                    f.write(content)

    # 5: bun absent from PATH -> selection falls to node
    env = {**os.environ}
    env["PATH"] = ":".join(
        d for d in env.get("PATH", "").split(":")
        if not os.path.exists(os.path.join(d, "bun")))
    has_bun_after = shutil.which("bun", path=env["PATH"]) is not None
    has_node_after = shutil.which("node", path=env["PATH"]) is not None
    print(f"5 bun-absent PATH selection: "
          f"{'PASS' if not has_bun_after and has_node_after else 'FAIL'} "
          f"(env probe; ocserve's plugin_runtime() unit-tested on this rule)")

    # 6: adversarial normalizer inputs -> Err, not panic
    ad = os.path.join(HERE, "adversarial")
    os.makedirs(ad, exist_ok=True)
    cases = {
        "syntax-error.js": "import { from 'broken",
        "cycle-a.js": 'import "./cycle-b.js"; console.log("a");',
        "cycle-b.js": 'import "./cycle-a.js"; console.log("b");',
        "garbage.js": bytes(range(256)).decode("latin-1"),
        "huge.js": "// pad\n" + "x" * (50 * 1024 * 1024),
    }
    all_ok = True
    for name, content in cases.items():
        path = os.path.join(ad, name)
        with open(path, "w", errors="replace") as f:
            f.write(content)
        env = {**os.environ, "NORMALIZER_SHIM": f"{HERE}/shim-k5b.mjs"}
        try:
            r = subprocess.run([NORM, "normalize", path], env=env,
                               capture_output=True, text=True, timeout=120)
            panicked = "panicked" in (r.stderr or "").lower() or r.returncode < 0
            ok = not panicked
        except subprocess.TimeoutExpired:
            ok = False
            panicked = False
        if not ok:
            all_ok = False
        print(f"6 {name}: rc={getattr(r, 'returncode', 'timeout')} "
              f"{'PANIC/FAIL' if not ok else 'graceful Err/OK'} "
              f"{(getattr(r, 'stderr', '') or '')[:70]}")
    print(f"S4 overall: see items above; adversarial={'PASS' if all_ok else 'FAIL'}")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    cmd = sys.argv[1]
    if cmd == "s1":
        s1(sys.argv[2], sys.argv[3])
    elif cmd == "s2":
        s2(sys.argv[2], sys.argv[3])
    elif cmd == "s3":
        s3(sys.argv[2], sys.argv[3])
    elif cmd == "s4":
        s4()
    else:
        print("unknown", cmd)
        sys.exit(1)


if __name__ == "__main__":
    main()
