#!/usr/bin/env bash
# Static guardrails (AGENTS §2.3/§2.5) — one command that fails the build on
# the bug CLASSES we have actually shipped once each:
#   1. swallowed storage writes        (the finalize-SQL silent failure)
#   2. literal backslash inside SQL    (the "\\" continuation syntax error)
#   3. whole-session Value materialization in HTTP (the OOM class) — the
#      messages route must stream via for_each_message_json
#   4. msg_part INSERT/UPDATE outside ocserve-store — every part write must
#      carry its part_search projection companion (W1 FTS invariant)
#   5. upstream drift watch broken (version compare / replay-output parse /
#      set-difference / report writer) — the nightly watch would go silently
#      wrong (PLAN §6/§8, K-DRIFT)
#   6. non-ocserve systemd units mutated from repo scripts (drop-ins/restarts —
#      the 2026-10-05 incident class; only ocserve* units are mutable, see
#      TESTING §1.6)
#   9. unit template keeps OOMPolicy=continue (child OOM must not bounce the
#      service — the 2026-10-06 stop-policy bounce; A3's completion)
#  10. A3 child wrapper stays byte-identical in ocserve-mcp + ocserve-plugin
#      (two copies, no shared dep — drift would silently drop oom_score_adj
#      or the L1 kids move from one spawn path)
#  11. installer/uninstaller never delete shared opencode state (config, auth,
#      legacy db, model cache, packages — only ocserve-derived *.normalized.mjs
#      artifacts are allowed near opencode paths)
#  12. pair-check allowlist entries must cite a named D-PAIR row (the only
#      legitimate way a freeze↔ocserve divergence passes the pair gate)
#  13. load-test: a line that INVOKES k6 may never name a LIVE service port
#      (4912/4901) — k6 targets fixture arms only; health asserts on the
#      live ports must not sit on k6 lines
#  14. every URL in the frozen v1 SDK surface is bound in the router or has
#      an exact-URL PLAN §17 citation — unbound routes fall through to the
#      HTML proxy and crash JSON-expecting clients (the 2026-10-08 web-UI
#      settings crash: /pty/shells → HTML → e.shells.reduce TypeError)
#  16. every operation in the frozen OpenAPI contract is bound or exact-cited
#      (spec-driven, whole-token citation match)
#  17. the embedded pinned web UI calls only routes in the frozen spec
#  18. permission oracle vectors exist, are well-formed, and carry the fixture
#      config — the behavioural differential (scripts/permission-check.sh) can
#      never silently no-op (route-binding/shape guards cannot see authz
#      semantics; the 2026-10-09 hardcoded-allow v2 bug)
#  19. the event-payload validator (bench/events/event-validate.py) is
#      non-vacuous — its selftest plants malformed payloads (partial
#      session.updated, part.updated missing `time`, bad permission id) and
#      must flag them. The live-DB scan runs in nightly (historical pre-fix
#      events would otherwise red the commit gate for an already-fixed bug).
#   7. exact assertions on process-global counters inside src/ (in-crate unit
#      tests run in one parallel process and race — the reader_opens flake of
#      2026-10-05; such tests belong in tests/ where they own the process)
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0

echo "== guard: swallowed storage writes =="
if out=$(grep -rn --include='*.rs' -E \
    'let _ = [a-z_]+\.write\(|let _ = .*append_event\(|let _ = .*finalize_session_prompt\(' \
    crates/*/src 2>/dev/null); then
  echo "$out"; echo "FAIL: storage write result discarded (AGENTS §2.5)"; fail=1
else
  echo "ok"
fi

echo "== guard: literal backslash in SQL string =="
# file contains sql: "...\\..." (escaped backslash → literal \ reaches SQLite)
if out=$(grep -rn --include='*.rs' -E 'sql: "[^"]*\\\\|\\\\$' crates/*/src 2>/dev/null); then
  echo "$out"; echo "FAIL: SQL string contains a literal backslash (syntax error class)"; fail=1
