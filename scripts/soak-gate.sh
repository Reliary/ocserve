#!/usr/bin/env bash
# Machine-checked soak gate (replaces eyeballing the CSV — TESTING anti-theater).
# Gate (soak.sh header): slope <1 MB/h after hour 1; no health != 200;
# duration >= 2h (fresh-deploy soaks run 24h; gate needs data first).
#
# Usage: scripts/soak-gate.sh <csv>     Exit: 0 pass / 1 fail (prints reasons)
set -euo pipefail
CSV="${1:?csv path (scripts/soak.sh output)}"
[ -s "$CSV" ] || { echo "FAIL: empty/missing csv"; exit 1; }

awk -F, '
NR == 1 { next }
{
  n++
  ts = $1 + 0; rss = $2 + 0; health_raw = $10; health = $10 + 0; wal = $5 + 0
  # cgroup (col 15) and kids (col 17) bound the WHOLE service, not just the
  # server RSS. A 500 MB kids allocation used to PASS this gate because only
  # column 2 was read (2026-10-23 review).
  cg = $15 + 0; kids = $17 + 0
  cgv[n] = cg; kidsv[n] = kids
  # A failed probe records 000000 ("000" + the `|| echo 000` fallback), which
  # coerces to 0. The old rule accepted 0, so a timed-out probe PASSED the gate
  # (live 2026-10-23: two `curl: (28)` rows and a green report). Only the exact
  # literal 200 is healthy; 0 is a lost sample, not a success.
  if (health_raw != "200") { bad_health++; if (bad_ts == 0) bad_ts = ts }
  if (n == 1) { t0 = ts }
  # anchor: first sample after t0+3600 (gate applies from hour 1)
  if (anchor_t == 0 && ts >= t0 + 3600) { anchor_t = ts; anchor_rss = rss }
  # WAL gate (perf guard audit 2026-10-11 blind spot h): the WAL gauge is
  # sampled but was never gated — an un-checkpointing writer (long reader
  # pinning the snapshot, checkpoint starvation) grows the WAL unboundedly.
  # Track the max after the same hour-1 anchor; gate on absolute size, not
  # slope, because the WAL legitimately seestooths between checkpoints.
  if (anchor_t != 0 && wal > wal_max) { wal_max = wal }
  if (wal > 0) { wal_seen = 1 }
  last_t = ts; last_rss = rss
}
END {
  if (n < 3) { print "FAIL: only " n " samples"; exit 1 }
  dur = last_t - t0
  if (dur < 7200) { print "FAIL: duration " int(dur/60) " min < 2h (gate needs data)"; exit 1 }
  if (bad_health > 0) { print "FAIL: " bad_health " samples whose health probe was not exactly 200 (first at " bad_ts ")"; exit 1 }
  # Containment ceilings are 1024M parent / 700M kids; a sustained breach means
  # the classes are not capped as configured. 5% margin for accounting skew.
  cg_bad = 0; kids_bad = 0
  for (i = 0; i < n; i++) {
    # Empty/blank columns coerce to 0 and are ignored (old runs pre-date the
    # cgroup columns); only a REAL value above the cap is a failure.
    if (cgv[i] > 1073741824 * 1.05) cg_bad++
    if (kidsv[i] > 734003200 * 1.05) kids_bad++
  }
  if (cg_bad > 0) { print "FAIL: " cg_bad " samples above the parent memory max"; exit 1 }
  if (kids_bad > 0) { print "FAIL: " kids_bad " samples above the kids memory max"; exit 1 }
  if (anchor_t == 0) { print "FAIL: no sample after hour 1"; exit 1 }
  span = last_t - anchor_t
  slope = (last_rss - anchor_rss) / span * 3600   # bytes/hour
  mbh = slope / 1048576
  printf "samples=%d duration=%.1fh rss_slope=%.3f MiB/h (anchor %.1f MiB -> last %.1f MiB)\n", \
    n, dur/3600, mbh, anchor_rss/1048576, last_rss/1048576
  if (mbh >= 1.0) { print "FAIL: RSS slope >= 1 MiB/h after hour 1"; exit 1 }
  # WAL ceiling: wal_autocheckpoint is 1000 pages (≈4 MiB) + journal_size_limit
  # 64 MiB; sustained growth past 256 MiB means checkpointing is starved.
  if (wal_seen) {
    printf "wal_max_after_h1=%.1f MiB\n", wal_max/1048576
    if (wal_max > 268435456) { print "FAIL: WAL exceeded 256 MiB after hour 1 (checkpoint starvation)"; exit 1 }
  }
  print "PASS"
}
' "$CSV"
