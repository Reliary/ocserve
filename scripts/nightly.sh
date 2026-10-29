#!/usr/bin/env bash
# Nightly verification program (SRE §CI/nightly — wired LOCALLY; promoted to
# PR CI when a remote exists, per PLAN §6). Companion to drift-watch.sh
# (upstream release triage — own cron line, flock-isolated).
#
# Every step FAIL-VISIBLE: a step that cannot run (network down for audit,
# missing binary) is a non-zero step, never a silent skip.
#
# Usage: scripts/nightly.sh [--with-mutants]
#   --with-mutants  also run cargo-mutants on ocserve-store/ocserve-core
#                   (hours of CPU — manual/weekly; NEVER during benchmark
#                   sessions: interleaved-comparison rule)
# Log:  bench/drift/nightly-<ts>.log (gitignored)  Exit: 0 all steps ok
set -uo pipefail
cd "$(dirname "$0")/.."

WITH_MUTANTS=0
[ "${1:-}" = "--with-mutants" ] && WITH_MUTANTS=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="bench/drift/nightly-${STAMP}.log"
mkdir -p bench/drift
: >"$LOG"
FAILS=0
FAILED_STEPS=()

# Receipt for `ocserve doctor` (scripts/lib/gate-status.sh). Nightly was red on
# 19 of 21 runs (2026-10-04 → 2026-10-27) and nothing surfaced it: the timer's
# exit code lived only in `systemctl show`, the log in a gitignored dir.
# shellcheck source=scripts/lib/gate-status.sh
. "$(dirname "$0")/lib/gate-status.sh"
gate_begin nightly

step() { # $1 = label, rest = command
  local label="$1"; shift
  echo "== ${label} ==" | tee -a "$LOG"
  if "$@" >>"$LOG" 2>&1; then
    echo "ok" | tee -a "$LOG"
  else
    echo "FAIL: ${label}" | tee -a "$LOG"
    FAILS=$((FAILS + 1))
    FAILED_STEPS+=("$label")
  fi
}

# 1. supply chain: vulnerabilities + licenses/bans/sources (deny.toml)
step "cargo audit" cargo audit
step "cargo deny" cargo deny check

# 2. crash protocol: SIGKILL fuzz on the blob/DB writer (M0 decisive test)
step "crash fuzz (SIGKILL blob/DB)" cargo test -p ocserve-store --test crash_fuzz

# 2b. CORPUS GATES — the byte-parity differentials for the read-path fast
#     paths. These are env-gated and would otherwise never run: no CI exists
#     in this repo, so a `cargo test` run silently skips them (they print
#     "skipped: ... not set" and pass). Run them here against the LIVE db,
#     read-only (`open_reader`), so the gates track the real corpus:
#     - splice: every msg.info + every part (inline AND blob_sha) — the
#       splice fast path must be byte-identical to the DOM path, 0 refused;
#       226,646 rows on the load fixture, more live.
#     - list wire: session-list bytes built from columns must equal the
#       JSON-building path.
# A skipped gate is worse than no gate: it looks green while proving
# nothing. So the step FAILS if the env var could not be set (missing db).
step "corpus: splice byte-parity over live db" \
  env OCSERVE_SPLICE_DB="${OCSERVE_DATA_DIR:-$HOME/.local/share/ocserve}/ocserve.db" \
      cargo test -p ocserve-store --test splice_parity -- --nocapture
step "corpus: session-list wire parity over live db" \
  env OCSERVE_LIST_PARITY_DB="${OCSERVE_DATA_DIR:-$HOME/.local/share/ocserve}/ocserve.db" \
      cargo test -p ocserve-store --test list_wire_parity -- --nocapture

# 3. provider stream fuzz: 10k seeded chunk-boundary splits vs whole-buffer
step "stream chunk-split fuzz" cargo test -p ocserve-llm chunk_split_fuzz

