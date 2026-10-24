#!/usr/bin/env python3
"""Tool-level permission differential: record the ask sequence each arm emits
for the same tool call, then compare.

`permission-check.sh` covers `evaluate()` through the v2 permission oracle. This
covers the other half of the contract: which asks a tool emits, in what order,
with which `patterns` and `always` grants.

Observed over the PUBLIC WIRE, not through a plugin: a client sees each ask as a
`permission.asked` event on `/global/event` and answers with
`POST /permission/{id}/reply`. That is exactly the contract under test, and it
avoids depending on plugin-loading behaviour in either arm.

Each case runs as a fresh session; cases are strictly sequential, so the
recorded asks map to the case that was running.

Usage:
  tool-oracle.py             compare ocserve against the committed recording
  tool-oracle.py --refresh   re-record the reference (needs the vendored binary)
  tool-oracle.py --self-test prove the comparator is non-vacuous
Exit 0 = parity, 1 = divergence, 2 = infrastructure failure.
"""
import json
import os
import pathlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
BASEDIR = ROOT / "bench/permission"
VENDOR = ROOT / "bench/parity/vendor/opencode"
OCSERVE_BIN = pathlib.Path(os.environ.get("OCSERVE_BIN", ROOT / "target/release/ocserve"))
CASES = BASEDIR / "tool-cases.json"
STUB = BASEDIR / "tool-stub.py"
RECORDING = BASEDIR / "1.18.31.tool-asks.json"

PORT_F = int(os.environ.get("PERM_TOOL_PORT_F", "4936"))
PORT_R = int(os.environ.get("PERM_TOOL_PORT_R", "4937"))
STUB_F = int(os.environ.get("PERM_TOOL_STUB_F", "4938"))
STUB_R = int(os.environ.get("PERM_TOOL_STUB_R", "4939"))

ARM_TIMEOUT = int(os.environ.get("PERM_TOOL_ARM_TIMEOUT", "600"))


def die(msg):
    print(f"tool-oracle: {msg}", file=sys.stderr)
    sys.exit(2)


def port_free(p):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", p))
        return True
    except OSError:
        return False
    finally:
        s.close()


def wait_health(port, secs=90):
    for _ in range(secs * 2):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health", timeout=2)
            return True
        except Exception:
            time.sleep(0.5)
    return False


def request(port, path, obj, timeout=120, method="POST"):
    data = json.dumps(obj).encode() if obj is not None else None
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data=data,
        headers={"content-type": "application/json"}, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        body = r.read()
        return json.loads(body) if body else None


class AskWatcher(threading.Thread):
    """Reads /global/event, appends every permission.asked for the sessions we
    created. Replies are made by the case loop, not here, so the recorded order
    is the server's order."""

    def __init__(self, port):
        super().__init__(daemon=True)
        self.port = port
        self.events = []
        self.lock = threading.Lock()
        self.stop_flag = False

    def run(self):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{self.port}/global/event")
            with urllib.request.urlopen(req, timeout=ARM_TIMEOUT) as r:
                for raw in r:
                    if self.stop_flag:
                        return
                    line = raw.decode("utf-8", "replace").strip()
                    if not line.startswith("data:"):
                        continue
                    try:
                        ev = json.loads(line[5:].strip())
                    except Exception:
                        continue
                    if isinstance(ev, dict) and "payload" in ev and "type" not in ev:
                        ev = ev["payload"]
                    if not isinstance(ev, dict):
                        continue
                    if ev.get("type") != "permission.asked":
                        continue
                    props = ev.get("properties") or {}
                    with self.lock:
                        self.events.append(props)
        except Exception:
            return

    def drain(self):
        with self.lock:
            out, self.events = self.events, []
        return out

    def stop(self):
        self.stop_flag = True


