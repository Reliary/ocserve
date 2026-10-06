#!/usr/bin/env bash
# Static guardrails (AGENTS §2.3/§2.5) — one command that fails the build on
# the bug CLASSES we have actually shipped once each:
#   1. swallowed storage writes        (the finalize-SQL silent failure)
#   2. literal backslash inside SQL    (the "\\" continuation syntax error)
#   3. whole-session Value materialization in HTTP (the OOM class) — the
#      messages route must stream via for_each_message_json
#   4. msg_part INSERT/UPDATE outside refine-store — every part write must
#      carry its part_search projection companion (W1 FTS invariant)
#   5. upstream drift watch broken (version compare / replay-output parse /
#      set-difference / report writer) — the nightly watch would go silently
#      wrong (PLAN §6/§8, K-DRIFT)
#   6. non-refine systemd units mutated from repo scripts (drop-ins/restarts —
#      the 2026-10-05 incident class; only refine* units are mutable, see
#      TESTING §1.6)
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
if out=$(grep -rn --include='*.rs' 'load_messages(' crates/refine-http/src 2>/dev/null | grep -v 'allow:load_messages'); then
  echo "$out"; echo "FAIL: load_messages in HTTP materializes whole sessions (OOM class) — use for_each_message_json"; fail=1
else
  echo "ok"
fi

echo "== guard: msg_part writes only in refine-store =="
if out=$(grep -rn --include='*.rs' -E 'INSERT (OR REPLACE |OR IGNORE )?INTO msg_part|UPDATE msg_part SET' crates/*/src 2>/dev/null | grep -v 'crates/refine-store/src/'); then
  echo "$out"; echo "FAIL: msg_part INSERT/UPDATE outside refine-store skips the search projection (use part_row_ops / insert_message / update_part)"; fail=1
else
  echo "ok"
fi

echo "== guard: upstream drift-watch selftest =="
if ./scripts/drift-watch.sh --selftest >/dev/null 2>&1; then
  echo "ok"
else
  echo "FAIL: drift-watch --selftest (upstream drift watcher broken — PLAN §6/§8 watch)"; fail=1
fi

echo "== guard: non-refine systemd units are read-only in scripts =="
# Named exception: lines carrying 'refine' (refine.service, refine-*.service,
# refine-tailscale-forward) — refine's own deploy/restart is legitimate.
# Read-only inspect (status/cat/show/is-active) is intentionally not matched.
if out=$(grep -rnE 'systemd/user/[A-Za-z0-9@._-]*service\.d|systemctl --user (edit|restart|stop|start|mask|kill)\b' \
    scripts bench --include='*.sh' --include='*.py' 2>/dev/null \
    | grep -v 'check-guards\.sh' | grep -v refine); then
  echo "$out"; echo "FAIL: repo script mutates a non-refine systemd unit (2026-10-05 incident class) — experiments belong in disposable containers (TESTING §1.6)"; fail=1
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

exit $fail
