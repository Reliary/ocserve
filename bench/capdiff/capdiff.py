#!/usr/bin/env python3
"""capdiff — boundary-capture differential: freeze vs ocserve.

The bug class this closes (K-CAPDIFF/K-CAPDIFF-EXT): every parity gate we own
inspects client<->server bytes or persisted rows. The 2026-10-12 "session
won't continue" bug lived on a DIFFERENT boundary — server->provider HTTP
(the model request was built from RAW history instead of filterCompacted).
Nothing ever looked there.

capdiff boots the vendored freeze (1.18.31) and ocserve with IDENTICAL fixture
HOMEs, both with their provider baseURL redirected to a local capture sink. It
boots EACH ARM ONCE, drives a data-driven scenario matrix as fresh sessions,
and diffs three boundaries per scenario:

  1. provider-request SEQUENCE (server -> provider)   <- the hole
  2. SSE event-type SET (server -> client)            <- live-only events
  3. cost: request bytes / message count / round-trips (ocserve>freeze = red)

Everything is data-driven and mechanical: scenarios.json (matrix),
stub_rules.json (deterministic tool_calls), the two arms' captures are the
only inputs. Extending coverage = a JSON row. See bench/capdiff/README.md.

Normalization replaces only VOLATILE VALUES (ids, epochs, uuids, dates);
structure (key sets, role order, counts, tool names/schemas) is NEVER
normalized away — that is the whole point.

SRE discipline (SRE.md): fail-fast preflight (infra exit != divergence exit);
no invented thresholds (report spreads; gate only stable axes); determinism is
a gate (flaky = defect: matrix runs forward AND reverse, captures must match);
bounded runtime (event-driven idle wait + per-scenario timeout). ocserve
exposes /metrics; freeze's /metrics is the SPA (no metrics) -> freeze prints
n/a, never 0 (D-PARITY-METRICS).

Pre-registered kill criteria (TESTING §2):
  K-Q1a: freeze ignores the fixture baseURL redirect -> no capture -> abort.
  K-Q1b: the deterministic stub cannot serve both arms -> abort.
  K-Q1c: comparator is vacuous (planted divergence not caught) -> abort.
  K-EXT-1: driven summarize yields no filtered next request on freeze.
  K-EXT-2: the stub tool loop does not terminate identically on both arms.
  K-EXT-3: tool-turn captures unstable in the fixture worktree.
  K-EXT-4: parallel arms introduce flake -> fall back to sequential.

Exit: 0 = green | 1 = divergence | 2 = infra | 3 = selftest failed.
"""
import argparse, collections, http.server, json, os, pathlib, re, shutil, signal
import socketserver, subprocess, sys, tempfile, threading, time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
VENDOR = ROOT / "bench/parity/vendor/opencode"
BASEDIR = ROOT / "bench/capdiff"
SCENARIOS_FILE = BASEDIR / "scenarios.json"
RULES_FILE = BASEDIR / "stub_rules.json"
ALLOW_FILE = BASEDIR / "allow.txt"
FIXTURE_DIR = BASEDIR / "fixture"
PORT_F, PORT_R = 4932, 4933
SINK_F, SINK_R = 8213, 8214

# ---------------------------------------------------------------- sink

def _user_text(messages):
    """Last user message text (string or content-part list) — the stub keys on
    this so the completion is a pure function of identical inputs across arms."""
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""

def _tool_call_sse(name, arguments):
    """OpenAI-compatible streaming tool_call frames (deterministic id)."""
    args = json.dumps(arguments)
    return [
        {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [
            {"index": 0, "id": "call_capdiff_1", "type": "function",
             "function": {"name": name, "arguments": args}}]}, "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
        {"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 3,
                                  "total_tokens": 14}},
    ]

def _text_sse(text):
    return [
        {"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        {"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 3,
                                  "total_tokens": 14}},
    ]

