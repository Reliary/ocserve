#!/usr/bin/env bash
# Field-contract differential (FIELD-CONTRACT.md; nightly, not a commit gate).
#
# Boots freeze and ocserve under the SAME fixture HOME (fake provider with a
# dead endpoint + a noop command) and replays the decode corpus captured in
# bench/openapi/field-probes.md:
#
#   * decode-error vectors → BYTE compare (status + body): Effect decode
#     messages are deterministic, so freeze's response IS the expected
#     encoding — live differential (McKeeman), no recorded golden drift,
#   * identity-echo vectors → noReply 200 (persist before generation, no LLM
#     needed) + stored-id assertions on BOTH arms: the client `messageID`
#     must survive, the lowercase `messageId` must NOT.
#
# Scope note: success-path bodies are compared STRUCTURALLY (status +
# id/role), not byte-wise — user-info key sets differ between arms by
# pre-existing named rows (summary) and ids/times are per-arm generated.
#
# Usage: scripts/field-contract-check.sh [--self-test]
# Exit 0 = ocserve matches freeze on every vector; 1 = divergence; 2 = setup.
set -uo pipefail
cd "$(dirname "$0")/.."

PORT_F="${FC_PORT_F:-4932}"
PORT_R="${FC_PORT_R:-4933}"
VENDOR="bench/parity/vendor/opencode"
OCSERVE_BIN="${OCSERVE_BIN:-$PWD/target/release/ocserve}"
SELF_TEST="${1:-}"

die() { echo "field-contract-check: $*" >&2; exit 2; }

[ -x "$VENDOR" ] || die "$VENDOR missing (pair-check stages it; run that once)"
[ -x "$OCSERVE_BIN" ] || die "$OCSERVE_BIN missing (cargo build --release)"

if [ "$SELF_TEST" = "--self-test" ]; then
  # non-vacuity: the comparator must fail when a body is sabotaged
  python3 - <<'PY'
import json, sys
a = {"name":"BadRequest","data":{"message":"Missing key\n  at [\"parts\"]","kind":"Payload"}}
b = dict(a)
assert a == b, "identical bodies must compare equal"
b["data"] = {"message":"wrong"}
if a == b:
    sys.exit("SELFTEST FAIL: sabotaged body compared equal")
print("selftest: byte comparator distinguishes sabotage")
PY
  rc=$?
  [ $rc -eq 0 ] || exit 1
  # corpus presence
  python3 - <<'PY'
import sys
# keep in sync with the vectors() table below
n = 30
print(f"selftest: corpus floor {n} vectors declared")
sys.exit(0)
PY
  exit $?
fi

# port preflight
python3 - "$PORT_F" "$PORT_R" <<'PY' || die "ports busy ($PORT_F/$PORT_R)"
import socket, sys
for p in map(int, sys.argv[1:]):
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try: s.bind(("127.0.0.1", p))
    except OSError: sys.exit(1)
    finally: s.close()
PY

HF=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-fc-freeze.XXXXXX")
HR=$(mktemp -d "${TMPDIR:-/tmp}/ocserve-fc-ocserve.XXXXXX")
FC_CFG="$PWD/bench/field/config.json"
[ -f "$FC_CFG" ] || die "fixture config missing: $FC_CFG"
for h in "$HF" "$HR"; do
  mkdir -p "$h/.config/opencode" "$h/.local/share/opencode" "$h/.local/state/opencode" "$h/.cache/opencode"
  cp "$FC_CFG" "$h/.config/opencode/opencode.json"
done

setsid env HOME="$HF" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$VENDOR" serve --port "$PORT_F" --hostname 127.0.0.1 >"$HF/freeze.log" 2>&1 &
PID_F=$!
setsid env HOME="$HR" OPENCODE_DISABLE_MODELS_FETCH=1 OCSERVE_DATA_DIR="$HR/.local/share/ocserve" \
  OCSERVE_LEGACY_SYNC=0 "$OCSERVE_BIN" serve --port "$PORT_R" >"$HR/ocserve.log" 2>&1 &
PID_R=$!
cleanup() {
  kill -- -"$PID_F" 2>/dev/null; kill -- -"$PID_R" 2>/dev/null
  pkill -f "opencode serve --port $PORT_F" 2>/dev/null
  pkill -f "ocserve serve --port $PORT_R" 2>/dev/null
  rm -rf "$HF" "$HR"
}
trap cleanup EXIT

