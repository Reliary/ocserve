#!/usr/bin/env bash
# Static guardrails (AGENTS §2.3/§2.5) — one command that fails the build on
# the bug CLASSES we have actually shipped once each:
#   1. swallowed storage writes        (the finalize-SQL silent failure)
#   2. literal backslash inside SQL    (the "\\" continuation syntax error)
#   3. whole-session Value materialization in HTTP (the OOM class) — the
#      messages route must stream via for_each_message_json
#   4. msg_part INSERT/UPDATE outside refine-store — every part write must
#      carry its part_search projection companion (W1 FTS invariant)
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
if out=$(grep -rn --include='*.rs' 'load_messages(' crates/refine-http/src 2>/dev/null); then
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

exit $fail
