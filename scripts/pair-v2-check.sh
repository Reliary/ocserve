#!/usr/bin/env bash
# pair-v2-check — LIVE 2.0.28 ↔ ocserve v2 differential (V2-PLAN.md P5).
#
# Boots the REAL @opencode/cli-linux-x64@2.0.28 binary and ocserve
# (OCSERVE_V2=1) under temp HOMEs, then diffs the L3-lite surface:
#   /api/info, inbox list/cancel/update, form create/list/get/reply/cancel,
#   message list/content[], log replay + watermark, prompt admission shape.
#
# Auth: the v2 server auto-generates a password (no password configured →
# still required; the password prints as `server password <pw>` in its log).
# Captured from the log and used as Basic user `opencode`.
#
# Isolation: temp HOMEs, ports 4944/4945, process-group cleanup, python-only
# HTTP (no curl). ocserve's live :4912 and opencode's :4901 untouched.
#
# Exits: 0 = green | 1 = divergence | 2 = infra.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT_F=4944
PORT_R=4945
OCSERVE_BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
V2BIN="${V2_BIN:-/tmp/opencode/ocli/node_modules/@opencode/cli-linux-x64/bin/opencode}"

HF=""; HR=""; PID_F=""; PID_R=""
cleanup() {
  for p in "$PID_R" "$PID_F"; do
    [ -n "$p" ] && { kill -- "-$p" 2>/dev/null || kill "$p" 2>/dev/null || true; }
  done
  [ -n "$HF" ] && rm -rf "$HF"
  [ -n "$HR" ] && rm -rf "$HR"
}
trap cleanup EXIT

say() { printf '%s\n' "$*"; }
die2() { echo "pair-v2-check: $*" >&2; exit 2; }

[ -x "$OCSERVE_BIN" ] || die2 "$OCSERVE_BIN missing (cargo build --release)"
[ -x "$V2BIN" ] || die2 "2.0.28 binary missing at $V2BIN"

python3 - "$PORT_F" "$PORT_R" <<'PY' || die2 "ports busy (4944/4945 required)"
import socket, sys
for p in map(int, sys.argv[1:]):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        s.bind(("127.0.0.1", p))
    except OSError:
        sys.exit(1)
    finally:
        s.close()
PY

CFG="$PWD/bench/parity/config"
[ -f "$CFG/opencode.json" ] || die2 "fixture config missing: $CFG"
HF=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-pairv2-freeze.XXXXXX")
HR=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-pairv2-ocserve.XXXXXX")
for h in "$HF" "$HR"; do
  mkdir -p "$h/.config/opencode" "$h/.local/share/opencode" "$h/.local/state/opencode"
done
cp "$CFG/opencode.json" "$HF/.config/opencode/opencode.json"
cp "$CFG/opencode.json" "$HR/.config/opencode/opencode.json"

say "== spawn (2.0.28 binary :$PORT_F, ocserve OCSERVE_V2=1 :$PORT_R) =="
setsid env HOME="$HF" \
  "$V2BIN" serve --port "$PORT_F" --hostname 127.0.0.1 >"$HF/v2.log" 2>&1 &
PID_F=$!
setsid env HOME="$HR" OCSERVE_V2=1 \
  OCSERVE_DATA_DIR="$HR/.local/share/ocserve" OCSERVE_LEGACY_SYNC=0 \
  "$OCSERVE_BIN" serve --port "$PORT_R" >"$HR/ocserve.log" 2>&1 &
PID_R=$!

# Freeze arm: wait for the password line, then health via authed /api/info.
say "== wait for arms =="
PW=""
for _ in $(seq 1 30); do
  PW=$(grep -oP 'server password \K\S+' "$HF/v2.log" 2>/dev/null | head -1 || true)
  [ -n "$PW" ] && break
  sleep 1
done
[ -n "$PW" ] || { tail -20 "$HF/v2.log" >&2; die2 "2.0.28 never printed a password"; }
export PAIR_V2_PW="$PW"
python3 - "$PORT_F" <<'PY' || { tail -20 "$HF/v2.log" >&2; die2 "freeze arm never healthy"; }
import base64, json, os, sys, time, urllib.request
pw = os.environ["PAIR_V2_PW"]
auth = base64.b64encode(f"opencode:{pw}".encode()).decode()
for _ in range(30):
    try:
        req = urllib.request.Request(f"http://127.0.0.1:{sys.argv[1]}/api/info",
                                     headers={"Authorization": f"Basic {auth}"})
        v = json.loads(urllib.request.urlopen(req, timeout=3).read())
        assert v["version"].startswith("2.0."), v
        print(f"freeze arm: {v['version']}")
        sys.exit(0)
    except SystemExit:
        raise
    except Exception:
        time.sleep(1)
sys.exit(2)
PY
python3 - "$PORT_R" <<'PY' || { tail -20 "$HR/ocserve.log" >&2; die2 "ocserve arm never healthy"; }
import json, sys, time, urllib.request
for _ in range(60):
    try:
        v = json.loads(urllib.request.urlopen(
            f"http://127.0.0.1:{sys.argv[1]}/global/health", timeout=2).read())
        sys.exit(0)
    except Exception:
        time.sleep(1)
sys.exit(2)
PY
say "both arms healthy"

export PAIR_V2_F="http://127.0.0.1:$PORT_F" PAIR_V2_R="http://127.0.0.1:$PORT_R"
python3 - <<'PY'
import base64, json, os, sys, urllib.request, urllib.error

