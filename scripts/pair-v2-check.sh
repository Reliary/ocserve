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
# The v2 CLI binary (201 MB ELF, from @opencode/cli-linux-x64@$V2_VER).
# SLOT-BASED (2026-11-05): the bump moves the pin, so this resolves the NEW
# version's tarball (staged by `bump.sh v2` in /var/tmp/ocserve-bump-<ver>)
# before falling back to the pinned scratch install. A hardcoded 2.0.28 path
# made the differential silently test the OLD binary after a bump — the same
# class as `acquire-freeze` printing success while `cp` failed.
V2_VER="${V2_VER:-$(sed -n 's/.*pub const V2_TAG: &str = "\([^"]*\)".*/\1/p' crates/ocserve-core/src/spec.rs | head -1)}"
V2_MAJOR="${V2_VER%.*}"
V2BIN="${V2_BIN:-}"
if [ -z "$V2BIN" ]; then
  _staged="/var/tmp/ocserve-bump-$V2_VER/cli/package/bin/opencode"
  if [ -x "$_staged" ]; then V2BIN="$_staged"
  else V2BIN="/var/tmp/ocserve-scratch/ocli/node_modules/@opencode/cli-linux-x64/bin/opencode"; fi
fi

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
[ -x "$V2BIN" ] || die2 "v2 binary for $V2_VER missing at $V2BIN"

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
# The new 2.2.5 families are flag-gated. Enable them in BOTH temp HOMEs so the
# vectors exercise real shapes, not just the refusal path — and never touch the
# committed fixture, which other oracles (capdiff, replay) share.
python3 - "$CFG/opencode.json" "$HF/.config/opencode/opencode.json" \
         "$HR/.config/opencode/opencode.json" <<'PYCFG'
import json, sys
src, *dsts = sys.argv[1:]
cfg = json.load(open(src))
cfg.setdefault("experimental", {}).update({"schedules": True, "artifacts": True})
for d in dsts:
    json.dump(cfg, open(d, "w"), indent=2)
PYCFG

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
[ -n "$PW" ] || { tail -20 "$HF/v2.log" >&2; die2 "$V2_VER never printed a password"; }
export PAIR_V2_PW="$PW"
python3 - "$PORT_F" "$V2_MAJOR" <<'PY' || { tail -20 "$HF/v2.log" >&2; die2 "freeze arm never healthy"; }
import base64, json, os, sys, time, urllib.request
sys.argv[2] and None
EXPECT_MAJOR = sys.argv[2]
pw = os.environ["PAIR_V2_PW"]
auth = base64.b64encode(f"opencode:{pw}".encode()).decode()
for _ in range(30):
    try:
        req = urllib.request.Request(f"http://127.0.0.1:{sys.argv[1]}/api/info",
                                     headers={"Authorization": f"Basic {auth}"})
        v = json.loads(urllib.request.urlopen(req, timeout=3).read())
        assert v["version"].startswith(EXPECT_MAJOR), v
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
python3 - "$V2_MAJOR" <<'PY'
import base64, json, os, sys, urllib.request, urllib.error
V2_MAJOR = sys.argv[1]

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
check("info-freeze", st_f == 200 and str(info.get("version", "")).startswith(V2_MAJOR),
      f"{st_f} {str(info)[:120]}")
st_r, infor = call(R, "GET", "/api/info")
check("info-enabled", st_r == 200 and str(infor.get("version", "")).startswith(V2_MAJOR),
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
st_r, mr = call(R, "GET", f"/api/session/{sid_r}/message?limit=5", auth=AUTH)
check("message-envelope", st_f == 200 and st_r == 200
      and isinstance(mf, dict) and isinstance(mr, dict)
      and "data" in mf and "data" in mr and "cursor" in mr, f"f={st_f} r={st_r}")

# Phase A+C fan-out: every TUI-boot route answers 200 on both arms.
# Consumer-driven shapes: the TUI always passes location + real project IDs
# (never bare paths), so vectors do the same — bare-path behavior is
# unspecified surface, not a parity target.
st_f, pf = call(F, "GET", "/api/project", auth=AUTH)
proj = (pf[0].get("id") if isinstance(pf, list) and pf else "global")
for name, path in [
    ("project", "/api/project"),
    ("mcp", "/api/mcp"),
    ("mcp-resource", "/api/mcp/resource"),
    ("vcs", "/api/vcs"),
    ("vcs-status", "/api/vcs/status"),
    ("vcs-branch", "/api/vcs/branch"),
    ("vcs-base", "/api/vcs/base?location[directory]=/tmp"),
    ("config", "/api/config"),
    ("config-shell", "/api/config/shell"),
    ("model-default", "/api/model/default"),
    ("debug-location", "/api/debug/location"),
    ("migration", "/api/experimental/migration/v1"),
    ("plugin", "/api/plugin"),
    ("worktree", f"/api/worktree?projectID={proj}"),
    ("active", "/api/session/active"),
]:
    st_f, _ = call(F, "GET", path, auth=AUTH)
    st_r, _ = call(R, "GET", path)
    # worktree with a real projectID: freeze 404s unknown ids (tagged
    # ProjectNotFoundError) — parity is "same status class", not 200.
    if name == "worktree":
        check(f"fanout-{name}", (st_f in (200, 404)) and st_r == 200, f"f={st_f} r={st_r}")
    else:
        check(f"fanout-{name}", st_f == 200 and st_r == 200, f"f={st_f} r={st_r}")

# Phase 5 (2.2.5): the new operation families. Every vector below was probed
# against the real 2.2.5 binary because these handlers were first written from
# the contract, and five of them were plausible-but-wrong. The families need
# their feature flags ON in the fixture config, so schedules/artifacts are
# exercised in their real shape rather than only their refusal path.
for name, method, path, body in [
    ("schedule-create-in", "POST", "/api/experimental/schedule",
     {"sessionID": "@SID", "prompt": "p", "when": {"in": "10 minutes"}}),
    ("schedule-create-cron", "POST", "/api/experimental/schedule",
     {"sessionID": "@SID", "prompt": "p", "when": {"cron": "*/5 * * * *"}}),
    ("schedule-create-badwhen", "POST", "/api/experimental/schedule",
     {"sessionID": "@SID", "prompt": "p", "when": {"nonsense": 1}}),
    ("schedule-create-empty", "POST", "/api/experimental/schedule", {}),
    ("schedule-list", "GET", "/api/experimental/schedule?sessionID=@SID", None),
    ("schedule-missing", "GET", "/api/experimental/schedule/sch_nope", None),
    ("schedule-pause-missing", "POST", "/api/experimental/schedule/sch_nope/pause", None),
    ("artifact-create", "POST", "/api/experimental/artifacts",
     {"title": "t", "files": {"a.txt": "hi"}}),
    ("artifact-badid", "GET", "/api/experimental/artifacts/nope", None),
    ("voice-models", "GET", "/api/experimental/voice/models", None),
    ("voice-empty", "POST", "/api/experimental/voice/session", {}),
    ("voice-object-model", "POST", "/api/experimental/voice/session",
     {"model": {"providerID": "x", "modelID": "y"}}),
    ("voice-string-transport", "POST", "/api/experimental/voice/session",
     {"model": "gpt-live-1", "transport": "webrtc"}),
    ("voice-unknown-model", "POST", "/api/experimental/voice/session",
     {"model": "not-a-voice-model", "transport": {"type": "websocket"}}),
    ("client-mcp-no-name", "GET", "/api/experimental/client-mcp", None),
    ("client-mcp-name", "GET", "/api/experimental/client-mcp?name=demo", None),
    ("client-mcp-token", "POST", "/api/experimental/client-mcp/token?name=demo", None),
    ("usage-limits", "GET", "/api/experimental/usage/limits", None),
    ("plugin-gui-no-id", "GET", "/api/plugin/gui", None),
    ("plugin-gui-unknown", "GET", "/api/plugin/gui?id=nope", None),
    ("fs-watch-no-path", "POST", "/api/experimental/fs/watch", {}),
    ("fs-watch-outside", "POST", "/api/experimental/fs/watch", {"path": "/etc/hostname"}),
    ("fs-watch-unwatch", "DELETE", "/api/experimental/fs/watch", {"path": "/tmp"}),
    ("heap-dump", "POST", "/api/debug/heap-dump", {}),
    ("generate-image-empty", "POST", "/api/experimental/generate/image", {}),
    ("generate-decisions-empty", "POST", "/api/experimental/generate/decisions", {}),
    ("permission-saved-missing", "PATCH", "/api/permission/saved/per_nope",
     {"action": "allow"}),
]:
    bf = body
    br = body
    if isinstance(body, dict) and body.get("sessionID") == "@SID":
        bf = {**body, "sessionID": sid_f}
        br = {**body, "sessionID": sid_r}
    pth_f = path.replace("@SID", sid_f)
    pth_r = path.replace("@SID", sid_r)
    rf = call(F, method, pth_f, bf, AUTH)
    rr = call(R, method, pth_r, br)
    st_f, bf_ = rf if isinstance(rf, tuple) else (0, {})
    st_r, br_ = rr if isinstance(rr, tuple) else (0, {})

    def _tag(b):
        return b.get("_tag") if isinstance(b, dict) else None

    # Parity = same success class, same error tag, and no contract key the
    # reference emits that we omit. Ids/timestamps are generated per arm.
    same_class = st_f == st_r or (st_f in (200, 201, 204) and st_r in (200, 201, 204))
    missing = (set(bf_) - set(br_)) if isinstance(bf_, dict) and isinstance(br_, dict) else set()
    check(f"v225-{name}", same_class and _tag(bf_) == _tag(br_) and not missing,
          f"f={st_f}/{_tag(bf_)} r={st_r}/{_tag(br_)} missing={sorted(missing)}")

# Phase 3: plugin inventory shape + RPC unregistered parity.
# Both arms return `{location, data:[...]}`; with no plugins loaded on either
# an empty data array is the honest shape (never fabricated on ocserve).
st_f, pf = call(F, "GET", "/api/plugin", auth=AUTH)
st_r, pr = call(R, "GET", "/api/plugin")
check("plugin-envelope", st_f == 200 and st_r == 200 and isinstance(pf, dict) and isinstance(pr, dict)
      and "location" in pf and "location" in pr and isinstance(pf.get("data"), list)
      and isinstance(pr.get("data"), list), f"f={st_f} r={st_r}")
# POST /api/rpc/{id}/{method} with an unregistered id: both arms answer the
# RPC-unavailable error (status + `type`), the parity contract we depend on.
rf = call(F, "POST", "/api/rpc/ghost/hello", body={"input": {}}, auth=AUTH)
rr = call(R, "POST", "/api/rpc/ghost/hello", body={"input": {}})
st_f, bf = rf if isinstance(rf, tuple) else (0, {})
st_r, br = rr if isinstance(rr, tuple) else (0, {})


def _rpc_type(b):
    if isinstance(b, dict):
        if b.get("type"):
            return b.get("type")
        d = b.get("data") or {}
        if isinstance(d, dict):
            return d.get("type")
    return None


check("rpc-unavailable", st_f == st_r and _rpc_type(bf) == _rpc_type(br) == "rpc.unavailable",
      f"f={st_f}:{_rpc_type(bf)} r={st_r}:{_rpc_type(br)}")

print(f"v2 pair: {len(fails)} divergences")
sys.exit(1 if fails else 0)
PY
rc=$?
exit "$rc"
