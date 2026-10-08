#!/usr/bin/env python3
"""One arm-run of the parity harness: S0 boot/idle → S1 seed → S2 reads →
S3 SSE fan-out → S4 post-load idle → result.json.

Usage:
  ROUND=m1u python3 lib/runner.py --arm upstream      # start + run + down
  ROUND=m1u python3 lib/runner.py --arm ocserve --no-down

Assumes images are built (`docker compose build` via run.sh preflight).
Each scenario is independently fault-isolated: a failure records `null`
with an error string — never a zero (measure_lib decidability rule).
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sampler import Sampler  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))          # .../lib
BENCH = os.path.dirname(HERE)                              # .../parity
REPO = os.path.dirname(os.path.dirname(BENCH))             # ocserve repo root

PORTS = {"upstream": 4921, "ocserve": 4922}
SERVICES = {"upstream": "upstream", "ocserve": "ocserve"}


def env_num(name: str, default: float) -> float:
    return float(os.environ.get(name, default))


class Err(Exception):
    pass


def jreq(base, method, path, body=None, timeout=90):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, method=method, data=data,
                                 headers={"Content-Type": "application/json"}
                                 if body is not None else {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def compose(round_id: str, *args, timeout=240):
    env = {**os.environ, "ROUND": round_id}
    return subprocess.run(
        ["docker", "compose", "-f", os.path.join(BENCH, "compose.yaml"), *args],
        capture_output=True, text=True, timeout=timeout, env=env, cwd=BENCH,
    )


def wait_health(base: str, budget: float) -> float:
    t0 = time.monotonic()
    last = None
    while time.monotonic() - t0 < budget:
        try:
            urllib.request.urlopen(base + "/global/health", timeout=3).read()
            return time.monotonic() - t0
        except Exception as e:
            last = e
            time.sleep(0.05)
    raise Err(f"health budget {budget}s exhausted: {last}")


def sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()


def assistant_texts(base: str, sid: str) -> list[str]:
    st, raw = jreq(base, "GET", f"/session/{sid}/message")
    if st != 200:
        raise Err(f"GET messages {st}: {raw[:200]!r}")
    out = []
    for m in json.loads(raw):
        if m["info"].get("role") != "assistant":
            continue
        t = "\n".join(p.get("text") or "" for p in m.get("parts", [])
                      if p.get("type") == "text").strip()
        if t:
            out.append(t)
    return out


def timed_request(conn_cls, host, port, method, path, body=None, timeout=30):
    """Returns (ttfb_s, total_s, status, body_bytes, headers_dict)."""
    conn = conn_cls(host, port)
    t0 = time.perf_counter()
    try:
        conn.connect()
        hdr = {"Content-Type": "application/json"} if body is not None else {}
        conn.request(method, path, body=json.dumps(body) if body is not None else None,
                     headers=hdr)
        resp = conn.getresponse()
        ttfb = time.perf_counter() - t0
        data = resp.read()
        total = time.perf_counter() - t0
        headers = dict(resp.getheaders())
        return ttfb, total, resp.status, data, headers
    finally:
        conn.close()


def pct(xs, p):
    if not xs:
        return None
    ys = sorted(xs)
    k = min(len(ys) - 1, max(0, int(round(p / 100 * (len(ys) + 1))) - 1))
    return round(ys[k], 4)


def sse_subscriber(port: int, sink: list, stop: threading.Event, lock: threading.Lock):
    """Raw /global/event reader with HTTP chunked decoding.

    The upstream (and axum) SSE streams are Transfer-Encoding: chunked —
    naively concatenating TCP reads glues chunk-size digits into event lines
    when boundaries split a line (observed: bare '65'/'f0' lines). Decode
    chunks properly, then split events.
    """
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=5)
        s.sendall(b"GET /global/event HTTP/1.1\r\nHost: bench\r\n"
                  b"Accept: text/event-stream\r\n\r\n")
        s.settimeout(1.0)
        buf = b""
        body = False
        # dechunk state: None = need size line; else remaining bytes in chunk
        remaining = None
        payload = b""

        def emit_lines(data: bytes, now: float):
            nonlocal payload
            payload += data
            lines = payload.split(b"\n")
            payload = lines.pop()
            for ln in lines:
                ln = ln.strip()
                if ln.startswith(b"data:"):
                    with lock:
                        sink.append((now, ln.decode("utf-8", "replace")))

        while not stop.is_set():
            try:
                chunk = s.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            now = time.monotonic()
            buf += chunk
            if not body:
                if b"\r\n\r\n" not in buf:
                    continue
                head, buf = buf.split(b"\r\n\r\n", 1)
                if b" 200" not in head.split(b"\r\n")[0]:
                    with lock:
                        sink.append((now, f"__handshake__ {head.split(b'\\r\\n')[0][:80]!r}"))
                    break
                body = True
            # consume complete chunks from buf
            while True:
                if remaining is None:
                    i = buf.find(b"\r\n")
                    if i < 0:
                        break
                    size_line = buf[:i]
                    buf = buf[i + 2:]
                    hexpart = size_line.split(b";", 1)[0].strip()
                    try:
                        remaining = int(hexpart, 16)
                    except ValueError:
                        remaining = 0
                    if remaining == 0:
                        # last chunk — keep-alive possible; stream ends here
                        stop.set()
                        break
                else:
                    if len(buf) < remaining + 2:
                        break
                    data = buf[:remaining]
                    buf = buf[remaining + 2:]  # data + CRLF
                    remaining = None
                    emit_lines(data, now)
        s.close()
    except Exception as e:
        with lock:
            sink.append((time.monotonic(), f"__subscriber_error__ {e}"))


def scrape_metrics(port: int) -> dict:
    """Parse the Prometheus text exposition at /metrics → {name: float}."""
    try:
        with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/metrics", timeout=5) as resp:
            out = {}
            for line in resp.read().decode("utf-8", "replace").splitlines():
                if not line or line.startswith("#"):
                    continue
                parts = line.rsplit(None, 1)
                if len(parts) == 2:
                    try:
                        out[parts[0].split("{", 1)[0]] = float(parts[1])
                    except ValueError:
                        pass
            return out
    except Exception:
        return {}


def count_sockets(port: int) -> int:
    """Established TCP sockets on the arm's port (client connection count)."""
    out = subprocess.run(
        ["ss", "-tn", "state", "established", f"( sport = :{port} )"],
        capture_output=True, text=True, timeout=10,
    )
    return max(0, len([l for l in out.stdout.splitlines() if l.strip()]) - 1)