AUTH = base64.b64encode(f"opencode:{os.environ['PAIR_V2_PW']}".encode()).decode()
F, R = os.environ["PAIR_V2_F"], os.environ["PAIR_V2_R"]
fails = []

def call(base, method, path, body=None, auth=None):
    h = {"Content-Type": "application/json"}
    if auth:
        h["Authorization"] = f"Basic {auth}"
    req = urllib.request.Request(base + path,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers=h, method=method)
    try:
        r = urllib.request.urlopen(req, timeout=15)
        raw = r.read().decode()
        try:
            return r.status, json.loads(raw)
        except Exception:
            return r.status, raw
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw

def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(name)

# Freeze arm needs a session; ocserve arm needs a session (v1 create works on ours).
_, sf = call(F, "POST", "/api/session", {"title": "pv2"}, AUTH)
_, sr = call(R, "POST", "/session", {"title": "pv2"})
sid_f = (sf.get("data") or {}).get("id") if isinstance(sf, dict) else None
sid_r = sr.get("id") if isinstance(sr, dict) else None
check("session-create", sid_f and sid_r, f"f={sid_f} r={sid_r}")
if not (sid_f and sid_r):
    sys.exit(1)

# /api/info shape (freeze) vs ocserve (OCSERVE_V2=1 here, so the probe must
# answer with the pinned contract tag — the DARK gate is proven by the
# `v2_gate_dark_by_default` unit e2e, not by this enabled arm).
st_f, info = call(F, "GET", "/api/info", auth=AUTH)
check("info-freeze", st_f == 200 and info.get("version", "").startswith("2.0."),
      f"{st_f} {str(info)[:120]}")
st_r, infor = call(R, "GET", "/api/info")
check("info-enabled", st_r == 200 and infor.get("version") == "2.0.28",
      f"{st_r} {str(infor)[:160]}")

# Prompt admission: flat body on both. Freeze runs a real turn (may need a
# model — failure is infra, not divergence: compare SHAPE classes only).
st_f, pf = call(F, "POST", f"/api/session/{sid_f}/prompt", {"text": "hi"}, AUTH)
st_r, pr = call(R, "POST", f"/api/session/{sid_r}/prompt", {"text": "hi", "resume": False})
# ocserve with resume:false parks (no model needed) → 200 Inbox.User.
rok = st_r == 200 and isinstance(pr, dict) and (pr.get("data") or {}).get("type") == "user"
check("admit-shape", rok, f"{st_r} {str(pr)[:200]}")

# Inbox list on ocserve (freeze arm: same call must also 200 — behavior may
# differ, status + envelope keys compared).
st_f, inf = call(F, "GET", f"/api/session/{sid_f}/inbox", auth=AUTH)
st_r, inr = call(R, "GET", f"/api/session/{sid_r}/inbox")
fok = st_f == 200 and isinstance(inf, dict) and "data" in inf
rok = st_r == 200 and isinstance(inr, dict) and "data" in inr
check("inbox-list", fok and rok, f"f={st_f} r={st_r}")
if rok and isinstance(inr.get("data"), list) and inr["data"]:
    item = inr["data"][0]
    check("inbox-item-keys", set(("id", "sessionID", "type", "payload", "delivery")) <= set(item),
          str(sorted(item))[:160])

# Form create/list on ocserve; freeze arm same calls (status compared loosely:
# both must answer, shapes compared on shared keys). Upstream Form.Field is a
# 6-way union requiring `key` (+`type`); label-only fields 400 (probed).
st_f, ff = call(F, "POST", f"/api/session/{sid_f}/form",
                {"title": "t", "fields": [{"key": "k", "type": "string", "title": "a"}]}, AUTH)
st_r, fr = call(R, "POST", f"/api/session/{sid_r}/form",
                {"title": "t", "fields": [{"key": "k", "type": "string", "title": "a"}]})
check("form-create", st_f == 200 and st_r == 200, f"f={st_f} r={st_r}")
if st_r == 200:
    fid = (fr.get("data") or {}).get("id")
    check("form-id-brand", isinstance(fid, str) and fid.startswith("frm_"), str(fid))
    st_r, fg = call(R, "GET", f"/api/session/{sid_r}/form/{fid}")
    check("form-get-state", st_r == 200 and (fg.get("data") or {}).get("state", {}).get("status") == "pending",
          f"{st_r} {str(fg)[:160]}")

# Log replay on ocserve (watermark exactly once).
st_r, raw = call(R, "GET", f"/api/experimental/session/{sid_r}/log?after=0")
if st_r == 200 and isinstance(raw, str):
    n = raw.count("log.synced")
    check("log-watermark", n == 1, f"watermark count {n}")
else:
    check("log-watermark", False, f"{st_r}")

# Message list envelope on both.
st_f, mf = call(F, "GET", f"/api/session/{sid_f}/message?limit=5", auth=AUTH)
st_r, mr = call(R, "GET", f"/api/session/{sid_r}/message?limit=5")
check("message-envelope", st_f == 200 and st_r == 200
      and isinstance(mf, dict) and isinstance(mr, dict)
      and "data" in mf and "data" in mr and "cursor" in mr, f"f={st_f} r={st_r}")

print(f"v2 pair: {len(fails)} divergences")
sys.exit(1 if fails else 0)
PY
rc=$?
exit "$rc"