else
  echo "ok"
fi

echo "== guard: HTTP must stream messages (no load_messages) =="
# Named exceptions: lines carrying the marker 'allow:load_messages' are
# documented full-reads (post_summarize compaction — upstream does the same)
if out=$(grep -rn --include='*.rs' 'load_messages(' crates/ocserve-http/src 2>/dev/null | grep -v 'allow:load_messages'); then
  echo "$out"; echo "FAIL: load_messages in HTTP materializes whole sessions (OOM class) — use for_each_message_json"; fail=1
else
  echo "ok"
fi

echo "== guard: msg_part writes only in ocserve-store =="
if out=$(grep -rn --include='*.rs' -E 'INSERT (OR REPLACE |OR IGNORE )?INTO msg_part|UPDATE msg_part SET' crates/*/src 2>/dev/null | grep -v 'crates/ocserve-store/src/'); then
  echo "$out"; echo "FAIL: msg_part INSERT/UPDATE outside ocserve-store skips the search projection (use part_row_ops / insert_message / update_part)"; fail=1
else
  echo "ok"
fi

echo "== guard: upstream drift-watch selftest =="
if ./scripts/drift-watch.sh --selftest >/dev/null 2>&1; then
  echo "ok"
else
  echo "FAIL: drift-watch --selftest (upstream drift watcher broken — PLAN §6/§8 watch)"; fail=1
fi

echo "== guard: non-ocserve systemd units are read-only in scripts =="
# Named exception: lines carrying 'ocserve' (ocserve.service, ocserve-*.service,
# ocserve-tailscale-forward) — ocserve's own deploy/restart is legitimate.
# Read-only inspect (status/cat/show/is-active) is intentionally not matched.
if out=$(grep -rnE 'systemd/user/[A-Za-z0-9@._-]*service\.d|systemctl --user (edit|restart|stop|start|mask|kill)\b' \
    scripts bench --include='*.sh' --include='*.py' 2>/dev/null \
    | grep -v 'check-guards\.sh' | grep -v ocserve); then
  echo "$out"; echo "FAIL: repo script mutates a non-ocserve systemd unit (2026-10-05 incident class) — experiments belong in disposable containers (TESTING §1.6)"; fail=1
else
  echo "ok"
fi

echo "== guard: global-counter exact assertions stay out of src/ =="
# In-crate unit tests run in one parallel process; exact deltas on the
# process-global reader-opens counter raced (flaked 2026-10-05). Such
# assertions belong in tests/ where they own the process (TESTING §1.6).
if out=$(grep -rnE 'assert(_eq|_ne)?!\([^)]*reader_opens' crates/*/src 2>/dev/null); then
  echo "$out"; echo "FAIL: exact reader_opens assertion in src/ races parallel tests — move it to crates/*/tests/ (process-isolated)"; fail=1
else
  echo "ok"
fi

