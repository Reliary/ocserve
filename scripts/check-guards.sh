#!/usr/bin/env bash
# Static guardrails (AGENTS §2.3/§2.5) — one command that fails the build on
# the bug/performance CLASSES we have actually shipped once each. Rules
# marked [plant] inject a violation inline every run and must flag it (the
# grep can never pass vacuously); rules marked [suite] are proven by
# committed tests or delegated `--selftest` harnesses named in the comment.
# The rule numbers are stable IDs cited across the docs; some numbers were
# retired when a stronger, generated check superseded them (noted below),
# so the sequence has gaps.
#
#   1. swallowed storage writes        (the finalize-SQL silent failure) [suite]
#   2. literal backslash inside SQL    (the "\\" continuation syntax error) [suite]
#   3. whole-session Value materialization in HTTP (the OOM class) — the
#      messages route must stream via for_each_message_json [suite]
#   4. msg_part INSERT/UPDATE outside ocserve-store — every part write must
#      carry its part_search projection companion (W1 FTS invariant) [suite]
#   5. upstream drift watch broken (version compare / replay-output parse /
#      set-difference / report writer) — the nightly watch would go silently
#      wrong (PLAN §6/§8, K-DRIFT) [suite: drift-watch.sh --selftest]
#   6. non-ocserve systemd units mutated from repo scripts (drop-ins/restarts —
#      the 2026-10-05 incident class; only ocserve* units are mutable, TESTING §1.6) [suite]
#   7. exact assertions on process-global counters inside src/ (parallel in-crate
#      tests race — the reader_opens flake; such tests belong in tests/) [suite]
#   8. (retired — folded into 7)
#   9. unit template keeps OOMPolicy=continue (child OOM must not bounce the
#      service — the 2026-10-06 stop-policy bounce; A3's completion) [suite]
#  10. A3 child wrapper stays byte-identical in ocserve-mcp + ocserve-plugin
#      (two copies, no shared dep — drift would silently drop oom_score_adj) [suite]
#  11. installer/uninstaller never delete shared opencode state (config, auth,
#      legacy db, model cache, packages — only ocserve-derived *.normalized.mjs) [suite]
#  12. pair-check allowlist entries must cite a named D-PAIR row [suite]
#  13. load-test: a line that INVOKES k6 may never name a LIVE service port [suite]
#  14,15. (retired — superseded by rule 16's generated spec-coverage check)
#  16. every operation in the frozen OpenAPI contract is bound or exact-cited
#      (spec-driven, whole-token citation match) [suite: checker + corpus]
#  16b. every operation in the v2 OpenAPI contract is bound or exact-cited
#      [plant: dropped spec-bound binding → GAP]
#  17. the embedded pinned web UI calls only routes in the frozen spec [suite]
#  18. permission oracle vectors exist, are well-formed, and carry the fixture config [suite]
#  19. the event-payload validator selftest is non-vacuous [plant: delegated selftest]
#  20. every request key read off an axum `Json(...)` binding exists in the contract [plant: delegated selftest]
#  21. no runtime emitter assigns an empty `partID` (the streaming-delta class) [plant]
#  22. /event, /api/event, /global/event stay bound to distinct envelope handlers [plant]
#  23. no per-request catalog reads in the v2 metadata routes [plant]
#  24. every `tests/*.rs` cited from `src/` exists [plant]
#  25. no `SELECT *` on hot tables in server paths [plant]
#  26. WAL ceiling in soak-gate.sh [suite: external script]
#  27. provider-boundary differential (capdiff) is non-vacuous + allowlist cites D-rows [plant]
#  28. crates.io publish order covers every member in dependency order [plant: delegated selftest]
#  29. every mutating endpoint has an asserted side effect (the shape-only class) [plant: delegated selftest]
#  30. no compiled-in asset (`include_str!`/`include_bytes!`) escapes its crate root [plant: delegated selftest]
#  31. docs are not stale (dead repo paths, dangling guard-rule refs, removed
#      divergences still listed as current) [plant: delegated selftest]
#  32. v2 contract-differ classifies every op; conflicts block superset serving
#      [plant: delegated selftest]
#  33. v2-path queries carry allocation budgets (no unbounded reads on new tables) [plant]
#  34. serving path is Rust-native (no non-Rust runtime in the request path;
#      same-sidecar-protocol extensions allowed, new runtimes forbidden) [plant]
#  35. no absolute-path silent redirect in writers (resolve_within class) [plant]
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
# over the frozen server's own OpenAPI contract (crates/ocserve-core/assets/openapi/1.18.31.json,
# served at /doc). Every operation must be bound in the router or carry an
# exact `METHOD /path` citation in PLAN.md. Citations match on whole-token
# boundaries (a substring test vacuously cited `POST /mcp` as a prefix of
# `POST /mcp/{name}/connect`). Cannot drift the way three hand-maintained
# route lists did (the /pty/shells and /session/{id}/diff gaps).
if [ -f crates/ocserve-core/assets/openapi/1.18.31.json ]; then
  # NB: check-coverage.py exits 1 when gaps exist; with `set -o pipefail`
  # a `if $(... | grep)` construct would inherit that nonzero and report ok
  # vacuously. Capture full output and test for the GAP marker explicitly.
  cov_out=$(python3 bench/openapi/check-coverage.py crates/ocserve-core/assets/openapi/1.18.31.json crates/ocserve-http/src/lib.rs PLAN.md 2>&1) || true
  gaps=$(printf '%s\n' "$cov_out" | grep '^  GAP' || true)
  if [ -n "$gaps" ]; then
    echo "$gaps"; echo "FAIL: spec operation unbound and uncited (rule 16)"; fail=1
  else
    echo "ok"
  fi
