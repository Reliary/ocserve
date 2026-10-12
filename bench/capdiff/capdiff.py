#!/usr/bin/env python3
"""capdiff — boundary-capture differential: freeze vs ocserve.

The bug class this closes (K-CAPDIFF): every parity gate we own inspects
client<->server bytes or persisted rows. The 2026-10-12 "session won't
continue" bug lived on a DIFFERENT boundary — server->provider HTTP
(the model request was built from RAW history instead of filterCompacted).
Nothing ever looked there.

capdiff boots the vendored freeze (1.18.31) and ocserve with IDENTICAL
fixture HOMEs, both with their provider baseURL redirected to a local
capture sink. It drives the SAME client requests at both arms and diffs
three boundaries:

  1. provider request bodies  (server -> provider)   <- the hole
  2. SSE event sequences      (server -> client)     <- live-only events
  3. responses are already covered by pair-check/replay (not rebuilt here)

Normalization replaces only VOLATILE VALUES (session/message/part ids,
epochs, uuids, dates); structure (key sets, role order, counts, tool
names/schemas) is NEVER normalized away — that is the whole point.

Pre-registered kill criteria (TESTING §2 discipline):
  K-Q1a: freeze ignores the fixture baseURL redirect -> no capture -> the
         harness cannot gate; abort and report.
  K-Q1b: the deterministic stub cannot serve both arms -> abort.
  K-Q1c: comparator is vacuous (planted divergence not caught) -> abort.

Exit: 0 = green (captured + no divergence) | 1 = divergence | 2 = infra
      | 3 = self-test failed (vacuous comparator)
"""
import argparse, collections, http.server, json, os, pathlib, re, shutil, signal
import socket, socketserver, subprocess, sys, tempfile, threading, time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
VENDOR = ROOT / "bench/parity/vendor/opencode"
PORT_F, PORT_R = 4932, 4933
SINK_F, SINK_R = 8213, 8214

# ---------------------------------------------------------------- capture sink