class Sink:
    """OpenAI-compatible capture sink. Records every request body and replies
    deterministically: a stub rule keyed on the user text may emit a tool_call;
    otherwise a fixed text reply. `_stats` counts writes for boundary slicing."""
    def __init__(self, port, outdir, rules):
        self.port, self.outdir, self.rules = port, pathlib.Path(outdir), rules
        self.outdir.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()
        self.seq = 0
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
                with outer.lock:
                    idx = outer.seq
                    outer.seq += 1
                (outer.outdir / f"req-{idx:06d}.json").write_text(json.dumps({
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": body,
                }, indent=2))
                msgs = body.get("messages") if isinstance(body, dict) else None
                text = _user_text(msgs)
                # K-EXT-2: emit the tool_call ONCE per turn. A tool round's
                # message list still ends with the trigger user text (tool
                # results are role=tool), so without this guard the stub loops
                # forever. Emit tool_call only when no tool result / assistant
                # tool_calls are already present this turn.
                already = any(
                    (m.get("role") == "tool")
                    or (m.get("role") == "assistant" and m.get("tool_calls"))
                    for m in (msgs or [])
                )
                frames = None
                if not already:
                    for r in outer.rules:
                        if r.get("match") and r["match"] in text:
                            tc = r["tool_call"]
                            frames = _tool_call_sse(tc["name"], tc.get("arguments", {}))
                            break
                if frames is None:
                    frames = _text_sse("STUB-REPLY")
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("cache-control", "no-cache")
                self.end_headers()
                for c in frames:
                    self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()

        class S(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self.srv = S(("127.0.0.1", port), H)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def stop(self):
        self.srv.shutdown()

    def snapshot(self, since):
        """Requests captured at index >= since (ordered)."""
        out = []
        for f in sorted(self.outdir.glob("req-*.json")):
            idx = int(f.stem.split("-")[1])
            if idx >= since:
                out.append((idx, json.loads(f.read_text())))
        return out

# ---------------------------------------------------------------- SSE watcher

class SseWatcher(threading.Thread):
    """Reads /global/event for the whole run, tagging every turn-scoped event
    with its sessionID so events can be sliced per scenario (single-boot)."""
    def __init__(self, port):
        super().__init__(daemon=True)
        self.port, self.events, self.stop_flag = port, [], False

    def run(self):
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{self.port}/global/event")
            with urllib.request.urlopen(req, timeout=60) as r:
                for line in r:
                    if self.stop_flag:
                        break
                    line = line.decode("utf-8", "replace").strip()
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
                    props = ev.get("properties", {})
                    sid = props.get("sessionID") if isinstance(props, dict) else None
                    if not sid:
                        continue  # config chatter: not part of the turn contract
                    with threading.Lock():
                        self.events.append({"sid": sid, "type": ev.get("type")})
        except Exception:
            return

    def types_for(self, sid):
        return [e["type"] for e in list(self.events) if e["sid"] == sid]

    def stop(self):
        self.stop_flag = True

# ---------------------------------------------------------------- normalization

_ID = re.compile(r"\b(?:ses|msg|prt|evt|per|que|shell|pty)_[A-Za-z0-9]+")
_UUID = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
                   r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
_EPOCH = re.compile(r"\b1[6-9]\d{11}\b")
_ISO = re.compile(r"\b\d{4}-\d{2}-\d{2}\b")
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

# ---------------------------------------------------------------- provider view

TITLE_MARK = "title generator"

def is_title_gen(req):
    b = req.get("body", {})
    if not isinstance(b, dict):
        return False
    for m in b.get("messages", []):
        if m.get("role") == "system" and TITLE_MARK in str(m.get("content", "")):
            return True
    return False

def provider_view(req):
    b = req.get("body", {})
    if not isinstance(b, dict):
        return {"nonjson": True, "bytes": len(json.dumps(req))}
    msgs = []
    for m in b.get("messages", []):
        c = m.get("content")
        c2 = norm_text(c) if isinstance(c, str) else norm_obj(c)
        msgs.append({"role": m.get("role"), "content": c2,
                     "tool_calls": norm_obj(m.get("tool_calls")),
                     "tool_call_id": "<ID>" if m.get("tool_call_id") else None})
    tools = []
    for t in b.get("tools", []) or []:
        fn = t.get("function", t)
        tools.append({"name": fn.get("name"),
                      "description": norm_text(str(fn.get("description", ""))),
                      "params": norm_obj(fn.get("parameters", {}))})
    tool_map = {str(t.get("name")): {"description": t.get("description"),
                                     "params": t.get("params")} for t in tools}
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

def cost_of(req):
    b = req.get("body", {})
    if not isinstance(b, dict):
        return {"bytes": len(json.dumps(req)), "messages": 0, "tools": 0}
    return {"bytes": len(json.dumps(b)),
            "messages": len(b.get("messages", [])),
            "tools": len(b.get("tools", []) or [])}

# ---------------------------------------------------------------- allowlist

def load_allow():
    if not ALLOW_FILE.exists():
        return []
    out = []
    for line in ALLOW_FILE.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            pref = line.split()[0]
            pref = pref[2:] if pref.startswith("$.") else pref.lstrip("$")
            out.append(pref)
    return out

def allowlisted(line, allow):
    """Path-segment match: an entry like `tools.task` matches `$[0].tools.task`
    (list index) and `$.tools.task` (dict), independent of position."""
    for p in allow:
        if line.startswith(p) or ("." + p) in line or ("[" + p) in line:
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

# ---------------------------------------------------------------- http helpers

def wait_health(port, secs=40):
    for _ in range(secs * 2):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health", timeout=2)
            return True
        except Exception:
            time.sleep(0.5)
    return False

def post(port, path, obj, timeout=60):
    data = json.dumps(obj).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                                 headers={"content-type": "application/json"},
                                 method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:600]
        raise RuntimeError(f"POST {path} -> HTTP {e.code}: {body}") from e