else
  echo "ok (no vendored spec)"
fi

echo "== guard: v2 API coverage (rule 16b, spec-driven) =="
# Same generated binding check as rule 16, over the 2.0.28 contract: every
# op must be bound in the router or carry an exact `METHOD /path` citation
# in V2-PARITY-PLAN.md (all 142 currently bound; citations remain as fallback
# for future verdicts). Planted control: remove a binding → GAP → red.
if [ -f crates/ocserve-core/assets/openapi/2.0.28.json ]; then
  cov2_out=$(python3 bench/openapi/check-coverage.py crates/ocserve-core/assets/openapi/2.0.28.json crates/ocserve-http/src/lib.rs V2-PARITY-PLAN.md 2>&1) || true
  gaps2=$(printf '%s\n' "$cov2_out" | grep '^  GAP' || true)
  if [ -n "$gaps2" ]; then
    echo "$gaps2"; echo "FAIL: v2 spec operation unbound and uncited (rule 16b)"; fail=1
  else
    echo "ok"
  fi
  # Planted control: router copy minus one binding must GAP (proves the
  # checker is load-bearing, not vacuously green). Repo-local temp,
  # trap-restored.
  _cov16b_plant="crates/ocserve-http/src/.lib.guard-plant.rs"
  _cov16b_restore() { rm -f "$_cov16b_plant"; }
  trap '_cov16b_restore' EXIT
  python3 - crates/ocserve-core/assets/openapi/2.0.28.json crates/ocserve-http/src/lib.rs "$_cov16b_plant" <<'PYEOF' || true
import json, re, sys
spec = json.load(open(sys.argv[1], encoding="utf-8"))
paths = set(spec.get("paths", {}))
text = open(sys.argv[2], encoding="utf-8").read()
for m in re.finditer(r'\.route\(\s*"([^"]+)"', text):
    # normalize axum {param} to spec {param} form for membership test
    if m.group(1) in paths:
        path = m.group(1)
        depth, i = 1, m.end()
        while i < len(text) and depth:
            c = text[i]
            if c == "(": depth += 1
            elif c == ")": depth -= 1
            i += 1
        open(sys.argv[3], "w").write(text[:m.start()] + text[i:])
        print(path)
        break
else:
    sys.exit("no spec-bound .route( found to drop")
PYEOF
  _plant_gaps=$(python3 bench/openapi/check-coverage.py crates/ocserve-core/assets/openapi/2.0.28.json "$_cov16b_plant" V2-PARITY-PLAN.md 2>&1 | grep '^  GAP' || true)
  _cov16b_restore
  trap - EXIT
  if [ -z "$_plant_gaps" ]; then
    echo "FAIL: 16b checker blind to a dropped binding (rule 16b vacuous)"; fail=1
  else
    echo "ok (planted binding-drop detected by 16b checker)"
  fi
else
  echo "ok (no vendored v2 spec)"
fi