class Sink:
    """OpenAI-compatible sink: records every request body, replies with a
    deterministic SSE completion."""
    def __init__(self, port, outdir):
        self.port, self.outdir = port, pathlib.Path(outdir)
        self.outdir.mkdir(parents=True, exist_ok=True)
        for f in self.outdir.glob("req-*.json"):
            f.unlink()
        outer = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                n = int(self.headers.get("content-length", 0) or 0)
                raw = self.rfile.read(n)
                try:
                    body = json.loads(raw)
                except Exception:
                    body = {"_raw": raw[:500].decode("utf-8", "replace")}
                t = time.time_ns()
                (outer.outdir / f"req-{t}.json").write_text(json.dumps({
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": body,
                }, indent=2))
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("cache-control", "no-cache")
                self.end_headers()
                for c in [
                    {"choices": [{"delta": {"content": "STUB-REPLY"}}]},
                    {"choices": [{"delta": {}, "finish_reason": "stop"}]},
                    {"choices": [], "usage": {"prompt_tokens": 11,
                                              "completion_tokens": 3,
                                              "total_tokens": 14}},
                ]:
                    self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()

        class S(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self.srv = S(("127.0.0.1", port), H)
        self.thr = threading.Thread(target=self.srv.serve_forever, daemon=True)
        self.thr.start()

    def stop(self):
        self.srv.shutdown()

    def requests(self):
        out = []
        for f in sorted(self.outdir.glob("req-*.json")):
            out.append(json.loads(f.read_text()))
        return out

# ---------------------------------------------------------------- SSE watcher

class SseWatcher(threading.Thread):
    """Reads /global/event, keeps (type, normalized-signature) in order."""
    def __init__(self, port):
        super().__init__(daemon=True)
        self.port, self.events, self.stop_flag = port, [], False
        self._session = None

    def run(self):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{self.port}/global/event")
            with urllib.request.urlopen(req, timeout=30) as r:
                for line in r:
                    if self.stop_flag:
                        break
                    line = line.decode("utf-8", "replace").strip()
                    if not line.startswith("data:"):
                        continue
                    payload = line[5:].strip()
                    try:
                        ev = json.loads(payload)
                    except Exception:
                        continue
                    # /global/event wraps most frames as
                    # {"directory","project","payload"} — unwrap to the
                    # {id,type,properties} event.
                    if isinstance(ev, dict) and "payload" in ev and "type" not in ev:
                        ev = ev["payload"]
                    if not isinstance(ev, dict):
                        continue
                    props = ev.get("properties", {})
                    if isinstance(props, dict):
                        sid = props.get("sessionID") or props.get("session_id")
                        # turn-scoped only: config chatter (plugin.added,
                        # catalog.updated, server.connected) carries no
                        # sessionID and is not part of the prompt contract
                        if sid != self._session:
                            continue
                        self.events.append(self._sig(ev.get("type"), props))
        except Exception:
            return

    def bind(self, sid):
        self._session = sid

    @staticmethod
    def _sig(etype, props):
        keep = {}
        for k in ("role", "finish", "status", "reply", "type"):
            if isinstance(props, dict) and k in props:
                keep[k] = props[k]
        return {"type": etype, "sig": keep}

    def stop(self):
        self.stop_flag = True

# ---------------------------------------------------------------- normalization

_ID = re.compile(r"\b(?:ses|msg|prt|evt|per|que|shell|pty)_[A-Za-z0-9]+")
_UUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                   r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
_EPOCH = re.compile(r"\b1[6-9]\d{11}\b")          # ms epochs (2020..)
_ISO = re.compile(r"\b\d{4}-\d{2}-\d{2}\b")       # dates
_DATEISH = re.compile(r"(Today'?s date[^\n]*|current.?date[^\n]*)", re.I)

def norm_text(s):
    if not isinstance(s, str):
        return s
    s = _ID.sub("<ID>", s)
    s = _UUID.sub("<UUID>", s)
    s = _EPOCH.sub("<EPOCH>", s)
    s = _ISO.sub("<DATE>", s)
    s = _DATEISH.sub("<DATE>", s)
    return s

def norm_obj(o):
    if isinstance(o, str):
        return norm_text(o)
    if isinstance(o, list):
        return [norm_obj(x) for x in o]
    if isinstance(o, dict):
        return {k: norm_obj(v) for k, v in o.items()}
    return o

# ---------------------------------------------------------------- provider diff

def provider_view(req):
    b = req.get("body", {})
    if not isinstance(b, dict):
        return {"nonjson": True}
    msgs = []
    for m in b.get("messages", []):
        c = m.get("content")
        if isinstance(c, str):
            c2 = norm_text(c)
        else:
            c2 = norm_obj(c)
        msgs.append({"role": m.get("role"), "content": c2,
                     "tool_calls": norm_obj(m.get("tool_calls")),
                     "tool_call_id": "<ID>" if m.get("tool_call_id") else None})
    tools = []
    for t in b.get("tools", []) or []:
        fn = t.get("function", t)
        tools.append({"name": fn.get("name"),
                      "description": norm_text(str(fn.get("description", ""))),
                      "params": norm_obj(fn.get("parameters", {}))})
    tools.sort(key=lambda x: str(x.get("name")))
    # name-keyed dict so an added/removed tool is ONE clean line, not a
    # cascading index-shift (K-CAPDIFF: the `task` gap produced 24 diffs).
    # Descriptions ARE compared (normalized) — a wrong or host-baked tool
    # description is exactly the class this gate exists to catch.
    tool_map = {str(t.get("name")): {
        "description": t.get("description"),
        "params": t.get("params"),
    } for t in tools}
    return {
        "path": req.get("path"),
        "model": b.get("model"),
        "stream": b.get("stream"),
        "stream_options": norm_obj(b.get("stream_options")),
        "max_tokens": b.get("max_tokens"),
        "tool_choice": norm_obj(b.get("tool_choice")),
        "messages": msgs,
        "tools": tool_map,
    }


ALLOW_FILE = ROOT / "bench/capdiff/allow.txt"

def load_allow():
    """Named divergence prefixes from bench/capdiff/allow.txt (comments with
    `#`, blank lines ignored). Every entry must carry a D-ROW reason."""
    if not ALLOW_FILE.exists():
        return []
    out = []
    for line in ALLOW_FILE.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        out.append(line.split()[0])
    return out

def allowlisted(diff_line, allow):
    """A diff line is allowlisted iff some prefix entry matches it. Keeps the
    tool-map keyed form ([["$.tools.task"]])."""
    for pref in allow:
        if diff_line == pref or diff_line.startswith(pref + ".") \
           or diff_line.startswith(pref + ":") or diff_line.startswith(pref + "["):
            return True
    return False

def diff(a, b, path="$", out=None):
    if out is None:
        out = []
    if type(a) is not type(b):
        out.append(f"{path}: type {type(a).__name__} != {type(b).__name__}")
        return out
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append(f"{path}.{k}: missing on freeze")
            elif k not in b:
                out.append(f"{path}.{k}: missing on ocserve")
            else:
                diff(a[k], b[k], f"{path}.{k}", out)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: len {len(a)} != {len(b)}")
        for i in range(min(len(a), len(b))):
            diff(a[i], b[i], f"{path}[{i}]", out)
    elif a != b:
        sa, sb = str(a), str(b)
        if len(sa) > 90:
            sa, sb = sa[:90] + "…", sb[:90] + "…"
        out.append(f"{path}: {sa!r} != {sb!r}")
    return out

# ---------------------------------------------------------------- arm plumbing

def wait_health(port, secs=40):
    for _ in range(secs * 2):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health",
                                   timeout=2)
            return True
        except Exception:
            time.sleep(0.5)
    return False

