#!/usr/bin/env bash
# refine soak (TESTING §8 / MEMORY §5): long-run sampler.
# Usage: scripts/soak.sh <url> <interval_s> <hours>
# Appends one CSV line per tick: ts,rss,peak,sidecar_rss,wal,sse_clients,queue,health
# Gate (evaluated afterwards): slope <1 MB/h after h1, zero swap growth, no health=503.
set -euo pipefail
URL="${1:?url (e.g. http://127.0.0.1:4911)}"
INTERVAL="${2:-60}"
HOURS="${3:-24}"
OUT="${SOAK_OUT:-/tmp/opencode/refine-soak-$(date +%s).csv}"
echo "ts,rss,peak,sidecar,wal,sse,queue,health" > "$OUT"
end=$(( $(date +%s) + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$end" ]; do
  ts=$(date +%s)
  m=$(curl -fsS --max-time 5 "$URL/metrics" || true)
  health=$(curl -fss -o /dev/null -w '%{http_code}' --max-time 5 "$URL/global/health" || echo 000)
  get() { echo "$m" | awk -v k="$1" '$1==k {print $2; exit}'; }
  echo "$ts,$(get refine_rss_bytes),$(get refine_rss_peak_bytes),$(get refine_sidecar_rss_bytes),$(get refine_wal_bytes),$(get refine_sse_clients),$(get refine_writer_queue_depth),$health" >> "$OUT"
  sleep "$INTERVAL"
done
echo "soak done: $OUT"