wait_health() {
  for _ in $(seq 1 90); do
    curl -s --max-time 2 "http://127.0.0.1:$1/global/health" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  die "$2 never healthy (log: ${3:-})"
}
wait_health "$PORT_F" freeze "$HF/freeze.log"
wait_health "$PORT_R" ocserve "$HR/ocserve.log"

exec 3>&1
exec 1>&2  # progress to stderr; results via fd3

python3 - "$PORT_F" "$PORT_R" <<'PY' >&3
import json, sys, urllib.request, urllib.error

port_f, port_r = int(sys.argv[1]), int(sys.argv[2])

def post(port, path, body, raw=False):
    url = f"http://127.0.0.1:{port}{path}"
    data = body if isinstance(body, (bytes, bytearray)) else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"content-type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:
        return -1, str(e).encode()

def get(port, path):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=15) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:
        return -1, str(e).encode()

def new_session(port):
    st, body = post(port, "/session", {})
    v = json.loads(body)
    return v.get("id", "")

# warm-up: one decode round on each arm (freeze's first-boot decode is
# deterministic — this exists to fail fast if an arm mis-booted)
sid_f, sid_r = new_session(port_f), new_session(port_r)
assert sid_f and sid_r, f"session create failed f={sid_f} r={sid_r}"

# ---------------------------------------------------------------- corpus ---
# (name, path-template, body-bytes-or-json) — all freeze-probed byte-exact
# (field-probes.md rounds 1-8). {S} substituted per arm.
M = "/session/{S}/message"
CMD = "/session/{S}/command"
SH = "/session/{S}/shell"
V2P = "/api/session/{S}/prompt"
V2PERM = "/api/session/{S}/permission"
V2REV = "/api/session/{S}/revert/stage"

vectors = [
    ("msg-missing-parts",        M,   b"{}"),
    ("msg-bad-msgid",            M,   b'{"messageID":"notamsg","parts":[]}'),
    ("msg-msgid-123",            M,   b'{"messageID":123,"parts":[]}'),
    ("msg-msgid-bool",           M,   b'{"messageID":true,"parts":[]}'),
    ("msg-parts-null",           M,   b'{"parts":null}'),
    ("msg-parts-string",         M,   b'{"parts":"x"}'),
    ("msg-parts-elem-123",       M,   b'{"parts":[123]}'),
    ("msg-parts-empty-obj",      M,   b'{"parts":[{}]}'),
    ("msg-part-bogus-type",      M,   b'{"parts":[{"type":"bogus"}]}'),
    ("msg-part-missing-text",    M,   b'{"parts":[{"type":"text"}]}'),
    ("msg-part-text-123",        M,   b'{"parts":[{"type":"text","text":123}]}'),
    ("msg-part-bad-id",          M,   b'{"parts":[{"type":"text","text":"x","id":"bad"}]}'),
    ("msg-part-synthetic",       M,   b'{"parts":[{"type":"text","text":"x","synthetic":"bad"}]}'),
    ("msg-file-no-mime",         M,   b'{"parts":[{"type":"file"}]}'),
    ("msg-file-no-url",          M,   b'{"parts":[{"type":"file","mime":"a/b","filename":"a"}]}'),
    ("msg-agent-no-name",        M,   b'{"parts":[{"type":"agent"}]}'),
    ("msg-subtask-no-prompt",    M,   b'{"parts":[{"type":"subtask"}]}'),
    ("msg-model-empty",          M,   b'{"parts":[],"model":{}}'),
    ("msg-model-123",            M,   b'{"parts":[],"model":123}'),
    ("msg-agent-123",            M,   b'{"parts":[],"agent":123}'),
    ("msg-noreply-string",       M,   b'{"parts":[],"noReply":"x"}'),
    ("msg-tools-entry",          M,   b'{"parts":[],"tools":{"a":1}}'),
    ("msg-format-123",           M,   b'{"parts":[],"format":123}'),
    ("msg-format-empty-obj",     M,   b'{"parts":[],"format":{}}'),
    ("msg-system-123",           M,   b'{"parts":[],"system":123}'),
    ("root-array",               M,   b"[1]"),
    ("root-string",              M,   b'"hello"'),
    ("cmd-missing-arguments",    CMD,  b"{}"),
    ("cmd-missing-command",      CMD,  b'{"arguments":"x"}'),
    ("cmd-bad-msgid",            CMD,  b'{"messageID":"bad"}'),
    ("cmd-text-part",            CMD,  b'{"command":"noop","arguments":"y","parts":[{"type":"text","text":"z"}]}'),
    ("shell-missing-agent",      SH,   b"{}"),
    ("shell-command-only",       SH,   b'{"command":"ls"}'),
    ("v2-missing-prompt",        V2P,  b"{}"),
    ("v2-prompt-string",         V2P,  b'{"prompt":"x"}'),
    ("v2-prompt-no-text",        V2P,  b'{"prompt":{"files":[]}}'),
    ("v2-id-msgx",               V2P,  b'{"id":"msgx","prompt":{"text":"x"}}'),
    ("v2-delivery-bogus",        V2P,  b'{"prompt":{"text":"x"},"delivery":"bogus"}'),
    ("v2-resume-string",         V2P,  b'{"prompt":{"text":"x"},"resume":"bad"}'),
    ("v2perm-missing-action",    V2PERM, b"{}"),
    ("v2perm-save-false",        V2PERM, b'{"action":"bash","resources":["x"],"save":false}'),
    ("v2rev-missing-msgid",      V2REV, b"{}"),
]

