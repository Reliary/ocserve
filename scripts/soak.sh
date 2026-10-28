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

# Receipt for `ocserve doctor` (scripts/lib/gate-status.sh): a sampler is
# judged by the freshness of this CSV and the health code on its newest row,
# so a soak that quietly stops writing is NOT SAMPLING rather than green.
# shellcheck source=scripts/lib/gate-status.sh
. "$(dirname "$0")/lib/gate-status.sh"
gate_sampler soak "$OUT" "$INTERVAL"
end=$(( $(date +%s) + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$end" ]; do
  ts=$(date +%s)
  m=$(curl -fsS --max-time 5 "$URL/metrics" || true)
  # `-w` already prints 000 on a connection failure, so `|| echo 000` appended a
  # SECOND one: a dead probe was recorded as `000000` (2026-10-26, seen during a
  # deliberate restart). Fail-closed either way, but the column must be one code.
  health=$(curl -so /dev/null -w '%{http_code}' --max-time 5 "$URL/global/health" 2>/dev/null || true)
  [ -n "$health" ] || health=000
  # Read the whole metric body: an `awk ... exit` here closed the pipe while
  # the producer was still writing, and under `set -o pipefail` the resulting
  # SIGPIPE killed the sampler mid-run (2026-10-27 08:46: "echo: write error:
  # Broken pipe" after 8h46m). Print the first match instead of exiting early.
  get() { printf '%s\n' "$m" | awk -v k="$1" '$1==k && !seen {print $2; seen=1}'; }
  # labeled series (durations render name_sum{label}) — prefix match
  getp() { printf '%s\n' "$m" | awk -v k="$1" 'index($1, k) == 1 && !seen {print $2; seen=1}'; }
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
  # `sync_us` used to be written from a metric that never existed (the column
  # was empty in 246/246 rows). Read the real histogram-free counter if present;
  # blank otherwise, never a fake zero.
  # No sync-timing metric exists; leave the column blank rather than invent
  # one. (The older `$BASE` was an undefined variable under `set -u`.)
  sync_us=""
  echo "$ts,$(get ocserve_rss_bytes),$(get ocserve_rss_peak_bytes),$(get ocserve_sidecar_rss_bytes),$(get ocserve_wal_bytes),$(get ocserve_sse_clients),$(get ocserve_writer_queue_depth),$(get ocserve_prompt_locks),$(get ocserve_prompt_tasks),$health,$(get ocserve_prompt_rss_delta_bytes),$(get ocserve_db_opens_total),$sync_us,$(get oc_rss),$cg,$cgp,$kids" >> "$OUT"
  sleep "$INTERVAL"
done
echo "soak done: $OUT"
