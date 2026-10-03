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
  ts = $1 + 0; rss = $2 + 0; health = $10 + 0
  if (health != 200 && health != 0) { bad_health++; if (bad_ts == 0) bad_ts = ts }
  if (n == 1) { t0 = ts }
  # anchor: first sample after t0+3600 (gate applies from hour 1)
  if (anchor_t == 0 && ts >= t0 + 3600) { anchor_t = ts; anchor_rss = rss }
  last_t = ts; last_rss = rss
}
END {
  if (n < 3) { print "FAIL: only " n " samples"; exit 1 }
  dur = last_t - t0
  if (dur < 7200) { print "FAIL: duration " int(dur/60) " min < 2h (gate needs data)"; exit 1 }
  if (bad_health > 0) { print "FAIL: " bad_health " samples with health != 200 (first at " bad_ts ")"; exit 1 }
  if (anchor_t == 0) { print "FAIL: no sample after hour 1"; exit 1 }
  span = last_t - anchor_t
  slope = (last_rss - anchor_rss) / span * 3600   # bytes/hour
  mbh = slope / 1048576
  printf "samples=%d duration=%.1fh rss_slope=%.3f MiB/h (anchor %.1f MiB -> last %.1f MiB)\n", \
    n, dur/3600, mbh, anchor_rss/1048576, last_rss/1048576
  if (mbh >= 1.0) { print "FAIL: RSS slope >= 1 MiB/h after hour 1"; exit 1 }
  print "PASS"
}
' "$CSV"