def sse_reader(port: int, sink: list, stop: threading.Event,
               lock: threading.Lock, mode: str = "live"):
    """SSE subscriber with a drain policy:
    live    — read as fast as possible (healthy client)
    slow    — one line per 5s (phone on a bad network)
    stalled — complete the handshake then STOP reading (backgrounded app;
              TCP backpressure parks the server's write; ring must hold)."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=5)
        s.sendall(b"GET /global/event HTTP/1.1\r\nHost: bench\r\n"
                  b"Accept: text/event-stream\r\n\r\n")
        s.settimeout(1.0)
        buf = b""
        handshake = True
        while not stop.is_set():
            if mode == "stalled" and not handshake:
                time.sleep(0.25)  # hold the socket open, read NOTHING
                continue
            try:
                chunk = s.recv(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if not chunk:
                break
            now = time.monotonic()
            buf += chunk
            if handshake and b"\r\n\r\n" in buf:
                buf = buf.split(b"\r\n\r\n", 1)[1]
                handshake = False
                with lock:
                    sink.append((now, f"__handshake__{mode}"))
                continue
            lines = buf.split(b"\n")
            buf = lines.pop()
            for ln in lines:
                ln = ln.strip()
                if ln.startswith(b"data:"):
                    with lock:
                        sink.append((now, ln.decode("utf-8", "replace")))
            if mode == "slow":
                time.sleep(5)
        s.close()
        with lock:
            sink.append((time.monotonic(), f"__closed__{mode}"))
    except Exception as e:
        with lock:
            sink.append((time.monotonic(), f"__reader_error__{mode}:{e}"))


def run(arm: str, round_id: str, no_down: bool) -> dict:
    base = f"http://127.0.0.1:{PORTS[arm]}"
    svc = SERVICES[arm]
    out_dir = os.path.join(BENCH, ".runs", round_id, arm)
    os.makedirs(out_dir, exist_ok=True)
    res: dict = {
        "arm": arm, "round": round_id, "started_utc": int(time.time()),
        "errors": [], "scenarios": {},
    }

    load1 = os.getloadavg()[0]
    max_load1 = env_num("MAX_LOAD1", 1.5)
    res["host_load1_start"] = round(load1, 2)
    res["max_load1"] = max_load1
    if load1 > max_load1:
        raise Err(f"quiet-host gate: loadavg1 {load1:.2f} > {max_load1} "
                  f"(MAX_LOAD1 override to force)")

    try:
        sha_out = subprocess.run(["git", "-C", REPO, "rev-parse", "--short", "HEAD"],
                                 capture_output=True, text=True, timeout=10)
        res["ocserve_git_sha"] = sha_out.stdout.strip() or None
    except Exception:
        res["ocserve_git_sha"] = None

    # ---- start containers ----
    t_up = time.monotonic()
    r = compose(round_id, "up", "-d", "stub", svc)
    if r.returncode != 0:
        raise Err(f"compose up: {r.stderr[-400:]}")
    compose_up_s = round(time.monotonic() - t_up, 3)
    try:
        # wait_health starts counting AFTER compose returns — true boot =
        # compose invocation → healthy (both arms include compose equally)
        wait_health(base, env_num("BOOT_BUDGET_S", 120))
        boot_s = round(time.monotonic() - t_up, 3)
    except Err as e:
        res["errors"].append(f"S0 boot: {e}")
        boot_s = None
        res["scenarios"]["S0"] = {"boot_s": None, "error": str(e)}
        return res  # nothing else can run
    res["scenarios"]["S0"] = {"boot_s": boot_s, "compose_up_s": compose_up_s}

    sampler = Sampler(f"parity-{svc}-1", os.path.join(out_dir, "sampler.csv"))
    sampler.start()

    try:
        # ---- S0: settle to idle (sampled) ----
        idle_s = env_num("S0_IDLE_S", 45)
        time.sleep(idle_s)
        res["scenarios"]["S0"]["idle_window_s"] = idle_s

        # ---- S1: seed through the stub ----
        k = int(env_num("S1_SESSIONS", 6))
        m = int(env_num("S1_PROMPTS", 4))
        lat, digests = [], []
        t0 = time.monotonic()
        sids = []
        for i in range(k):
            st, raw = jreq(base, "POST", "/session", {"title": f"seed-{round_id}-{i}"})
            if st not in (200, 201):
                raise Err(f"session create {st}: {raw[:160]!r}")
            sid = json.loads(raw)["id"]
            sids.append(sid)
            for j in range(m):
                text = (f"Seed prompt session={i} turn={j}: report the status "
                        f"for fixture region {i}-{j} exactly as configured.")
                ts = time.monotonic()
                st, raw = jreq(base, "POST", f"/session/{sid}/message",
                               {"parts": [{"type": "text", "text": text}],
                                "model": {"providerID": "stub", "modelID": "stub-1"}},
                               timeout=120)
                lat.append(round(time.monotonic() - ts, 4))
                if st != 200:
                    raise Err(f"seed msg {i}.{j} HTTP {st}: {raw[:160]!r}")
        res["scenarios"]["S1"] = {
            "sessions": k, "prompts_per_session": m,
            "wall_s": round(time.monotonic() - t0, 3),
            "latency_s": {"p50": pct(lat, 50), "p95": pct(lat, 95),
                          "n": len(lat), "raw": lat},
        }
        # persisted-content digests (cross-arm equality proof)
        all_texts = []
        for sid in sids:
            all_texts.extend(assistant_texts(base, sid))
        res["seed_digests"] = [sha(t) for t in all_texts]
        res["seed_word_counts"] = [len(t.split()) for t in all_texts]
        res["seed_messages"] = len(all_texts)

        # ---- S2: shared read routes ----
        iters = int(env_num("S2_ITERS", 30))
        sid0 = sids[0]
        # find a cursor page (limit=5 → second page via X-Next-Cursor)
        st, raw = jreq(base, "GET", f"/session/{sid0}/message?limit=2")
        cursor = None
        # re-request to capture headers
        try:
            req = urllib.request.Request(base + f"/session/{sid0}/message?limit=2")
            with urllib.request.urlopen(req, timeout=10) as resp:
                cursor = resp.getheader("X-Next-Cursor")
        except Exception:
            cursor = None
        routes = [
            ("session_list", "GET", "/session?limit=10", None),
            ("message_page", "GET", f"/session/{sid0}/message?limit=2", None),
            ("message_cursor", "GET",
             f"/session/{sid0}/message?limit=2&before={cursor}" if cursor else None, None),
            ("file_list", "GET", "/file?path=/root", None),
            ("config", "GET", "/config", None),
            ("agent", "GET", "/agent", None),
            ("command", "GET", "/command", None),
        ]
        s2 = {}
        for name, method, path, _ in routes:
            if path is None:
                s2[name] = {"error": "no cursor from first page", "n": 0}
                continue
            ttfbs, totals, statuses = [], [], []
            err = None
            for _ in range(iters):
                try:
                    ttfb, total, status, _, _ = timed_request(
                        http.client.HTTPConnection, "127.0.0.1", PORTS[arm],
                        method, path)
                    ttfbs.append(round(ttfb, 5))
                    totals.append(round(total, 5))
                    statuses.append(status)
                except Exception as e:
                    err = f"{type(e).__name__}: {e}"
                    break
            ok = [t for t, s in zip(totals, statuses) if s == 200]
            s2[name] = {
                "n": len(ttfbs), "ok200": len(ok), "error": err,
                "total_p50": pct(ok, 50), "total_p95": pct(ok, 95),
                "ttfb_p50": pct([t for t, s in zip(ttfbs, statuses) if s == 200], 50),
            }
            if err:
                res["errors"].append(f"S2 {name}: {err}")
        res["scenarios"]["S2"] = s2

        # ---- S3: SSE fan-out ----
        # Correlation: FRESH session per write, match events by sessionID
        # (upstream generates its own message ids — client messageId is
        # ignored there — so mid-based matching never fires on one arm).
        subs = int(env_num("S3_SUBS", 4))
        writes = int(env_num("S3_WRITES", 4))
        lock = threading.Lock()
        sink: list = []
        stop = threading.Event()
        threads = [threading.Thread(target=sse_subscriber,
                                    args=(PORTS[arm], sink, stop, lock),
                                    daemon=True)
                   for _ in range(subs)]
        for t in threads:
            t.start()
        time.sleep(0.7)  # handshakes (server.connected) land
        first_lags, idle_lags, s3_err = [], [], None
        t_win0 = time.monotonic()
        try:
            for w in range(writes):
                st, raw = jreq(base, "POST", "/session", {"title": f"sse-{w}"})
                sid = json.loads(raw)["id"]
                needle = f'"sessionID":"{sid}"'
                ts = time.monotonic()
                st, raw = jreq(base, "POST", f"/session/{sid}/prompt_async",
                               {"parts": [{"type": "text",
                                           "text": f"SSE burst write {w}"}],
                                "model": {"providerID": "stub",
                                          "modelID": "stub-1"}}, timeout=30)
                if st != 204:
                    s3_err = f"prompt_async {st}: {raw[:160]!r}"
                    break
                deadline = time.monotonic() + 45
                got_first = got_idle = False
                while time.monotonic() < deadline and not (got_first and got_idle):
                    with lock:
                        hits = [(tline, line) for tline, line in sink
                                if tline >= ts and needle in line]
                    if not got_first and hits:
                        first_lags.append(round(min(t for t, _ in hits) - ts, 4))
                        got_first = True
                    if not got_idle and any("session.idle" in line
                                            for _, line in hits):
                        got_idle = True
                        idle_lags.append(round(
                            max(t for t, line in hits
                                if "session.idle" in line) - ts, 4))
                    time.sleep(0.02)
                if not got_first:
                    s3_err = f"no event for {sid} within 45s"
                    break
        except Exception as e:
            s3_err = f"{type(e).__name__}: {e}"
        window = time.monotonic() - t_win0
        stop.set()
        for t in threads:
            t.join(timeout=3)
        with lock:
            n_lines = sum(1 for _, line in sink if not line.startswith("__"))
            sub_errors = [line for _, line in sink if line.startswith("__")]
        res["scenarios"]["S3"] = {
            "subscribers": subs, "writes": writes,
            "completed_first": len(first_lags), "completed_idle": len(idle_lags),
            "lag_p50": pct(first_lags, 50), "lag_p95": pct(first_lags, 95),
            "idle_lag_p50": pct(idle_lags, 50),
            "idle_lag_p95": pct(idle_lags, 95),
            "events_per_s": round(n_lines / window, 2) if window > 0 else None,
            "window_s": round(window, 3), "error": s3_err,
            "subscriber_errors": sub_errors[:3],
        }
        if s3_err:
            res["errors"].append(f"S3: {s3_err}")

        # ---- S4: post-load idle (leak slope) ----
        s4 = env_num("S4_SECONDS", 120)
        time.sleep(s4)
        res["scenarios"]["S4"] = {"idle_s": s4, "anom_slope_mb_per_h": None}

        # ---- S5: connection storm (stalled/slow/live readers + big frames)
        s5_secs = env_num("S5_SECONDS", 0)
        if s5_secs > 0:
            res["scenarios"]["S5"] = run_s5(
                base, arm, int(s5_secs), sampler, out_dir)

    finally:
        summ = sampler.stop()
        slope = sampler.slope_mb_per_hour()
        res["scenarios"].setdefault("S4", {})
        res["scenarios"]["S4"]["anom_slope_mb_per_h"] = slope
        res["resources"] = summ

    # ---- disk footprint ----
    data_dir = os.path.join(BENCH, ".runs", round_id, arm)
    db_bytes = 0
    for root, _, files in os.walk(data_dir):
        for f in files:
            if f.endswith((".db", ".db-wal", ".db-shm")) or f == "opencode.db":
                db_bytes += os.path.getsize(os.path.join(root, f))
    res["db_bytes"] = db_bytes
    res["host_load1_end"] = round(os.getloadavg()[0], 2)
    res["finished_utc"] = int(time.time())

    path = os.path.join(out_dir, "result.json")
    with open(path, "w") as f:
        json.dump(res, f, indent=1)
    print(f"[{arm}/{round_id}] done → {path} errors={len(res['errors'])}")
    if not no_down:
        compose(round_id, "down", timeout=180)
    return res


def run_s5(base: str, arm: str, s5_secs: float, sampler, out_dir: str) -> dict:
    """Connection storm: L live + S slow + X stalled SSE readers against
    sustained big-frame writes. Records — no invented thresholds:
    anon curve at phase marks, /metrics deltas (ring lag/evicted/bytes),
    healthy-reader delivery, socket counts before/after, storm prompt
    latencies (compared to S1 in the report)."""
    live_n = int(env_num("S5_LIVE", 4))
    slow_n = int(env_num("S5_SLOW", 2))
    stalled_n = int(env_num("S5_STALLED", 2))
    port = PORTS[arm]
    out: dict = {"readers": {"live": live_n, "slow": slow_n, "stalled": stalled_n},
                 "errors": []}
    lock = threading.Lock()
    sink: list = []
    stop = threading.Event()

    m0 = scrape_metrics(port)
    socks0 = count_sockets(port)
    out["sockets_before"] = socks0
    out["metrics_before"] = {k: m0.get(k) for k in
                             ("ocserve_event_ring_lag_total",
                              "ocserve_event_ring_evicted_total",
                              "ocserve_event_ring_bytes",
                              "ocserve_event_ring_depth",
                              "ocserve_sse_clients")}

    readers = []
    for mode, n in (("live", live_n), ("slow", slow_n), ("stalled", stalled_n)):
        for _ in range(n):
            t = threading.Thread(target=sse_reader,
                                 args=(port, sink, stop, lock, mode), daemon=True)
            t.start()
            readers.append(t)
    time.sleep(3)  # handshakes
    marks: dict = {"t_start": time.time()}
    counts = {"live": 0, "slow": 0, "stalled": 0}
    with lock:
        for _, line in sink:
            for mode in counts:
                if line == f"__handshake__{mode}":
                    counts[mode] += 1
    out["handshakes"] = counts

    phase_a = 60.0
    time.sleep(phase_a)
    marks["t_phaseA_end"] = time.time()
    m_a = scrape_metrics(port)

    # Phase B: sustained writes with BIG frames (stub forced via env)
    phase_b = max(60.0, s5_secs - phase_a - 60.0)
    t_b0 = time.monotonic()
    storm_lat = []
    st, raw = jreq(base, "POST", "/session", {"title": "s5-storm"})
    sid = json.loads(raw)["id"] if st == 200 else None
    write_err = None
    while sid and time.monotonic() - t_b0 < phase_b:
        ts = time.monotonic()
        try:
            st, raw = jreq(base, "POST", f"/session/{sid}/message",
                           {"parts": [{"type": "text",
                                       "text": f"storm frame {int(ts)}"}],
                            "model": {"providerID": "stub",
                                      "modelID": "stub-1"}}, timeout=90)
            if st == 200:
                storm_lat.append(round(time.monotonic() - ts, 4))
            else:
                write_err = f"storm write {st}: {raw[:120]!r}"
                break
        except Exception as e:
            write_err = f"storm write: {type(e).__name__}: {e}"
            break
        time.sleep(max(0.0, 5.0 - (time.monotonic() - ts)))
    marks["t_phaseB_end"] = time.time()
    m_b = scrape_metrics(port)
    out["storm_writes"] = len(storm_lat)
    out["storm_latency"] = {"p50": pct(storm_lat, 50), "p95": pct(storm_lat, 95),
                            "n": len(storm_lat)}
    if write_err:
        out["errors"].append(write_err)

    phase_c = 60.0
    time.sleep(phase_c)
    marks["t_phaseC_end"] = time.time()
    m_c = scrape_metrics(port)

    # healthy readers were served during the storm?
    with lock:
        data_lines = [(t, ln) for t, ln in sink
                      if not ln.startswith("__")]
        out["frames_seen_by_readers"] = len(data_lines)
        out["reader_errors"] = [ln for _, ln in sink
                                if ln.startswith("__reader_error__")][:4]
        out["closed_events"] = [ln for _, ln in sink
                                if ln.startswith("__closed__")]

    stop.set()
    for t in readers:
        t.join(timeout=5)
    time.sleep(2)  # let server-side tasks tear down
    socks1 = count_sockets(port)
    m1 = scrape_metrics(port)
    out["sockets_after"] = socks1
    out["metrics_after"] = {k: m1.get(k) for k in
                            ("ocserve_event_ring_lag_total",
                             "ocserve_event_ring_evicted_total",
                             "ocserve_event_ring_bytes",
                             "ocserve_event_ring_depth",
                             "ocserve_sse_clients")}
    out["metrics_phase_a"] = {k: m_a.get(k) for k in
                              ("ocserve_event_ring_bytes",
                               "ocserve_event_ring_depth")}
    out["metrics_phase_b"] = {k: m_b.get(k) for k in
                              ("ocserve_event_ring_bytes",
                               "ocserve_event_ring_depth")}
    out["metrics_phase_c"] = {k: m_c.get(k) for k in
                              ("ocserve_event_ring_bytes",
                               "ocserve_event_ring_depth")}

    def delta(k):
        a, b = out["metrics_before"].get(k), out["metrics_after"].get(k)
        if a is None or b is None:
            return None
        return b - a

    out["ring_lag_delta"] = delta("ocserve_event_ring_lag_total")
    out["ring_evicted_delta"] = delta("ocserve_event_ring_evicted_total")
    # factual booleans only (no invented thresholds):
    out["assert_lag_or_evict_fired"] = bool(
        (out["ring_lag_delta"] or 0) > 0 or (out["ring_evicted_delta"] or 0) > 0)
    out["assert_sockets_clean"] = socks1 <= socks0
    out["assert_readers_handshook"] = (
        counts["live"] == live_n and counts["slow"] == slow_n
        and counts["stalled"] == stalled_n)
    out["assert_healthy_got_frames"] = out["frames_seen_by_readers"] > 0

    # anon curve at marks (from the sampler's full series)
    curve = {}
    for name, ts in marks.items():
        rows = [r for r in sampler.rows if r[0] >= ts and r[2] >= 0]
        if rows:
            curve[name] = rows[-1][2]
    out["anon_curve"] = curve
    out["marks"] = marks
    # persist side artifacts
    with open(os.path.join(out_dir, "s5_events.json"), "w") as f:
        json.dump({"frames": len(sink),
                   "sample_lines": [ln[:200] for _, ln in sink[:20]]}, f, indent=1)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", choices=["upstream", "ocserve"], required=True)
    ap.add_argument("--round", default=os.environ.get("ROUND"))
    ap.add_argument("--no-down", action="store_true")
    args = ap.parse_args()
    if not args.round:
        ap.error("ROUND env or --round required")
    try:
        res = run(args.arm, args.round, args.no_down)
        sys.exit(0 if not res.get("errors") else 2)
    except Err as e:
        print(f"FATAL: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