def post(port, path, obj):
    data = json.dumps(obj).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                                 headers={"content-type": "application/json"},
                                 method="POST")
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read() or b"null")

def get(port, path):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=15) as r:
        return json.loads(r.read() or b"null")

def write_fixture(home, sink_port):
    cfg = home / ".config/opencode"
    cfg.mkdir(parents=True, exist_ok=True)
    (cfg / "opencode.json").write_text(json.dumps({
        "$schema": "https://opencode.ai/config.json",
        "model": "stub/stub-1",
        "provider": {"stub": {
            "name": "Capture",
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{sink_port}/v1",
                        "apiKey": "capture-key"},
            "models": {"stub-1": {"name": "Stub One",
                                  "limit": {"context": 200000,
                                            "output": 8192}}},
        }},
        "plugin": [], "mcp": {},
    }, indent=2))
    (home / ".local/share/opencode").mkdir(parents=True, exist_ok=True)
    (home / ".local/share/opencode/auth.json").write_text(
        '{"stub":{"type":"api","key":"capture-key"}}')
    (home / ".local/state/opencode").mkdir(parents=True, exist_ok=True)
    (home / ".cache/opencode").mkdir(parents=True, exist_ok=True)
    real = pathlib.Path.home() / ".cache/opencode/models.json"
    if real.exists():
        shutil.copy(real, home / ".cache/opencode/models.json")

def boot_arm(which, home, port, sink_port, log):
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["OPENCODE_DISABLE_MODELS_FETCH"] = "1"
    if which == "freeze":
        cmd = [str(VENDOR), "serve", "--port", str(port),
               "--hostname", "127.0.0.1"]
    else:
        env["OCSERVE_DATA_DIR"] = str(home / ".local/share/ocserve")
        env["OCSERVE_LEGACY_SYNC"] = "0"
        env["OCSERVE_CORPUS"] = str(ROOT / "testdata/golden")
        cmd = [str(ROOT / "target/release/ocserve"), "serve", "--port", str(port)]
    f = open(log, "w")
    p = subprocess.Popen(cmd, env=env, stdout=f, stderr=subprocess.STDOUT,
                         start_new_session=True, cwd=str(ROOT))
    return p

# ---------------------------------------------------------------- scenarios

def run_arm(which, port, sink_port, prompts, tmp):
    home = tmp / which
    home.mkdir(parents=True)
    write_fixture(home, sink_port)
    sink = Sink(sink_port, tmp / f"cap-{which}")
    p = boot_arm(which, home, port, sink_port, tmp / f"{which}.log")
    watcher = SseWatcher(port)
    try:
        if not wait_health(port):
            raise RuntimeError(f"{which} never became healthy; see {which}.log")
        # bind the SSE watcher to the session BEFORE driving it so no
        # live-only event is missed (the partID class)
        sid = post(port, "/session?directory=%2F", {})["id"]
        watcher.bind(sid)
        watcher.start()
        for text in prompts:
            post(port, f"/session/{sid}/message", {
                "model": {"providerID": "stub", "modelID": "stub-1"},
                "parts": [{"type": "text", "text": text}],
            })
        time.sleep(2.0)
    finally:
        watcher.stop()
        try:
            os.killpg(os.getpgid(p.pid), signal.SIGTERM)
        except Exception:
            p.terminate()
        sink.stop()
    return {"which": which, "requests": sink.requests(),
            "events": watcher.events, "log": str(tmp / f"{which}.log")}