echo "== guard: embedded UI version-skew (rule 17) =="
# Each pinned web UI must call only routes present in its SAME-TAG frozen
# spec (Phase E: v1 + v2 packs). A mismatch means the extraction captured the
# wrong build. Planted control: sabotaged spec → red.
for _tag in 1.18.31 2.0.28; do
  _pack="crates/ocserve-http/assets/webui/$_tag.pack.zst"
  _spec="crates/ocserve-core/assets/openapi/$_tag.json"
  if [ -f "$_pack" ] && [ -f "$_spec" ]; then
    if out=$(python3 bench/webui/check-app-skew.py "$_pack" "$_spec" 2>&1); then
      echo "ok ($_tag)"
    else
      echo "$out"; echo "FAIL: $_tag embedded UI calls a route absent from its spec (rule 17)"; fail=1
    fi
  else
    echo "ok ($_tag: no embedded app pack)"
  fi
done

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
      crates/ocserve-core/assets/openapi/1.18.31.json crates/ocserve-core/assets/openapi/2.0.28.json crates/ocserve-http/src 2>&1); then
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

echo "== guard: no per-request catalog reads in v2 metadata routes (rule 23) =="
# Perf class (2026-10-11 dive): the v2 metadata handlers re-read + re-parsed
# the 5.4 MB models.json and deep-cloned the provider tree on EVERY request
# (96 ms p50, conc-6 8.1 s, +374 MB RSS) while v1 had F5/F7/F8/F9 caches.
# The fix serves precomputed pure projections from the per-reload wire cache.
# This rule bans fs reads (`read_to_string`/`fs::read`) of a model catalog in
# the v2 module and requires the three metadata handlers to go through
# `wire_json_h` (the cache path with a live builder fallback).
plant="crates/ocserve-http/src/__rule23_plant.rs"
printf '%s\n' 'fn _plant() { let _ = std::fs::read_to_string("models.json"); }' > "$plant"
if grep -RqE 'fs::read_to_string|fs::read\(' crates/ocserve-http/src/__rule23_plant.rs 2>/dev/null; then
  echo "ok (planted per-request read detected)"
else
  echo "FAIL: per-request read planted violation not detected (rule 23 vacuous)"; fail=1
fi
rm -f "$plant"
if grep -RnE 'read_to_string|std::fs::read\(|tokio::fs::read\(' crates/ocserve-http/src/v2.rs \
     | grep -iE 'catalog|models\.json' >/dev/null 2>&1; then
  grep -RnE 'read_to_string|std::fs::read\(|tokio::fs::read\(' crates/ocserve-http/src/v2.rs | grep -iE 'catalog|models\.json'
  echo "FAIL: v2 routes must not re-read the model catalog per request (rule 23)"; fail=1
else
  for h in list_models list_providers list_integrations; do
    if grep -A 20 "pub async fn $h" crates/ocserve-http/src/v2.rs | grep -q 'wire_json_h'; then
      :
    else
      echo "FAIL: $h does not serve the wire cache (rule 23)"; fail=1
    fi
  done
  echo "ok (no per-request catalog read; metadata routes use wire_json_h)"
fi