echo "== guard: normalized plugin outputs written atomically =="
# D1: a torn .normalized.mjs (partial fs::write after a crash) loads as a
# syntax error or half-plugin at boot. Every FINAL normalized path write
# must go through atomic_write (tmp+rename); crash matrix: any prefix of
# write→rename→hash leaves hash≠content → rebuild (D1-PLAN).
if out=$(grep -rnE 'fs::write\([^)]*\.normalized' crates/*/src 2>/dev/null); then
  echo "$out"; echo "FAIL: direct fs::write to a final normalized path — use atomic_write (tmp+rename), D1-PLAN"; fail=1
else
  echo "ok"
fi

echo "== guard: unit template keeps OOMPolicy=continue (A3 completion) =="
# 2026-10-06: default OOMPolicy=stop bounced the service after the kernel
# correctly killed only the sidecar child — the "respawnable child" property
# is false without this line (TESTING §1.6, SRE §5).
if grep -q 'OOMPolicy=continue' deploy/ocserve.service 2>/dev/null; then
  echo "ok"
else
  echo "FAIL: deploy/ocserve.service lost OOMPolicy=continue — child OOM will bounce the whole unit again (A3 class)"; fail=1
fi

echo "== guard: A3/L1 child wrapper identical in mcp + plugin =="
w1=$(grep -h 'oom_score_adj' crates/ocserve-mcp/src/lib.rs | grep 'exec' || true)
w2=$(grep -h 'oom_score_adj' crates/ocserve-plugin/src/lib.rs | grep 'exec' || true)
if [ -z "$w1" ] || [ -z "$w2" ] || [ "$w1" != "$w2" ]; then
  echo "mcp:    $w1"; echo "plugin: $w2"
  echo "FAIL: OOM_CHILD_WRAPPER copies drifted (rule 10 — both spawn paths must carry adj+kids move)"; fail=1
else
  echo "ok"
fi

echo "== guard: install/uninstall never delete shared opencode state =="
# rm targets may sit next to "opencode" ONLY for the derived normalized
# artifacts (basename-gated in remove_derived); anything else (config/auth/
# models/packages) is the 2026-10-05 blast-radius class.
if out=$(grep -nE 'rm .*opencode|rm .*auth\.json|rm .*models\.json' \
    scripts/install.sh scripts/uninstall.sh 2>/dev/null | grep -v normalized); then
  echo "$out"; echo "FAIL: installer/uninstaller would delete shared opencode state (rule 11)"; fail=1
else
  echo "ok"
fi

echo "== guard: pair allowlist cites D-PAIR rows =="
if [ -f bench/pair/allow.txt ]; then
  if out=$(grep -v '^#' bench/pair/allow.txt | grep -vE '^\s*$' | grep -vE '# *D-PAIR-[0-9]'); then
    echo "$out"; echo "FAIL: pair allow entry without a D-PAIR citation (rule 12)"; fail=1
  else
    echo "ok"
  fi
else
  echo "ok (no allowlist)"
fi

echo "== guard: k6 never targets live services =="
if out=$(grep -nE 'K6_IMG|k6 run|k6_flags' scripts/load-test.sh | grep -E '4912|4901'); then
  echo "$out"; echo "FAIL: k6-invoking line names a live service port (rule 13)"; fail=1
else
  echo "ok"
fi

echo "== guard: full API coverage (rule 16, spec-driven) =="
# Supersedes rules 14 + 15 + the web-bundle extractor with ONE generated check
# over the frozen server's own OpenAPI contract (bench/openapi/1.18.31.json,
# served at /doc). Every operation must be bound in the router or carry an
# exact `METHOD /path` citation in PLAN.md. Citations match on whole-token
# boundaries (a substring test vacuously cited `POST /mcp` as a prefix of
# `POST /mcp/{name}/connect`). Cannot drift the way three hand-maintained
# route lists did (the /pty/shells and /session/{id}/diff gaps).
if [ -f bench/openapi/1.18.31.json ]; then
  # NB: check-coverage.py exits 1 when gaps exist; with `set -o pipefail`
  # a `if $(... | grep)` construct would inherit that nonzero and report ok
  # vacuously. Capture full output and test for the GAP marker explicitly.
  cov_out=$(python3 bench/openapi/check-coverage.py bench/openapi/1.18.31.json crates/ocserve-http/src/lib.rs PLAN.md 2>&1) || true
  gaps=$(printf '%s\n' "$cov_out" | grep '^  GAP' || true)
  if [ -n "$gaps" ]; then
    echo "$gaps"; echo "FAIL: spec operation unbound and uncited (rule 16)"; fail=1
  else
    echo "ok"
  fi
else
  echo "ok (no vendored spec)"
fi

echo "== guard: embedded UI version-skew (rule 17) =="
# The pinned web UI must call only routes present in the frozen spec (same
# tag); a mismatch means the extraction captured the wrong build. Replaces the
# old Cloudflare-latest webui-routes.txt. Planted control: sabotaged spec → red.
if [ -f bench/webui/app/1.18.31.pack.zst ] && [ -f bench/openapi/1.18.31.json ]; then
  if out=$(python3 bench/webui/check-app-skew.py bench/webui/app/1.18.31.pack.zst bench/openapi/1.18.31.json 2>&1); then
    echo "ok"
  else
    echo "$out"; echo "FAIL: embedded UI calls a route absent from the frozen spec (rule 17)"; fail=1
  fi
else
  echo "ok (no embedded app pack)"
fi

echo "== guard: permission oracle vectors present + self-consistent (rule 18) =="
# Behavioural layer: route-binding (16) and shape (P4) cannot see authorization
# SEMANTICS. The differential itself (freeze↔ocserve) runs in
# scripts/permission-check.sh (boots both arms); this static check guarantees
# the committed vectors + fixture exist and the vectors are well-formed, so the
# differential can never silently no-op. Planted control: malformed vectors → red.
if [ -f bench/permission/1.18.31.vectors.json ]; then
  if out=$(python3 - <<'PY' 2>&1
import json, sys
try:
    d = json.load(open("bench/permission/1.18.31.vectors.json"))
except Exception as e:
    sys.exit(f"vectors unreadable: {e}")
v = d.get("vectors")
if not isinstance(v, list) or len(v) < 10:
    sys.exit("vectors missing or too few")
for row in v:
    if set(row) != {"agent", "action", "resources", "effect"}:
        sys.exit(f"malformed vector: {row}")
    if row["effect"] not in ("allow", "ask", "deny"):
        sys.exit(f"bad effect: {row}")
import os
if not os.path.isfile("bench/permission/config/opencode.json"):
    sys.exit("fixture config missing")
print(f"ok ({len(v)} vectors)")
PY
); then
    echo "$out"
  else
    echo "$out"; echo "FAIL: permission vectors malformed/missing (rule 18)"; fail=1
  fi
else
  echo "ok (no permission vectors)"
fi

echo "== guard: event-payload validator is non-vacuous (rule 19) =="
# Event-surface layer: rule 16 (binding) and P4 (JSON 200 bodies) cannot see
# the SSE/event stream, where the 2026-10-09 TUI crash lived (partial
# session.updated → r.title.length TypeError). The validator selftest plants
# malformed payloads (partial session.updated, part.updated missing `time`,
# bad permission id) and asserts they are flagged — so the gate is deterministic
# and can never pass vacuously. The live-DB scan (historical + new events) runs
# in nightly, not here: pre-fix events live in the log and would red the commit
# gate for a bug that is already fixed at the source.
if python3 bench/events/event-validate-selftest.py >/dev/null 2>&1; then
  echo "ok (validator selftest: malformed payloads flagged)"
else
  echo "FAIL: event-validate selftest (validator is vacuous/broken)"; fail=1
fi

echo "== guard: handler request keys are contract-valid (rule 20) =="
# Field-contract layer: the 2026-10-09 double-prompt bug was a request-key
# divergence (`messageId` read vs `messageID` wire — FIELD-CONTRACT.md).
# The prompt family is typed (wire.rs, compile-checked against the spec by
# wire_keys_match_frozen_spec); this scan extends the net to every remaining
# axum Json(...) handler: request keys must exist in the frozen contract.
# Selftest plants a poisoned handler read — if that is not flagged the
# scanner is vacuous and the gate fails.
if out=$(python3 bench/openapi/check-request-keys.py --selftest 2>&1) \
  && scan=$(python3 bench/openapi/check-request-keys.py \
      bench/openapi/1.18.31.json crates/ocserve-http/src 2>&1); then
  echo "ok (selftest + scan: $(echo "$scan" | tail -1))"
else
  echo "$out"; echo "$scan"
  echo "FAIL: handler request keys violate the frozen contract (rule 20)"; fail=1
fi

echo "== guard: no empty partID in live event emitters (rule 21) =="
# Streaming-identity class: the 2026-10-11 "web UI shows nothing until reload"
# bug emitted `message.part.delta` with `partID:""` — the client keys deltas by
# a real `^prt` part id and drops them otherwise (STREAM-DELTA.md). The typed
# boundary can't see bus-only events, and rule 19 only scans the PERSISTED log
# (deltas are `emit_live`, never stored). This static rule bans the literal
# empty partID assignment anywhere in the runtime crates; the behavioral
# half is tests/stream_delta.rs (real stub-provider turn + a planted pre-fix
# negative control). Planted-violation self-check first so the grep can never
# be vacuous.
plant_dir="crates/ocserve-core/src"
plant_file="$plant_dir/__rule21_plant.rs"
printf '%s\n' 'fn _plant() { let _ = json!({"partID": "", "field": "text"}); }' > "$plant_file"
if grep -RnE '"partID"\s*:\s*""' "$plant_dir" >/dev/null 2>&1; then
  echo "ok (planted empty partID detected)"
else
  echo "FAIL: empty-partID planted violation not detected (rule 21 vacuous)"; fail=1
fi
rm -f "$plant_file"
if grep -RnE '"partID"\s*:\s*""' crates/*/src >/dev/null 2>&1; then
  grep -RnE '"partID"\s*:\s*""' crates/*/src
  echo "FAIL: runtime emitter assigns an empty partID (rule 21)"; fail=1
else
  echo "ok (no empty partID in runtime emitters)"
fi

echo "== guard: three event routes stay bound to distinct envelope handlers (rule 22) =="
# SSE-envelope class: the 2026-10-11 bug hunt found /event and /api/event both
# wired to the /global/event handler, so all three served the same
# `{payload}` envelope — but freeze /event is bare `{id,type,properties}` and
# /api/event is v2 `{id,type,data}`. The static rule requires each route to map
# to its own handler; the behavioral half is tests/wire_hardening.rs +
# golden.rs. Selftest: plant an aliased route and confirm it is flagged.
plant="crates/ocserve-http/src/__rule22_plant.rs"
printf '%s\n' '.route("/api/event", get(global_event))' > "$plant"
if grep -RhoE '\.route\("/api/event", get\([a-z0-9_]+\)\)' crates/ocserve-http/src >/dev/null 2>&1; then
  echo "ok (planted aliased event route detected)"
else
  echo "FAIL: aliased event route planted violation not detected (rule 22 vacuous)"; fail=1
fi
rm -f "$plant"
route_event=$(grep -RhoE '\.route\("/event", get\([a-z0-9_]+\)\)' crates/ocserve-http/src | head -1)
route_api=$(grep -RhoE '\.route\("/api/event", get\([a-z0-9_]+\)\)' crates/ocserve-http/src | head -1)
route_global=$(grep -RhoE '\.route\("/global/event", get\([a-z0-9_]+\)\)' crates/ocserve-http/src | head -1)
h_event=$(echo "$route_event" | sed -E 's/.*get\(([a-z0-9_]+)\).*/\1/')
h_api=$(echo "$route_api" | sed -E 's/.*get\(([a-z0-9_]+)\).*/\1/')
h_global=$(echo "$route_global" | sed -E 's/.*get\(([a-z0-9_]+)\).*/\1/')
if [ -z "$h_event" ] || [ -z "$h_api" ] || [ -z "$h_global" ]; then
  echo "FAIL: an event route is missing (rule 22): event='$h_event' api='$h_api' global='$h_global'"; fail=1
elif [ "$h_event" = "$h_api" ] || [ "$h_event" = "$h_global" ] || [ "$h_api" = "$h_global" ]; then
  echo "FAIL: event routes share a handler ($h_event / $h_api / $h_global) — envelopes will diverge (rule 22)"; fail=1
else
  echo "ok (event=$h_event api=$h_api global=$h_global)"
fi

exit $fail