def write_fixture(home, stub_port):
    cfg = home / ".config/opencode"
    cfg.mkdir(parents=True, exist_ok=True)
    (cfg / "opencode.json").write_text(json.dumps({
        "$schema": "https://opencode.ai/config.json",
        "model": "stub/stub-1",
        "provider": {"stub": {
            "name": "Perm Stub",
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{stub_port}/v1",
                        "apiKey": "perm-oracle-key"},
            "models": {"stub-1": {"name": "Stub One",
                                  "limit": {"context": 200000, "output": 8192}}},
        }},
        # Every family asked explicitly: the default config allows reads, so a
        # case would otherwise record nothing and look like parity.
        "permission": {"bash": "ask", "edit": "ask", "read": "ask",
                       "glob": "ask", "grep": "ask", "webfetch": "ask",
                       "todowrite": "ask", "skill": "ask", "task": "ask",
                       "external_directory": "ask"},
        "plugin": [],
        "mcp": {},
    }, indent=2))
    (home / ".local/share/opencode").mkdir(parents=True, exist_ok=True)
    (home / ".local/share/opencode/auth.json").write_text(
        '{"stub":{"type":"api","key":"perm-oracle-key"}}')
    (home / ".local/state/opencode").mkdir(parents=True, exist_ok=True)
    (home / ".local/state/opencode/model.json").write_text(
        '{"recent":[{"providerID":"stub","modelID":"stub-1"}],"favorite":[],"variant":{}}')
    (home / ".cache/opencode").mkdir(parents=True, exist_ok=True)
    real = pathlib.Path.home() / ".cache/opencode/models.json"
    if real.exists():
        shutil.copy(real, home / ".cache/opencode/models.json")
    work = home / "work"
    work.mkdir(exist_ok=True)
    (work / "Cargo.toml").write_text("[workspace]\nmembers = []\n")
    (work / "src").mkdir(exist_ok=True)
    (work / "src" / "main.rs").write_text("fn main() {}\n")


def boot(which, home, port, log):
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["OPENCODE_DISABLE_MODELS_FETCH"] = "1"
    if which == "freeze":
        cmd = [str(VENDOR), "serve", "--port", str(port), "--hostname", "127.0.0.1"]
    else:
        env["OCSERVE_DATA_DIR"] = str(home / ".local/share/ocserve")
        env["OCSERVE_LEGACY_SYNC"] = "0"
        env["OCSERVE_CONFIG_WATCH"] = "0"
        cmd = [str(OCSERVE_BIN), "serve", "--port", str(port)]
    f = open(log, "w")
    return subprocess.Popen(cmd, env=env, stdout=f, stderr=subprocess.STDOUT,
                            start_new_session=True, cwd=str(home / "work"))


def run_turn(port, sid, text, watcher, timeout):
    """POST a prompt while answering every ask it raises. Returns the asks in
    server order. The prompt call blocks until the turn finishes, so it runs on
    its own thread."""
    result = {}

    def go():
        try:
            request(port, f"/session/{sid}/message", {
                "model": {"providerID": "stub", "modelID": "stub-1"},
                "parts": [{"type": "text", "text": f"TOOLCASE:{text}"}],
            }, timeout=timeout)
            result["ok"] = True
        except Exception as e:  # a failed turn is itself a finding
            result["ok"] = False
            result["error"] = str(e)[:300]

    th = threading.Thread(target=go, daemon=True)
    th.start()
    asks = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        for req in watcher.drain():
            if req.get("sessionID") != sid:
                continue
            asks.append({
                "permission": req.get("permission"),
                "patterns": req.get("patterns") or [],
                "always": req.get("always") or [],
                "metadata_keys": sorted((req.get("metadata") or {}).keys()),
                "diff": (req.get("metadata") or {}).get("diff"),
                "has_tool": req.get("tool") is not None,
            })
            rid = req.get("id")
            try:
                # directory-scoped: the reference resolves the instance from
                # `?directory=`, and a session created at `?directory=%2F`
                # lives in that instance. Replying without it 404s on the
                # reference (probed), so the oracle must send it to be a fair
                # comparison rather than testing our own leniency.
                request(port, f"/permission/{rid}/reply?directory=%2F",
                        {"reply": "once"}, timeout=30)
            except Exception:
                pass
        if not th.is_alive():
            # one last drain: the ask event can land just before the turn ends
            time.sleep(0.2)
            for req in watcher.drain():
                if req.get("sessionID") == sid:
                    asks.append({
                        "permission": req.get("permission"),
                        "patterns": req.get("patterns") or [],
                        "always": req.get("always") or [],
                        "metadata_keys": sorted((req.get("metadata") or {}).keys()),
                        "diff": (req.get("metadata") or {}).get("diff"),
                        "has_tool": req.get("tool") is not None,
                    })
            break
        time.sleep(0.1)
    th.join(timeout=5)
    return asks, result


def record_arm(which, port, stub_port, tmp):
    home = tmp / which
    home.mkdir(parents=True)
    write_fixture(home, stub_port)
    stub = subprocess.Popen([sys.executable, str(STUB), str(stub_port), str(CASES),
                             str(home / "work")],
                            stdout=open(tmp / f"{which}.stub.log", "w"),
                            stderr=subprocess.STDOUT, start_new_session=True)
    srv = boot(which, home, port, tmp / f"{which}.log")
    out = {}
    try:
        if not wait_health(port):
            raise RuntimeError(f"{which} never became healthy; see {tmp}/{which}.log")
        watcher = AskWatcher(port)
        watcher.start()
        time.sleep(0.5)
        for case in json.load(open(CASES))["cases"]:
            try:
                sid = request(port, "/session?directory=%2F", {})["id"]
            except Exception as e:
                out[case["name"]] = {"asks": [], "error": f"create: {e}"[:200]}
                continue
            asks, res = run_turn(port, sid, case["name"], watcher, timeout=120)
            out[case["name"]] = {"asks": asks, **({"error": res["error"]} if not res.get("ok") else {})}
        watcher.stop()
        return out
    finally:
        for p in (srv, stub):
            try:
                os.killpg(os.getpgid(p.pid), signal.SIGTERM)
            except Exception:
                p.terminate()