# 3b. cargo-fuzz smoke pass (TESTING §6): every parser target for a bounded
#     10s each. Weekly CI runs the 60s variants (fuzz.yml); this keeps the
#     targets live in the local nightly too. Skipped-with-a-note (not FAIL)
#     when the nightly toolchain/cargo-fuzz is absent — a missing dev tool is
#     not a product defect, but it IS reported so the absence is visible.
if rustup toolchain list 2>/dev/null | grep -q '^nightly' && command -v cargo-fuzz >/dev/null 2>&1; then
  for t in sse_parser splice_parser wire_decode event_decoder stream_assembler config_toml substr_window mcp_jsonrpc; do
    step "fuzz smoke: $t (10s)" \
      cargo +nightly fuzz run "$t" -- -max_total_time=10 -timeout=30
  done
else
  echo "== fuzz smoke: SKIPPED (nightly toolchain or cargo-fuzz absent) ==" | tee -a "$LOG"
fi

# 4. backup drill: online VACUUM INTO while serving, then integrity +
#    row-count check on the COPY (live service never written)
drill() {
  local tmp bin
  tmp="$(mktemp -d)"
  bin="target/release/ocserve"
  [ -x "$bin" ] || bin="$(command -v ocserve || true)"
  if [ -z "$bin" ]; then echo "no ocserve binary"; return 1; fi
  local db="${OCSERVE_DATA_DIR:-$HOME/.local/share/ocserve}/ocserve.db"
  [ -f "$db" ] || { echo "no db at $db"; return 1; }
  # live count FIRST (backup may only grow, never lose, committed rows)
  local live
  live="$(python3 - "$db" <<'PY'
import sqlite3, sys
c = sqlite3.connect("file:%s?mode=ro" % sys.argv[1], uri=True)
print(c.execute("select count(*) from msg").fetchone()[0])
PY
)" || return 1
  "$bin" backup --dest "$tmp/b.db" || { rm -rf "$tmp"; return 1; }
  python3 - "$tmp/b.db" "$live" <<'PY'
import sqlite3, sys
conn, live = sys.argv[1], int(sys.argv[2])
c = sqlite3.connect(conn)
ok = c.execute("pragma integrity_check").fetchone()[0]
n = c.execute("select count(*) from msg").fetchone()[0]
print(f"integrity={ok} msgs={n} (live-at-start={live})")
assert ok == "ok", "integrity_check failed"
assert n >= live, f"backup lost rows: {n} < {live}"
PY
  local rc=$?
  rm -rf "$tmp"
  return $rc
}
step "backup drill (VACUUM INTO + integrity + counts)" bash -c "$(declare -f drill); drill"