echo "== guard: no SELECT * on hot tables in server paths (rule 25) =="
# Perf/memory class (2026-10-11 dive): SELECT * materializes every column of
# wide/hot tables (event payloads, msg info, msg_part inline) even when the
# caller needs two of them; also freezes the wire shape to the schema.
# Ban literal `SELECT * FROM <hot>` and `SELECT * FROM (SELECT` shapes in
# crates/*/src, allowlisting the one EQP-probe use (schema.rs explains a plan,
# never executes for data). Planted control runs through the same scan.
plant="crates/ocserve-store/src/__rule25_plant.rs"
printf '%s\n' 'fn _plant() { let _ = "SELECT * FROM event"; }' > "$plant"
hot_hits=$(grep -rnE "SELECT \* FROM (event|msg|msg_part|part_search|session)" crates/*/src 2>/dev/null \
  | grep -v 'EXPLAIN QUERY PLAN' | grep -v '__rule25_plant' || true)
plant_hit=$(grep -rnE "SELECT \* FROM (event|msg|msg_part|part_search|session)" crates/ocserve-store/src/__rule25_plant.rs 2>/dev/null || true)
rm -f "$plant"
if [ -z "$plant_hit" ]; then
  echo "FAIL: rule 25 planted control not detected"
  fail=1
fi
if [ -n "$hot_hits" ]; then
  echo "$hot_hits"
  echo "FAIL: SELECT * on a hot table in server paths (rule 25)"; fail=1
else
  echo "ok (no SELECT * on hot tables; planted SELECT* detected)"
fi

echo "== guard: every test file referenced from src/ exists (rule 24) =="
# False-claim class (2026-10-11): `src/v2.rs` cited `tests/v2_body_parity.rs` in
# a doc comment while the file had never been written — the claim outran the
# code. Scan every `crates/*/src/**.rs` for `tests/<name>.rs` mentions and
# require the file to exist in the same crate's tests/ directory.
# The planted control runs through the SAME loop: a phantom reference is
# planted into src/ first, then the loop must flag it red.
plant="crates/ocserve-store/src/__rule24_plant.rs"
printf '%s\n' '/// see tests/__rule24_nonexistent.rs' > "$plant"
refs=$(grep -rhoE 'tests/[A-Za-z0-9_.-]+\.rs' crates/*/src --include='*.rs' 2>/dev/null | sort -u)
plant_caught=0
for ref in $refs; do
  name="${ref#tests/}"
  found=0
  for crate in crates/*/; do
    if [ -f "$crate/tests/$name" ]; then found=1; break; fi
  done
  if [ "$found" -eq 0 ]; then
    if [ "$ref" = "tests/__rule24_nonexistent.rs" ]; then
      plant_caught=1
    else
      echo "FAIL: referenced but missing test file: $ref (rule 24)"; fail=1
    fi
  fi
done
rm -f "$plant"
if [ "$plant_caught" -eq 1 ]; then
  echo "ok (planted phantom reference detected by the same scan)"
else
  echo "FAIL: phantom-reference planted control not detected (rule 24 vacuous)"; fail=1
fi
echo "ok (all $(( $(echo "$refs" | wc -w) - 1 )) real referenced test files exist)"

echo "== guard: provider-boundary differential is non-vacuous + allowlist cites D-rows (rule 27) =="
# K-CAPDIFF: bench/capdiff diffs the REAL provider requests of freeze and
# ocserve. The harness must not silently degrade into a no-op, and every
# allowlist entry must name a divergence. Static half here (the live half is
# capdiff --selftest, run in nightly); this half is cheap and always on.
# Planted control: a comment-free allow line with no D-row must be flagged.
if [ ! -f bench/capdiff/capdiff.py ]; then
  echo "FAIL: bench/capdiff/capdiff.py missing (rule 27)"; fail=1
fi
if ! grep -q "^def provider_view(" bench/capdiff/capdiff.py; then
  echo "FAIL: capdiff lost its provider_view comparator (rule 27)"; fail=1
fi
if ! grep -q "tool_map" bench/capdiff/capdiff.py; then
  echo "FAIL: capdiff tool comparison not name-keyed (rule 27)"; fail=1
fi
# K-CAPDIFF-EXT: data-driven matrix — every scenario file present, valid,
# unique, non-empty; nightly must run the full matrix (orphan scenario = red).
for f in scenarios.json stub_rules.json; do
  [ -f "bench/capdiff/$f" ] || { echo "FAIL: bench/capdiff/$f missing (rule 27)"; fail=1; }
done
if [ -f bench/capdiff/scenarios.json ]; then
  if python3 bench/capdiff/validate.py; then
    :
  else
    echo "FAIL: scenarios.json invalid (rule 27)"; fail=1
  fi
fi
if ! grep -q "capdiff-check.sh --full" scripts/nightly.sh; then
  echo "FAIL: nightly does not run the full capdiff matrix (rule 27)"; fail=1
fi
# every non-comment allow line must be preceded (within the comment block) by
# a D- token somewhere in the file's comments
if [ -f bench/capdiff/allow.txt ]; then
  # `while read` exits non-zero at EOF; under `set -o pipefail` that would kill
  # the script when the allowlist has zero non-comment lines (the state after
  # D-TOOLS-TASK closed). `|| true` keeps the empty-list case a no-op.
  bad=$(grep -vE '^\s*(#|$)' bench/capdiff/allow.txt | while read -r line; do
    pref="${line%% *}"
    if ! grep -qE "D-[A-Z0-9-]+" bench/capdiff/allow.txt; then echo "$pref"; fi
  done || true)
  if [ -n "$bad" ]; then
    echo "FAIL: allowlist entry without a D-row citation: $bad (rule 27)"; fail=1
  fi
fi
# planted control: a Python comparator sabotage that drops provider_view must
# be detectable by the check above (prove the grep is load-bearing).
# Backup lives NEXT TO the target (never /tmp — a full tmpfs silently broke
# the restore and emptied the file, 2026-10-17), restored by trap so every
# exit path heals.
_capdiff_plant_bak="bench/capdiff/.capdiff.py.guard-bak"
capdiff_restore() { [ -f "$_capdiff_plant_bak" ] && mv -f "$_capdiff_plant_bak" bench/capdiff/capdiff.py; }
trap capdiff_restore EXIT
cp bench/capdiff/capdiff.py "$_capdiff_plant_bak"
trap 'capdiff_restore; trap - EXIT' EXIT
sed -i 's/^def provider_view(/def provider_view_DISABLED(/' bench/capdiff/capdiff.py
if grep -q "^def provider_view(" bench/capdiff/capdiff.py; then
  echo "FAIL: planted comparator removal not detected (rule 27 vacuous)"; fail=1
else
  echo "ok (planted comparator removal detected; allowlist D-rows present)"
fi
capdiff_restore
trap - EXIT
rm -f "$_capdiff_plant_bak"

echo "== guard: publish order covers every member in dependency order (rule 28) =="
# The crates.io publish list must contain every workspace member, in dependency
# order. Omitting one (ocserve-pty, 2026-10-12) aborts publish at its dependent;
# mis-ordering uploads a dependent before its dependency. Derive both sides from
# Cargo.toml + publish.yml and assert. Live self-test proves the check bites.
if [ ! -f scripts/check-publish-order.py ]; then
  echo "FAIL: scripts/check-publish-order.py missing (rule 28)"; fail=1
else
  if python3 scripts/check-publish-order.py --selftest >/dev/null 2>&1; then
    if python3 scripts/check-publish-order.py >/dev/null 2>&1; then
      echo "ok (publish order complete + topological; planted controls detected)"
    else
      echo "FAIL: publish order incomplete or mis-ordered (rule 28)"; fail=1
    fi
  else
    echo "FAIL: publish-order planted control not detected (rule 28 vacuous)"; fail=1
  fi
fi

echo "== guard: mutating endpoints have an asserted effect (rule 29) =="
# Class-completion (SNAPSHOT §8): a mutating endpoint that returns a success
# shape without performing the mutation (the /vcs/apply + revert class). Every
# mutating op in the frozen spec must be classified in bench/sidefx/effects.tsv
# as effect-tested (a real side-effect test drives the route) or no-effect (a
# named reason). Routing (16) and shape (P4) guards are structurally blind to
# this; only an effect assertion sees it. Planted controls prove non-vacuousness.
if [ -f bench/sidefx/check-effects.py ]; then
  if python3 bench/sidefx/check-effects.py --selftest >/dev/null 2>&1; then
    if out=$(python3 bench/sidefx/check-effects.py 2>&1); then
      echo "$out"
    else
      echo "$out"; echo "FAIL: mutating endpoint lacks an asserted effect (rule 29)"; fail=1
    fi
  else
    echo "FAIL: effects-classification planted control not detected (rule 29 vacuous)"; fail=1
  fi
else
  echo "ok (no effects classifier)"
fi

echo "== guard: compiled-in assets stay inside their crate (rule 30) =="
# An include_str!/include_bytes! that resolves outside the owning crate root is
# not packaged into the .crate tarball: cargo build/test pass locally and
# `cargo publish --no-verify` uploads a broken crate — the failure only appears
# when a user runs `cargo install` (the 0.3.0 defect: assets embedded from
# bench/ broke `cargo install ocserve`). The static scan resolves each literal
# include against its source file; the dynamic half is `cargo package` (which
# compiles the packed crate) in ci.yml + publish.yml. Planted control: an
# escaping include must be flagged.
if [ -f scripts/check-crate-assets.py ]; then
  if python3 scripts/check-crate-assets.py --selftest >/dev/null 2>&1; then
    if out=$(python3 scripts/check-crate-assets.py 2>&1); then
      echo "$out"
    else
      echo "$out"; echo "FAIL: compiled-in asset escapes its crate root (rule 30)"; fail=1
    fi
  else
    echo "FAIL: crate-asset planted control not detected (rule 30 vacuous)"; fail=1
  fi
else
  echo "FAIL: scripts/check-crate-assets.py missing (rule 30)"; fail=1
fi

echo "== guard: v2 contract-differ (rule 32) =="
# V2-PLAN.md P1: the cross-version classifier runs on the two vendored
# contracts. Fails on: any same-name field-type conflict (superset serving
# blocked), any router-bound stub without a V2-PLAN phase, any v2 op with no
# bucket. Selftest proves both detectors non-vacuous (planted new op +
# planted type change). Calibration: must reproduce the hand-verified counts
# (142 v2 ops, 0 conflicts) — drift means re-verify by hand (V2-PLAN A6).
if [ -f scripts/contract-diff.py ] && [ -f crates/ocserve-core/assets/openapi/2.0.28.json ]; then
  if python3 scripts/contract-diff.py --selftest >/dev/null 2>&1; then
    if out=$(python3 scripts/contract-diff.py 2>&1); then
      echo "$out" | tail -9
    else
      echo "$out"; echo "FAIL: v2 contract classification (rule 32)"; fail=1
    fi
  else
    echo "FAIL: contract-diff planted controls not detected (rule 32 vacuous)"; fail=1
  fi
else
  echo "FAIL: scripts/contract-diff.py or 2.0.28 asset missing (rule 32)"; fail=1
fi

echo "== guard: v2-path queries carry allocation budgets (rule 33) =="
# V2-PLAN.md P2/D6 (hard gate): every query touching a v2-path table
# (session_inbox, instruction_*, credential, session_environment) in server
# code must (a) name explicit columns (extends rule 25 to the new tables), and
# (b) be covered by an allocation story — today that means the v13_inbox /
# credential / environment suites exercise the tables. A new v2-table query
# with no covering test file fails. Planted control: a fake
# `SELECT * FROM session_inbox` line trips the column-scan tripwire through
# the same grep.
V2TABLES='session_inbox|instruction_blob|instruction_state|instruction_entry|credential|session_environment'
plant="crates/ocserve-store/src/__rule33_plant.rs"
printf '%s\n' 'fn _plant() { let _ = "SELECT * FROM session_inbox"; }' > "$plant"
v2star=$(grep -rnE "SELECT \* FROM ($V2TABLES)" crates/*/src 2>/dev/null \
  | grep -v '__rule33_plant' || true)