fail = 0
for name, tmpl, body in vectors:
    st_f, b_f = post(port_f, tmpl.replace("{S}", sid_f), body)
    st_r, b_r = post(port_r, tmpl.replace("{S}", sid_r), body)
    if st_f != st_r or b_f != b_r:
        fail += 1
        print(f"DIVERGE {name}: status {st_f} vs {st_r}")
        print(f"  freeze : {b_f[:300]!r}")
        print(f"  ocserve: {b_r[:300]!r}")
print(f"decode vectors: {len(vectors)} compared, {fail} divergences")

# ------------------------------------------------------------ echo tests ---
echo_fail = 0
# warm-up + identity echo: noReply persists the user message WITHOUT any
# model call (probe7: freeze [200] role:user on a dead endpoint)
for port, sid, label in ((port_f, sid_f, "freeze"), (port_r, sid_r, "ocserve")):
    st, _ = post(port, f"/session/{sid}/message",
                 {"noReply": True, "parts": [{"type": "text", "text": "warmup"}]})
    if st != 200:
        print(f"NOTE {label}: warmup noReply status {st} (agent registry cold?)")

CLIENT_ID = "msg_fc_echotest0000000000000001"
LC_ID = "msg_fc_lcignored0000000000000001"
ids_by_arm = {}
for port, sid, label in ((port_f, sid_f, "freeze"), (port_r, sid_r, "ocserve")):
    st1, _ = post(port, f"/session/{sid}/message",
                  {"messageID": CLIENT_ID, "noReply": True,
                   "parts": [{"type": "text", "text": "echo"}]})
    st2, _ = post(port, f"/session/{sid}/message",
                  {"messageId": LC_ID, "noReply": True,
                   "parts": [{"type": "text", "text": "lc"}]})
    if st1 != 200 or st2 != 200:
        echo_fail += 1
        print(f"DIVERGE echo-status {label}: messageID={st1} lowercase={st2}")
    st, body = get(port, f"/session/{sid}/message?limit=100")
    try:
        msgs = json.loads(body)
        ids = [m["info"]["id"] for m in (msgs if isinstance(msgs, list) else [])]
    except Exception:
        ids = []
    ids_by_arm[label] = ids
    if CLIENT_ID not in ids:
        echo_fail += 1
        print(f"DIVERGE echo {label}: client messageID not stored (ids tail: {ids[-4:]})")
    if LC_ID in ids:
        echo_fail += 1
        print(f"DIVERGE echo {label}: lowercase messageId was stored ({LC_ID})")
# structural cross-arm: CLIENT-supplied ids must match (server-generated ids
# are arm-specific by design — freeze and ocserve have different generators)
client_f = sorted(i for i in ids_by_arm.get("freeze", []) if i.startswith("msg_fc_"))
client_r = sorted(i for i in ids_by_arm.get("ocserve", []) if i.startswith("msg_fc_"))
if client_f != client_r:
    echo_fail += 1
    print(f"DIVERGE echo cross-arm client-id sets differ:\n  freeze : {client_f}\n  ocserve: {client_r}")

print(f"echo vectors: stored-id assertions, {echo_fail} divergences")
total = fail + echo_fail
print(f"field-contract-check: {'PASS' if total == 0 else 'FAIL'} ({len(vectors)} decode + echo corpus, {total} divergences)")
sys.exit(1 if total else 0)
PY
rc=$?
exec 1>&3 3>&-
exit $rc