# 5. binary size ceiling (re-baselined, never bypassed — provenance rule):
#    9,279,528 B measured 2026-10-03 → ceiling 10,485,760 B (10.0 MiB)
#    20,359,600 B measured 2026-10-06 → ceiling 20,971,520 B (20 MiB)
#    20,999,792 B measured 2026-10-07 → ceiling 21,500,000 B
#      (+F78/B1/S-A code and x86-64-v3 C-track; PERF-10X C7 standing rule —
#       measured, provenance comment, ceiling moved, never bypassed)
#      D1: rolldown linked into ocserve as the plugin normalizer
#      (decision record: bench/normalizer-spike/D1-PLAN.md — user call after
#      Phase-3 probe; 20.3 MB still 9x smaller than upstream's 185 MB ELF;
#      tripwire re-baselined from measurement, thresholds never moved to fit)
#     K-PTY/K-REVERT/SDK-route batch: 21,124,176 B measured 2026-10-08
#      (pty crate + compat routes) → ceiling 21,500,000 B unchanged (fits)
#     K-WEBUI-PERF batch: 22,433,968 B measured 2026-10-08 (brotli + flate2
#      linked for web-UI compression, WEBUI-PLAN W1) → ceiling 23,000,000 B
#      (still ~8x smaller than upstream's 185 MB ELF; raised from measurement,
#      never bypassed)
#     K-TUI-SURFACE batch: 23,341,872 B measured 2026-10-09 (full v2 /api/*
#      surface ~90 route handlers + /doc embedded spec; the generated
#      coverage guard requires every op bound) → ceiling 24,000,000 B
#     P6 embedded web UI: 28,473,616 B measured 2026-10-09 (the pinned
#      1.18.31 app pack, zstd 5.08 MB, embedded via include_bytes! — upstream
#      parity: upstream embeds its whole app/dist too; still ~6.5x smaller
#      than upstream's 185 MB ELF) → ceiling 30,000,000 B
#     Phase E second pack: 32,965,584 B measured 2026-10-17 (the pinned
#      2.0.28 app pack, zstd 3.16 MB, same pipeline — both packs embedded,
#      selected by OCSERVE_UI_PACK; still ~5.6x smaller than upstream's
#      185 MB ELF) → ceiling 34,000,000 B
#     K-SECREV web-UI completeness: 61,479,024 B measured 2026-10-18. The
#      browser acceptance (scripts/webui-v2-check.py) proved the first pack
#      missed ~52 lazily code-split chunks AND the root assets (sw.js/favicon);
#      the complete closure is v1 944 files/36 MB raw + v2 1678 files/87 MB raw
#      (36 MB of it lazily-loaded office-preview WASM). Packs switched to
#      OCAP2 (per-file zstd frames) so the loader decompresses ONE file at a
#      time into a 16 MiB bounded LRU — resident memory is O(16 MiB), not the
#      87 MB closure (MEMORY.md bounded-by-construction). Size re-baselined
#      from measurement (provenance rule); still ~3x smaller than upstream's
#      185 MB ELF. NOTE: 36 MB of this is office-document-preview WASM — a
#      candidate to drop if a leaner default is wanted (documented tradeoff).
#      P0-P6 plugin batch: 61,746,576 B measured 2026-10-22 (hook timeouts +
#      duplex sidecar + plugin tools + v2 prompt/dispatch/proxies; no new
#      embedded assets) → ceiling 64,000,000 B (provenance rule).
#     D-ZEN-TLS: 64,330,256 B measured 2026-10-25 (BoringSSL/hyper-boring
#      linked for the opencode.ai fingerprint gate; +~2.1 MB over rustls).
#      → ceiling 66,000,000 B (provenance rule; still ~2.9x smaller than
#      upstream's 185 MB ELF).
size_gate() {
  local bin="target/release/ocserve"
  if [ ! -x "$bin" ]; then
    echo "release binary missing — building"
    cargo build --release || return 1
  fi
  local sz ceiling=66000000
  sz="$(stat -c%s "$bin")"
  echo "size=${sz} ceiling=${ceiling}"
  [ "$sz" -le "$ceiling" ] || { echo "OVER SIZE CEILING"; return 1; }
}
# cgroup containment (SRE §5.2): boots a transient unit and drives the REAL
# /shell route. Local-only by design - the dance needs a delegated systemd
# tree that CI runners do not provide; `check-guards.sh` rule 45 is the CI
# proxy (static, with a planted control).
# Plugin hook contract (2026-10-22): captures the reference's trigger payloads,
# replays them against ocserve, and asserts shape + order + consumption. Local
# only — needs the vendored freeze. Guard rule 47 is the CI-side proxy.
step "hook contract (capture + diff + effects)" bash -c './scripts/hook-contract-check.sh'
step "child containment battery" bash -c './scripts/cgroup-battery.sh'
step "uninstall selftest (canary battery)" bash -c './scripts/uninstall.sh --selftest'
step "binary size ceiling" bash -c "$(declare -f size_gate); size_gate"

# 6. zen free-tier gate (live trio, 3 requests): positive + text-only
#    (compaction shape) + negative (malformed session id — proves the wall
#    still exists). Evidence ledger: bench/zen-probe/FINDINGS.md; production
#    counts flips via ocserve_zen_freetier_total (K-MODEL-STATE).
step "zen free-tier gate (live trio)" \
  cargo test -p ocserve-core --test zen_live -- --ignored
step "models.dev live fetch" \
  cargo test -p ocserve live_fetch_contains_big_pickle -- --ignored