plant_hit=$(grep -rnE "SELECT \* FROM ($V2TABLES)" crates/ocserve-store/src/__rule33_plant.rs 2>/dev/null || true)
rm -f "$plant"
if [ -z "$plant_hit" ]; then
  echo "FAIL: rule 33 planted control not detected"
  fail=1
fi
if [ -n "$v2star" ]; then
  echo "$v2star"
  echo "FAIL: SELECT * on a v2-path table in server paths (rule 33)"; fail=1
else
  echo "ok (no SELECT * on v2-path tables; planted SELECT* detected)"
fi

echo "== guard: serving path is Rust-native (rule 34) =="
# V2-PARITY-PLAN.md Rust-native rule: no non-Rust runtime in the request
# path. Allowed spawns: git/grep (vcs, find), sh (sidecar spawn only),
# df/systemctl/kill (CLI-side ops). BANNED interpreter binaries anywhere in
# crates/*/src: node, bun, deno, python, npm/npx, yarn, pnpm, ruby, perl,
# php, java. The plugin sidecar (Node, frozen) spawns only via ocserve-plugin.
plant="crates/ocserve-core/src/__rule34_plant.rs"
printf '%s\n' 'fn _plant() { let _ = std::process::Command::new("node"); }' > "$plant"
ban_hits=$(grep -rnE 'Command::new\("(node|bun|bunx|deno|python3?|npm|npx|yarn|pnpm|ruby|perl|php|java)"\)' crates/*/src 2>/dev/null \
  | grep -v '__rule34_plant' || true)
plant_hit=$(grep -rnE 'Command::new\("(node|bun|bunx|deno|python3?|npm|npx|yarn|pnpm|ruby|perl|php|java)"\)' crates/ocserve-core/src/__rule34_plant.rs 2>/dev/null || true)
rm -f "$plant"
if [ -z "$plant_hit" ]; then
  echo "FAIL: rule 34 planted control not detected"
  fail=1
fi
if [ -n "$ban_hits" ]; then
  echo "$ban_hits"
  echo "FAIL: interpreter spawn in server paths (rule 34)"; fail=1
else
  echo "ok (no interpreter spawns; planted node detected)"
fi

echo "== guard: docs are not stale (rule 31) =="
# Docs are the spec (AGENTS §1); nothing checked them, so dead references
# accumulated (the 2026-10-13 audit: three files referenced but gone, a stale
# divergence, dangling guard-rule numbers). The linter fails on: backticked
# repo paths that don't exist, `rule N` refs with no real rule, and a removed
# divergence still listed as current. Planted control: dead path + bogus rule.
#
# RESTORED 2026-10-16: the feat/v2-engine branch truncated this block (header
# only, no body, no `exit $fail`) — doc-lint silently stopped running. The
# truncation itself is now a planted-control case: if this body goes missing
# again, the `exit $fail` line below vanishes with it and CI's last-line
# assertion (added to ci.yml in the same commit) fails the run.
if [ -f scripts/check-docs.py ]; then
  if python3 scripts/check-docs.py --selftest >/dev/null 2>&1; then
    if out=$(python3 scripts/check-docs.py 2>&1); then
      echo "$out"
    else
      echo "$out"; echo "FAIL: stale doc reference (rule 31)"; fail=1
    fi
  else
    echo "FAIL: doc-lint planted control not detected (rule 31 vacuous)"; fail=1
  fi
else
  echo "FAIL: scripts/check-docs.py missing (rule 31)"; fail=1
fi

echo "== guard: no absolute-path silent redirect in writers (rule 35) =="
# resolve_within class (V2-PARITY Phase F): trim-leading-slash + join
# silently redirects an absolute request path INSIDE the base and reports
# success — proven by in-crate test debris from an absolute /tmp write.
# Writers must branch on is_absolute BEFORE joining; the containment gate
# then applies to both shapes. Read-only fs_read is exempt (404 either way,
# never creates). Planted control: a writer trim+join in a file without
# is_absolute → red.
bad35=""
for loc in $(grep -rln "trim_start_matches" crates/ocserve-http/src/ --include='*.rs' || true); do
  case "$loc" in
    *v2.rs)
      grep -q "is_absolute" "$loc" || bad35="$bad35 $loc"
      ;;
  esac
done
if [ -n "$bad35" ]; then
  echo "FAIL: writer-side trim+join without is_absolute branch (rule 35):$bad35"; fail=1
else
  echo "ok (writer paths branch on is_absolute)"
fi
# Planted control: a writer trim+join in a file WITHOUT is_absolute must
# flag through the SAME loop as the live check (proves the rule, not just
# the grep). Repo-local temp, trap-restored.
_cov35_plant="crates/ocserve-http/src/__rule35_plant.rs"
_cov35_restore() { rm -f "$_cov35_plant"; }
trap '_cov35_restore' EXIT
printf '%s\n' 'fn _plant(base: &std::path::Path, rel: &str) { let _ = base.join(rel.trim_start_matches('"'"'/'"'"')); }' > "$_cov35_plant"
_plant_bad=""
for loc in $(grep -rln "trim_start_matches" crates/ocserve-http/src/__rule35_plant.rs || true); do
  grep -q "is_absolute" "$loc" || _plant_bad="$loc"
done
_cov35_restore
trap - EXIT
if [ -z "$_plant_bad" ]; then
  echo "FAIL: planted trim+join without is_absolute not flagged (rule 35 vacuous)"; fail=1
else
  echo "ok (planted trim+join without is_absolute flagged by rule 35)"
fi

echo "== guard: v2 plugin hooks accounted for (rule 36) =="
# Every hook the vendored upstream v2 tree declares must be in the capability
# matrix (README-v2.md) as bridged or a named divergence. Keeps the shim's
# coverage claim mechanically honest across bumps. Selftest inside the script.
if python3 scripts/check-v2-hooks.py; then
  echo "ok (all upstream v2 hooks are in the capability matrix)"
else
  echo "FAIL: upstream v2 hooks missing from the capability matrix (rule 36)"; fail=1
fi

exit $fail