def compare(ref, got):
    failures, notes = [], []
    for case in sorted(set(ref) | set(got)):
        r = ref.get(case, {}).get("asks", [])
        g = got.get(case, {}).get("asks", [])

        def key(a):
            return (a["permission"], tuple(a["patterns"]), tuple(a["always"]),
                    tuple(a["metadata_keys"]), a["has_tool"], a.get("diff"))

        if [key(a) for a in r] == [key(a) for a in g]:
            notes.append(f"ok   {case}: {len(r)} ask(s)")
            continue
        if sorted(map(key, r)) == sorted(map(key, g)):
            failures.append(f"ORDER {case}: same asks, different order\n"
                            f"        reference: {[a['permission'] for a in r]}\n"
                            f"        ocserve:   {[a['permission'] for a in g]}")
            continue
        for i in range(max(len(r), len(g))):
            if i >= len(r):
                failures.append(f"EXTRA {case}[{i}]: ocserve asked {g[i]}")
            elif i >= len(g):
                failures.append(f"MISSING {case}[{i}]: reference asked {r[i]}")
            elif key(r[i]) != key(g[i]):
                diffs = [f"{f}: reference={r[i][f]!r} ocserve={g[i][f]!r}"
                         for f in ("permission", "patterns", "always",
                                   "metadata_keys", "has_tool", "diff")
                         if r[i][f] != g[i][f]]
                failures.append(f"SHAPE {case}[{i}]: " + "; ".join(diffs))
        if ref.get(case, {}).get("error") or got.get(case, {}).get("error"):
            failures.append(f"TURN {case}: reference_error={ref.get(case, {}).get('error')!r} "
                            f"ocserve_error={got.get(case, {}).get('error')!r}")
    return failures, notes


def main():
    self_test = "--self-test" in sys.argv
    refresh = "--refresh" in sys.argv
    if not OCSERVE_BIN.exists():
        die(f"ocserve binary missing: {OCSERVE_BIN} (cargo build --release)")
    if refresh and not VENDOR.exists():
        die(f"vendored reference missing: {VENDOR} (run scripts/acquire-freeze.sh)")
    for p in (PORT_F, PORT_R, STUB_F, STUB_R):
        if not port_free(p):
            die(f"port {p} busy")

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="perm-oracle-", dir="/var/tmp"))
    ok = False
    try:
        if refresh:
            print(f"tool-oracle: recording reference on :{PORT_F}")
            ref = record_arm("freeze", PORT_F, STUB_F, tmp)
            RECORDING.write_text(json.dumps({
                "note": "recorded from the reference binary; never hand-edit",
                "source": f"1.18.31 vendor, {len(ref)} cases",
                "cases": ref}, indent=2) + "\n")
            print(f"tool-oracle: recorded {RECORDING}")
        else:
            ref = json.load(open(RECORDING))["cases"]

        print(f"tool-oracle: recording ocserve on :{PORT_R}")
        got = record_arm("ocserve", PORT_R, STUB_R, tmp)

        if self_test:
            planted = json.loads(json.dumps(got))
            for name, entry in planted.items():
                if entry.get("asks"):
                    entry["asks"][0]["patterns"] = ["<planted-divergence>"]
                    break
            f, _ = compare(ref, planted)
            if not f:
                die("self-test: comparator passed a planted divergence (vacuous)")
            f2, _ = compare(ref, got)
            if f2:
                print("tool-oracle: self-test: real diff present, cannot isolate")
            print(f"tool-oracle: self-test OK ({len(f)} planted divergence(s) caught)")
            ok = True
            return 0

        failures, notes = compare(ref, got)
        for n in notes:
            print(n)
        if failures:
            print(f"tool-oracle: {len(failures)} divergence(s)")
            for f in failures:
                print("  " + f)
            print(f"tool-oracle: logs kept at {tmp}")
            return 1
        total = sum(len(v["asks"]) for v in ref.values())
        print(f"tool-oracle: {len(ref)} case(s), {total} ask(s), 0 divergences")
        ok = True
        return 0
    finally:
        if ok:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