# 6b. permission differential (K-PERMISSION, guard rule 18's live half): boots
#     freeze + ocserve under one fixture and replays the committed permission
#     oracle vectors. Behavioural layer the route/shape guards cannot see.
step "permission differential (freeze vs ocserve)" ./scripts/permission-check.sh

# 6c. event-payload scan (guard rule 19's live half): validates the live event
#     log against the frozen Event contract. Post-fix events must be clean;
#     historical pre-fix rows are counted but not fatal (already fixed at the
#     source). Read-only. NOTE: `checked 0` on an idle window is NO DATA, not
#     a pass — the count is echoed so the report states vacuity explicitly.
step "event payloads vs frozen contract" \
  bash -c 'out=$(python3 bench/events/event-validate.py \
    "${OCSERVE_DATA_DIR:-$HOME/.local/share/ocserve}/ocserve.db" --minutes 60); \
    echo "$out"; echo "$out" | grep -q "checked 0 events" && \
    echo "NOTE: event window empty — vacuous green (no traffic to validate)" || true'

# 6c-bis. snapshot/revert/vcs-apply integration (K-SNAPSHOT, SNAPSHOT.md §7):
#     containerised single-arm + freeze differential. Manual tier (docker);
#     skipped when docker is unavailable.
if command -v docker >/dev/null 2>&1; then
  step "snapshot integration + differential (freeze vs ocserve)" \
    ./bench/snapshot-it/run.sh
else
  echo "-- snapshot-it skipped (no docker)"
fi

# 6d. field-contract differential (K-FIELDCONTRACT, guard rule 20's live
#     half): boots freeze + ocserve under one fixture and byte-compares the
#     decode corpus (field-probes.md) + identity-echo assertions. The class
#     route/shape guards cannot see: request-KEY handling.
step "field-contract differential (freeze vs ocserve)" \
  ./scripts/field-contract-check.sh

# 6e. provider-boundary differential (K-CAPDIFF, guard rule 27's live half):
#     boots freeze + ocserve under one fixture with the provider baseURL
#     redirected to a capture sink, drives the SAME prompts, and diffs the
#     REAL server->provider request bodies (messages, system bytes, tool
#     schemas+descriptions, params) + the SSE event-type set. The boundary
#     every client-facing gate is blind to — the 2026-10-12 "session won't
#     continue" bug (raw-vs-filtered history) lived here. `--selftest` proves
#     the comparator is non-vacuous before trusting a green run.
step "provider-boundary differential (freeze vs ocserve)" \
  bash -c './scripts/capdiff-check.sh --selftest && ./scripts/capdiff-check.sh --full && ./scripts/capdiff-check.sh --full --reverse' 

# 6f. v2 column (K-V2-P5, V2-PARITY-PLAN.md §6): live differential vs the real
#     2.0.28 binary (11 vectors: info/inbox/form/log/messages/admission) +
#     v2 contract selftest + v2 shape validation + v2 drift detection.
#     Local-nightly only (needs the 2.0.28 binary + password harness; hosted
#     runners 500 on freeze probes — same reason as pair/capdiff). The live
#     arms are SKIPPED-with-a-note when the 2.0.28 artifacts are absent (a
#     missing dev tool is not a product defect — the established nightly rule,
#     same as the fuzz step); the offline contract-differ still runs.
V2CLI="${V2_CLI:-/var/tmp/ocserve-scratch/ocli/node_modules/@opencode/cli-linux-x64/bin/opencode}"
if [ -x "$V2CLI" ]; then
  step "v2 live differential (2.0.28 vs ocserve)" ./scripts/pair-v2-check.sh
  step "v2 shapes vs 2.0.28 contract" ./scripts/spec-validate-v2.sh
  step "v2 client acceptance (2.0.28 CLI vs ocserve)" ./scripts/v2cli-check.sh
  if python3 -c 'import playwright' >/dev/null 2>&1; then
    step "v2 web UI browser acceptance (2.0.28 pack)" \
      bash -c 'D=$(mktemp -d); cp -r testdata/fixture "$D/fh"; \
        setsid env HOME="$D/fh" OCSERVE_DATA_DIR="$D" OCSERVE_V2=1 OCSERVE_UI_PACK=2.0.28 \
        OPENCODE_DISABLE_MODELS_FETCH=1 ./target/release/ocserve serve --port 5151 >"$D/s.log" 2>&1 & \
        P=$!; sleep 4; python3 scripts/webui-v2-check.py http://127.0.0.1:5151; rc=$?; \
        kill $P 2>/dev/null; rm -rf "$D"; exit $rc'
  else
    echo "== v2 web UI browser acceptance: SKIPPED (python playwright absent) ==" | tee -a "$LOG"
  fi