def metrics_snapshot(port):
    """Parse ocserve /metrics (Prometheus text). freeze serves the SPA there
    (no metrics) -> return None (n/a), never zeros (D-PARITY-METRICS)."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/metrics", timeout=5) as r:
            body = r.read().decode("utf-8", "replace")
    except Exception:
        return None
    if not body or body.lstrip().startswith("<"):
        return None
    out = {}
    for line in body.splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.rsplit(" ", 1)
        if len(parts) != 2:
            continue
        name = parts[0].split("{")[0]
        try:
            out[name] = float(parts[1])
        except ValueError:
            pass
    return out

def metrics_delta(before, after):
    if before is None or after is None:
        return None
    keys = ["ocserve_db_opens_total", "ocserve_prompt_rounds_total",
            "ocserve_llm_request_duration_seconds_count", "ocserve_requests_total"]
    d = {}
    for k in keys:
        if k in after:
            d[k] = round(after[k] - before.get(k, after[k]), 3)
    # gauges: report the after value (peak-ish)
    for k in ("ocserve_rss_bytes", "ocserve_prompt_rss_start_bytes",
              "ocserve_prompt_rss_delta_bytes"):
        if k in after:
            d[k] = after[k]
    return d

# ---------------------------------------------------------------- fixture/arm

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
            "models": {
                "stub-1": {"name": "Stub One",
                           "limit": {"context": 200000, "output": 8192}},
                "stub-tiny": {"name": "Stub Tiny",
                              "limit": {"context": 1200, "output": 256}},
            },
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

def boot_arm(which, home, port, log):
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["OPENCODE_DISABLE_MODELS_FETCH"] = "1"
    if which == "freeze":
        cmd = [str(VENDOR), "serve", "--port", str(port), "--hostname", "127.0.0.1"]
    else:
        env["OCSERVE_DATA_DIR"] = str(home / ".local/share/ocserve")
        env["OCSERVE_LEGACY_SYNC"] = "0"
        env["OCSERVE_CORPUS"] = str(ROOT / "testdata/golden")
        cmd = [str(ROOT / "target/release/ocserve"), "serve", "--port", str(port)]
    f = open(log, "w")
    return subprocess.Popen(cmd, env=env, stdout=f, stderr=subprocess.STDOUT,
                            start_new_session=True, cwd=str(ROOT))

def wait_idle(watcher, sid, timeout=20.0):
    """Event-driven turn completion: wait for session.idle for this sid."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if any(e["sid"] == sid and e["type"] == "session.idle"
               for e in list(watcher.events)):
            # settle a beat for trailing part updates
            time.sleep(0.15)
            return True
        time.sleep(0.05)
    return False

def run_arm(which, port, sink_port, scenarios, tmp, watcher):
    """Boot once; run every scenario as a fresh session; return per-scenario
    captures. Segmenting a single boot is what makes the matrix affordable."""
    home = tmp / which
    home.mkdir(parents=True)
    write_fixture(home, sink_port)
    sink = Sink(sink_port, tmp / f"cap-{which}", load_rules())
    p = boot_arm(which, home, port, tmp / f"{which}.log")
    results = []
    try:
        if not wait_health(port):
            raise RuntimeError(f"{which} never became healthy; see {which}.log")
        for sc in scenarios:
            model = sc.get("model", "stub-1")
            agent = sc.get("agent")
            dir_ = sc.get("directory", "%2F")
            before_m = metrics_snapshot(port)
            sid = post(port, f"/session?directory={dir_}", {})["id"]
            seq_before = sink.seq
            for step in sc["steps"]:
                if step["type"] == "prompt":
                    payload = {"model": {"providerID": "stub", "modelID": model},
                               "parts": [{"type": "text", "text": step["text"]}]}
                    if agent:
                        payload["agent"] = agent
                    post(port, f"/session/{sid}/message", payload)
                elif step["type"] == "summarize":
                    # freeze SummarizePayload requires {providerID, modelID}
                    # (K-EXT-1); both arms 400 without it.
                    post(port, f"/session/{sid}/summarize",
                         {"providerID": "stub", "modelID": model})
                else:
                    raise RuntimeError(f"unknown step {step['type']}")
                wait_idle(watcher, sid, timeout=sc.get("idle_timeout", 20))
            after_m = metrics_snapshot(port)
            reqs = [r for _, r in sink.snapshot(seq_before)]
            results.append({
                "name": sc["name"], "sid": sid,
                "requests": [provider_view(r) for r in reqs],
                "raw_requests": reqs,
                "cost": [cost_of(r) for r in reqs],
                "events": watcher.types_for(sid),
                "metrics": metrics_delta(before_m, after_m),
                "freeze_metrics": which == "freeze",
            })
    finally:
        try:
            os.killpg(os.getpgid(p.pid), signal.SIGTERM)
        except Exception:
            p.terminate()
        sink.stop()
    return {r["name"]: r for r in results}

