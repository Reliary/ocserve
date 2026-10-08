#!/usr/bin/env bash
# ocserve soak (TESTING §8 / MEMORY §5): long-run sampler.
# Usage: scripts/soak.sh <url> <interval_s> <hours>
# Appends one CSV line per tick: ts,rss,peak,sidecar_rss,wal,sse_clients,queue,locks,tasks,health
# Gate (evaluated afterwards): slope <1 MB/h after h1, zero swap growth, no health=503.
set -euo pipefail
URL="${1:?url (e.g. http://127.0.0.1:4911)}"
INTERVAL="${2:-60}"
HOURS="${3:-24}"
OUT="${SOAK_OUT:-/tmp/opencode/ocserve-soak-$(date +%s).csv}"
mkdir -p "$(dirname "$OUT")"
echo "ts,rss,peak,sidecar,wal,sse,queue,locks,tasks,health,rss_delta,db_opens,sync_us,oc_rss,cgroup,cgroup_peak,kids" > "$OUT"
end=$(( $(date +%s) + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$end" ]; do
  ts=$(date +%s)
  m=$(curl -fsS --max-time 5 "$URL/metrics" || true)
  health=$(curl -fss -o /dev/null -w '%{http_code}' --max-time 5 "$URL/global/health" || echo 000)
  get() { echo "$m" | awk -v k="$1" '$1==k {print $2; exit}'; }
  # labeled series (durations render name_sum{label}) — prefix match
  getp() { echo "$m" | awk -v k="$1" 'index($1, k) == 1 {print $2; exit}'; }
  # opencode mirror RSS (MEMORY §7.6 — native as the future-usage ceiling model)
  oc_rss=0
  oc_pid=$(pgrep -f "opencode serve --port 4901" 2>/dev/null | head -1 || true)
  if [ -n "$oc_pid" ] && [ -r "/proc/$oc_pid/status" ]; then
    oc_rss=$(awk '/^VmRSS:/ {print $2*1024}' "/proc/$oc_pid/status" 2>/dev/null || echo 0)
  fi
  # cgroup view (2026-10-05 OOM postmortem): the unit cap bounds main+sidecars
  # together; soak's sidecar metric tracks the plugin host only — cgroup/
  # kids close that visibility gap (kids = cgroup current − main rss).
  cg=""; cgp=""; kids=""
  cg_dir="/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/app.slice/ocserve.service"
  if [ -r "$cg_dir/memory.current" ]; then
    cg=$(cat "$cg_dir/memory.current" 2>/dev/null || true)
    cgp=$(cat "$cg_dir/memory.peak" 2>/dev/null || true)
    main_rss=$(get ocserve_rss_bytes)
    if [ -n "$cg" ] && [ -n "$main_rss" ] && [ "$main_rss" -gt 0 ] 2>/dev/null; then
      kids=$((cg - main_rss))
    fi
  fi
  echo "$ts,$(get ocserve_rss_bytes),$(get ocserve_rss_peak_bytes),$(get ocserve_sidecar_rss_bytes),$(get ocserve_wal_bytes),$(get ocserve_sse_clients),$(get ocserve_writer_queue_depth),$(get ocserve_prompt_locks),$(get ocserve_prompt_tasks),$health,$(get ocserve_prompt_rss_delta_bytes),$(get ocserve_db_opens_total),$(getp ocserve_sync_tick_sum),$(get oc_rss),$cg,$cgp,$kids" >> "$OUT"
  sleep "$INTERVAL"
done
echo "soak done: $OUT"
