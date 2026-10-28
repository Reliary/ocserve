#!/usr/bin/env bash
# Gate receipts (see crates/ocserve-cli/src/gate_status.rs).
#
# Sourced by scripts/nightly.sh, scripts/drift-watch.sh and scripts/soak.sh.
# Each writes <data_dir>/gate-status/<gate>.json atomically so `ocserve doctor`
# can answer "did the verification program run, recently, and pass?" without
# parsing logs, journals, or relying on systemd.
#
# Why this exists: nightly.sh exited non-zero on 19 of 21 runs between
# 2026-10-04 and 2026-10-27 and nothing surfaced it. `doctor` is the instrument
# an operator actually runs, so the receipt is written where `doctor` looks.
#
# The data dir matches the server's (OCSERVE_DATA_DIR, else
# ~/.local/share/ocserve) so a fenced HOME in a harness does not leak receipts
# into the real install.

GATE_DIR="${OCSERVE_DATA_DIR:-${OCSERVE_HOME:-$HOME}/.local/share/ocserve}/gate-status"

# JSON string escape (backslash, quote, control chars). Labels are ours, but a
# failed-step name can come from a log line, so escape rather than trust.
_gate_json_str() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  s="${s//$'\n'/\\n}"
  s="${s//$'\r'/\\r}"
  s="${s//$'\t'/\\t}"
  printf '"%s"' "$s"
}

_gate_json_list() {
  local first=1 out="[" x
  for x in "$@"; do
    [ -n "$x" ] || continue
    [ "$first" -eq 1 ] || out+=","
    first=0
    out+="$(_gate_json_str "$x")"
  done
  printf '%s]' "$out"
}

# Atomic write: tmp in the same directory, then rename. A torn receipt is
# judged as a FAILURE by doctor (never as "never ran"), but it should not
# happen in the first place.
_gate_write() {
  local name="$1" body="$2"
  mkdir -p "$GATE_DIR" 2>/dev/null || return 0
  local tmp
  tmp="$(mktemp "$GATE_DIR/.$name.XXXXXX" 2>/dev/null)" || return 0
  printf '%s\n' "$body" >"$tmp" && mv -f "$tmp" "$GATE_DIR/$name.json" 2>/dev/null
  rm -f "$tmp" 2>/dev/null
  return 0
}

# gate_begin <gate> [kind]
# Mark a run as in progress. If the process is killed, doctor reports
# ABANDONED rather than silently keeping the previous green receipt.
gate_begin() {
  local name="$1" kind="${2:-run}" now
  now=$(date +%s)
  _gate_write "$name" "{\"gate\":$(_gate_json_str "$name"),\"kind\":$(_gate_json_str "$kind"),\"result\":\"running\",\"started_at\":$now,\"finished_at\":0,\"failed\":[],\"detail\":\"\",\"path\":\"\",\"interval\":0}"
}

# gate_finish <gate> <ok|failed> <detail> [failed-step ...]
gate_finish() {
  local name="$1" result="$2" detail="$3"; shift 3
  local now start span
  now=$(date +%s)
  # Preserve the real start time so "how long did it take" stays answerable.
  start="$now"
  local prev="$GATE_DIR/$name.json"
  if [ -r "$prev" ]; then
    local s
    s=$(python3 -c 'import json,sys
try:
    print(int(json.load(open(sys.argv[1])).get("started_at") or 0))
except Exception:
    print(0)' "$prev" 2>/dev/null || echo 0)
    # NOT `[ -n "$s" ] && [ "$s" -gt 0 ] && start="$s"`: a false && chain is a
    # failing statement, and under `set -e` (drift-watch, soak) that aborts this
    # function mid-write and leaves the receipt saying `running`. It did exactly
    # that on the first live run.
    if [ -n "$s" ] && [ "$s" -gt 0 ] 2>/dev/null; then
      start="$s"
    fi
  fi
  span=$((now - start))
  _gate_write "$name" "{\"gate\":$(_gate_json_str "$name"),\"kind\":\"run\",\"result\":$(_gate_json_str "$result"),\"started_at\":$start,\"finished_at\":$now,\"failed\":$(_gate_json_list "$@"),\"detail\":$(_gate_json_str "$detail in ${span}s"),\"path\":\"\",\"interval\":0}"
}

# gate_sampler <gate> <output-path> <interval-secs>
# A sampler never "finishes": doctor judges it by the freshness of its output
# and the health code on the newest row.
gate_sampler() {
  local name="$1" path="$2" interval="$3" now
  now=$(date +%s)
  _gate_write "$name" "{\"gate\":$(_gate_json_str "$name"),\"kind\":\"sampler\",\"result\":\"running\",\"started_at\":$now,\"finished_at\":0,\"failed\":[],\"detail\":\"\",\"path\":$(_gate_json_str "$path"),\"interval\":$interval}"
}