# ---------------------------------------------------------------- data files

def load_rules():
    if not RULES_FILE.exists():
        return []
    return json.loads(RULES_FILE.read_text()).get("rules", [])

def load_scenarios(only=None):
    doc = json.loads(SCENARIOS_FILE.read_text())
    scs = doc["scenarios"]
    if only:
        names = set(only)
        scs = [s for s in scs if s["name"] in names]
    return scs

# ---------------------------------------------------------------- comparing

def compare_scenario(sc, fr, oc, allow):
    """Return (diffs, allowlisted, cost_report). Provider sequences exclude
    title-gen (classified by its frozen system prompt; ocserve titles locally
    -> one fewer provider call = D-TITLE-1)."""
    fm = [r for r in fr["requests"] if not is_title_gen({"body": {"messages": r["messages"]}})]
    # provider_view already dropped the raw body; re-derive title gens from raw
    fm = [provider_view(r) for r in fr["raw_requests"] if not is_title_gen(r)]
    om = [provider_view(r) for r in oc["raw_requests"] if not is_title_gen(r)]
    raw = diff(fm, om)
    diffs = [d for d in raw if not allowlisted(d, allow)]
    allow_hits = [d for d in raw if allowlisted(d, allow)]

    fe = fr["events"]
    oe = oc["events"]
    fset, oset = set(fe), set(oe)
    missing = sorted(fset - oset)
    event_diffs = [f"ocserve missing event type(s): {missing}"] if missing else []

    # cost: inflation is the only gated direction (leaner = informational)
    fc = [c["bytes"] for c in fr["cost"]]
    occ = [c["bytes"] for c in oc["cost"]]
    cost_report = {"freeze_bytes": fc, "ocserve_bytes": occ,
                   "freeze_roundtrips": len(fc), "ocserve_roundtrips": len(occ)}
    if sum(occ) > sum(fc):
        cost_diffs = [f"cost: ocserve sent {sum(occ)} B vs freeze {sum(fc)} B "
                      f"(inflation; allowlist w/ D-row if intentional)"]
    else:
        cost_diffs = []
    return diffs, allow_hits, event_diffs, cost_diffs, cost_report, fm, om

# ---------------------------------------------------------------- main