else
  echo "== v2 live column: SKIPPED (2.0.28 CLI absent at $V2CLI) ==" | tee -a "$LOG"
fi
step "v2 contract-differ selftest + classify" \
  bash -c 'python3 scripts/contract-diff.py --selftest && python3 scripts/contract-diff.py'
step "v2 drift detection (npm latest vs vendored)" ./scripts/v2-drift.sh

# 7. mutants (opt-in): survivor report is triaged like a defect (TESTING §9)
if [ "$WITH_MUTANTS" = "1" ]; then
  if command -v cargo-mutants >/dev/null 2>&1; then
    # skip the two known-slow harness tests per mutant (the 50s chunk-split
    # fuzz + the SIGKILL crash harness — both run unmutated every gate).
    # Acceleration lives in .cargo/mutants.toml (profile=mutants debug=none,
    # minimum_test_timeout=45). NOTE: cargo-mutants appends CONFIG test-args
    # after the post-`--` test-binary args without re-inserting the separator,
    # so `additional_cargo_test_args` must stay empty when using the
    # `-- -- --skip X` form below (they were removed for this reason).
    # jobs via CARGO_MUTANTS_JOBS (cargo-mutants defaults to 1; the measured
    # baseline used 4 locally / 8 on the i9).
    mkdir -p target/mutants-tmp
    # --test-workspace true is NOT optional. cargo-mutants runs only the
    # MUTATED PACKAGE's tests by default, so every `ocserve-store` function
    # covered solely by `ocserve-http`'s integration tests reads as a survivor.
    # Measured 2026-10-29: fork.rs scores 0/26 at the package default and
    # 13/13 with the workspace scope; revert.rs 10/16. (An earlier 23/3 for
    # fork.rs is withdrawn: those catches all came from crash_fuzz failing
    # spuriously before the migration-atomicity fix, so the run measured a
    # broken harness. See bench/mutants/BASELINE.md.)
    #
    # `--skip sigkill` stays: that harness kills a child at a random offset, so
    # under mutation it fails for reasons unrelated to the mutant. Note that it
    # is also the only test that ever observed the non-atomic migration bug
    # (2026-10-28), which is why that class is now pinned by a deterministic
    # test in tests/mig_atomicity.rs instead of relying on this fuzz.
    step "cargo mutants (store+core, workspace test scope)" env TMPDIR="$PWD/target/mutants-tmp" \
      CARGO_MUTANTS_JOBS="${CARGO_MUTANTS_JOBS:-4}" \
      ./scripts/run-mutants.sh
    # Gate the survivor count against the agreed ceiling (TESTING §1 rule 1:
    # threshold agreed at first run, then ratcheted down). Only meaningful if
    # the run produced outcomes; absent results are a FAIL not a skip.
    step "mutation-score gate" python3 scripts/check-mutants.py
  else
    echo "FAIL: --with-mutants but cargo-mutants not installed" | tee -a "$LOG"
    FAILS=$((FAILS + 1))
  fi
fi

echo "== nightly summary: ${FAILS} failed ==" | tee -a "$LOG"
echo "log: $LOG"
if [ "$FAILS" -eq 0 ]; then
  gate_finish nightly ok "all steps ok"
else
  gate_finish nightly failed "${FAILS} step(s) failed" "${FAILED_STEPS[@]}"
fi
[ "$FAILS" -eq 0 ]
