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

step() { # $1 = label, rest = command
  local label="$1"; shift
  echo "== ${label} ==" | tee -a "$LOG"
  if "$@" >>"$LOG" 2>&1; then
    echo "ok" | tee -a "$LOG"
  else
    echo "FAIL: ${label}" | tee -a "$LOG"
    FAILS=$((FAILS + 1))
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
size_gate() {
  local bin="target/release/ocserve"
  if [ ! -x "$bin" ]; then
    echo "release binary missing — building"
    cargo build --release || return 1
  fi
  local sz ceiling=30000000
  sz="$(stat -c%s "$bin")"
  echo "size=${sz} ceiling=${ceiling}"
  [ "$sz" -le "$ceiling" ] || { echo "OVER SIZE CEILING"; return 1; }
}
step "uninstall selftest (canary battery)" bash -c './scripts/uninstall.sh --selftest'
step "binary size ceiling" bash -c "$(declare -f size_gate); size_gate"

# 6. zen free-tier gate (live trio, 3 requests): positive + text-only
#    (compaction shape) + negative (malformed session id — proves the wall
#    still exists). Evidence ledger: bench/zen-probe/FINDINGS.md; production
#    counts flips via ocserve_zen_freetier_total (K-MODEL-STATE).
step "zen free-tier gate (live trio)" \
  cargo test -p ocserve-llm --test zen_live -- --ignored
step "models.dev live fetch" \
  cargo test -p ocserve live_fetch_contains_big_pickle -- --ignored

# 6b. permission differential (K-PERMISSION, guard rule 18's live half): boots
#     freeze + ocserve under one fixture and replays the committed permission
#     oracle vectors. Behavioural layer the route/shape guards cannot see.
step "permission differential (freeze vs ocserve)" ./scripts/permission-check.sh

# 7. mutants (opt-in): survivor report is triaged like a defect (TESTING §9)
if [ "$WITH_MUTANTS" = "1" ]; then
  if command -v cargo-mutants >/dev/null 2>&1; then
    # skip the 50s fuzz (per-mutant cost) — mutants target store/core logic
    # per-mutant `cargo test` skips the 50s fuzz + SIGKILL harness (both run
    # unmutated every gate/nightly anyway). QUIRK (observed twice): cargo-
    # mutants joins test args WITHOUT inserting `--`, and cargo test rejects
    # bare --skip — so the separator must be PASSED as the first test arg:
    #   ... -- -- --skip <name>   →  cargo test ... -- --skip <name>
    # TMPDIR on REAL disk: default /tmp is tmpfs on some hosts — the first
    #529-mutant run filled the16G ramdisk and died with ENOSPC mid-build
    mkdir -p target/mutants-tmp
    step "cargo mutants (store+core)" env TMPDIR="$PWD/target/mutants-tmp" \
      cargo mutants -p ocserve-store -p ocserve-core --timeout 120 \
      -- -- --skip chunk_split --skip sigkill
  else
    echo "FAIL: --with-mutants but cargo-mutants not installed" | tee -a "$LOG"
    FAILS=$((FAILS + 1))
  fi
fi

echo "== nightly summary: ${FAILS} failed ==" | tee -a "$LOG"
echo "log: $LOG"
[ "$FAILS" -eq 0 ]