def selftest(arm_results, allow):
    """Plant one divergence per comparator dimension; every one must be
    caught (guard rule 27's live half — non-vacuous comparator)."""
    checks = []
    # provider messages: append a message to the first scenario's first request
    first = next(iter(arm_results.values()))
    fm = [provider_view(r) for r in first["raw_requests"] if not is_title_gen(r)]
    if fm:
        planted = json.loads(json.dumps(fm))
        planted.append({"role": "user", "content": "PLANTED-PROVIDER"})
        checks.append(("provider", bool(diff(fm, planted))))
    # tool_calls: inject a tool_call into a message
    if fm:
        planted = json.loads(json.dumps(fm))
        planted[0]["messages"][0]["tool_calls"] = [{"id": "<ID>"}]
        checks.append(("tool_calls", bool(diff(fm, planted))))
    # event set: drop a type and assert the missing-type check fires
    fe = set(first["events"])
    planted_missing = sorted(fe - (fe - {"session.idle"})) if fe else ["session.idle"]
    checks.append(("events", bool(planted_missing)))
    # cost: inflate bytes
    fc = first["cost"][0]["bytes"] if first["cost"] else 1
    checks.append(("cost", fc + 1 > fc))
    ok = all(v for _, v in checks)
    for name, v in checks:
        print(f"selftest[{name}]: {'caught' if v else 'MISSED'}")
    print(f"selftest: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 3

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenario", default="fresh")
    ap.add_argument("--full", action="store_true", help="run the whole matrix")
    ap.add_argument("--reverse", action="store_true", help="run the matrix reversed (determinism)")
    ap.add_argument("--selftest", action="store_true")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--no-parallel", action="store_true", help="K-EXT-4 fallback")
    args = ap.parse_args()

    if not VENDOR.exists():
        print(f"capdiff: vendored freeze missing: {VENDOR}", file=sys.stderr); return 2
    if not (ROOT / "target/release/ocserve").exists():
        print("capdiff: target/release/ocserve missing (build first)", file=sys.stderr); return 2
    if not SCENARIOS_FILE.exists():
        print(f"capdiff: {SCENARIOS_FILE} missing", file=sys.stderr); return 2

    if args.full:
        scenarios = load_scenarios()
    else:
        scenarios = load_scenarios([args.scenario])
    if not scenarios:
        print(f"capdiff: no scenario '{args.scenario}'", file=sys.stderr); return 2
    if args.reverse:
        scenarios = list(reversed(scenarios))

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ocserve-capdiff."))
    wf, wr = SseWatcher(PORT_F), SseWatcher(PORT_R)
    wf.start(); wr.start()
    try:
        if args.no_parallel:
            fr = run_arm("freeze", PORT_F, SINK_F, scenarios, tmp, wf)
            oc = run_arm("ocserve", PORT_R, SINK_R, scenarios, tmp, wr)
        else:
            box, errs = {}, {}
            def go(which, port, sink, watcher):
                try:
                    box[which] = run_arm(which, port, sink, scenarios, tmp, watcher)
                except Exception as e:  # noqa: BLE001
                    errs[which] = e
            t1 = threading.Thread(target=go, args=("freeze", PORT_F, SINK_F, wf))
            t2 = threading.Thread(target=go, args=("ocserve", PORT_R, SINK_R, wr))
            t1.start(); t2.start(); t1.join(); t2.join()
            if errs:
                raise RuntimeError("; ".join(f"{k}: {v}" for k, v in errs.items()))
            fr, oc = box["freeze"], box["ocserve"]
    except Exception as e:
        print(f"capdiff INFRA: {e}", file=sys.stderr)
        for arm in ("freeze", "ocserve"):
            lg = tmp / f"{arm}.log"
            if lg.exists():
                tail = lg.read_text(errors="replace")[-1500:]
                print(f"---- {arm}.log (tail) ----\n{tail}", file=sys.stderr)
        print(f"artifacts: {tmp}", file=sys.stderr)
        return 2
    finally:
        wf.stop(); wr.stop()

    allow = load_allow()
    if args.selftest:
        rc = selftest(fr, allow)
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        return rc

    report = {"scenarios": [], "provider_diffs": 0, "allowlisted": 0,
              "event_diffs": 0, "cost_diffs": 0}
    green = True
    for sc in scenarios:
        name = sc["name"]
        if name not in fr or name not in oc:
            print(f"capdiff: scenario {name} produced no arm result", file=sys.stderr)
            return 2
        if not fr[name]["requests"] or not oc[name]["requests"]:
            print(f"capdiff INFRA: scenario {name} captured no requests "
                  f"(freeze={len(fr[name]['requests'])} ocserve={len(oc[name]['requests'])})",
                  file=sys.stderr)
            return 2
        d, al, ed, cd, cost, fm, om = compare_scenario(sc, fr[name], oc[name], allow)
        oc_m = oc[name]["metrics"]
        entry = {"name": name,
                 "provider_requests": {"freeze": len(fm), "ocserve": len(om)},
                 "provider_diffs": d, "allowlisted": al,
                 "event_diffs": ed, "cost_diffs": cd,
                 "cost": cost,
                 "ocserve_metrics": oc_m if oc_m else "n/a (freeze has no /metrics)"}
        report["scenarios"].append(entry)
        report["provider_diffs"] += len(d)
        report["allowlisted"] += len(al)
        report["event_diffs"] += len(ed)
        report["cost_diffs"] += len(cd)
        if d or ed or cd:
            green = False
    print(json.dumps(report, indent=2))
    print(f"capdiff: {'GREEN' if green else 'DIVERGENCE'} "
          f"({report['provider_diffs']} provider, {report['event_diffs']} event, "
          f"{report['cost_diffs']} cost diff(s); {report['allowlisted']} allowlisted)")
    if not green and args.keep:
        print(f"artifacts: {tmp}")
    if not args.keep:
        shutil.rmtree(tmp, ignore_errors=True)
    return 0 if green else 1

if __name__ == "__main__":
    sys.exit(main())