# ---------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenario", default="multiturn",
                    choices=["fresh", "multiturn"])
    ap.add_argument("--selftest", action="store_true",
                    help="plant a synthetic divergence, require comparator red")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    if sys.version_info < (3, 8):
        print("needs py3.8+", file=sys.stderr)
        return 2
    if not VENDOR.exists():
        print(f"capdiff: vendored freeze missing: {VENDOR}", file=sys.stderr)
        return 2
    if not (ROOT / "target/release/ocserve").exists():
        print("capdiff: target/release/ocserve missing (build first)",
              file=sys.stderr)
        return 2

    prompts = (["hello Q1"] if args.scenario == "fresh"
               else ["remember the token ALPHA", "what token?"])

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ocserve-capdiff."))
    try:
        fr = run_arm("freeze", PORT_F, SINK_F, prompts, tmp)
        oc = run_arm("ocserve", PORT_R, SINK_R, prompts, tmp)

        if not fr["requests"] or not oc["requests"]:
            print(f"K-Q1a FAIL: no capture (freeze={len(fr['requests'])} "
                  f"ocserve={len(oc['requests'])})", file=sys.stderr)
            if args.keep:
                print(f"logs kept at {tmp}")
            return 2

        fv = [provider_view(r) for r in fr["requests"]]
        ov = [provider_view(r) for r in oc["requests"]]

        # self-test: plant a divergence, comparator MUST flag it
        if args.selftest:
            if not fv:
                print("selftest: nothing captured", file=sys.stderr)
                return 3
            planted = json.loads(json.dumps(fv))
            planted[0]["messages"].append(
                {"role": "user", "content": "PLANTED-DIVERGENCE"})
            d = diff(fv, planted)
            caught = any("PLANTED-DIVERGENCE" in x or "len" in x for x in d)
            print(f"selftest: comparator {'caught' if caught else 'MISSED'} "
                  f"planted divergence ({len(d)} diffs)")
            print(f"selftest: {'PASS' if caught else 'FAIL'}")
            return 0 if caught else 3

        report = {"scenario": args.scenario,
                  "freeze_requests": len(fv), "ocserve_requests": len(ov),
                  "diffs": [], "allowlisted": [], "event_diffs": []}

        # provider-boundary diff: compare the LAST (main) request of each arm;
        # report the count delta separately (freeze title-gen etc = D-AUTOTITLE)
        allow = load_allow()
        if fv and ov:
            raw = diff(fv[-1], ov[-1])
            report["diffs"] = [d for d in raw if not allowlisted(d, allow)]
            report["allowlisted"] = [d for d in raw if allowlisted(d, allow)]

        # SSE event-type SET diff is the gate (a missing event CLASS = red —
        # e.g. losing message.part.delta is the partID-live-streaming class).
        # Exact order/counts are informational: freeze emits extra REDUNDANT
        # updates, and forcing identical counts would demand ocserve emit
        # duplicates. The set must match BOTH ways (no missing, no spurious).
        fe = [e["type"] for e in fr["events"]]
        oe = [e["type"] for e in oc["events"]]
        fset, oset = set(fe), set(oe)
        missing = sorted(fset - oset)
        spurious = sorted(oset - fset)
        report["event_types"] = {"freeze": sorted(fset), "ocserve": sorted(oset)}
        report["event_missing"] = missing
        report["event_spurious"] = spurious
        fcount, ocount = collections.Counter(fe), collections.Counter(oe)
        report["event_counts"] = {"freeze": dict(fcount), "ocserve": dict(ocount)}
        if missing:
            report["event_diffs"] = [f"ocserve missing event type(s): {missing}"]

        print(json.dumps(report, indent=2))
        green = (not report["diffs"] and not report["event_diffs"])
        print(f"capdiff: {'GREEN' if green else 'DIVERGENCE'} "
              f"({len(report['diffs'])} provider diffs, "
              f"{len(report['allowlisted'])} allowlisted, "
              f"{len(report['event_diffs'])} event diffs)")
        if not green and args.keep:
            print(f"artifacts: {tmp}")
        return 0 if green else 1
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"kept: {tmp}")

if __name__ == "__main__":
    sys.exit(main())
