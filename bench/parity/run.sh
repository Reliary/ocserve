#!/usr/bin/env bash
# Parity harness driver: preflight → warmup (discarded) →3 interleaved
# measured pairs → report.md. Local tool (no CI in v1).
#
#   ./run.sh                 # full pipeline (~45–60 min)
#   S4_SECONDS=60 ./run.sh   # shorter idle watch
#   WARMUP=0 ./run.sh        # skip warmup (not recommended)
set -euo pipefail
cd "$(dirname "$0")"

MAX_LOAD1="${MAX_LOAD1:-1.5}"
ROUNDS="${ROUNDS:-3}"
# compose interpolates ${ROUND:?} even for build/down — provide a harmless
# placeholder; per-run runner sets the real ROUND (explicit env wins)
export ROUND="${ROUND:-preflight}"

# ---------- preflight ----------
echo "== preflight =="
command -v docker >/dev/null || { echo "docker missing"; exit 1; }
load1=$(awk '{print $1}' /proc/loadavg)
awk -v l="$load1" -v m="$MAX_LOAD1" 'BEGIN{exit !(l<=m)}' \
    || { echo "quiet-host gate: loadavg1 $load1 > $MAX_LOAD1 (MAX_LOAD1= to override)"; exit 1; }
echo "loadavg1=$load1 (gate $MAX_LOAD1)"

if [ ! -x vendor/opencode ]; then
    src=$(readlink -f /home/linuxbrew/.linuxbrew/bin/opencode 2>/dev/null || true)
    [ -n "$src" ] && [ -x "$src" ] || {
        echo "vendor/opencode missing and brew freeze binary not found"; exit 1; }
    echo "staging freeze binary from $src"
    cp "$src" vendor/opencode && chmod +x vendor/opencode
fi
./vendor/opencode --version | grep -q . || { echo "vendor binary won't run"; exit 1; }

docker image inspect parity-upstream parity-stub parity-refine >/dev/null 2>&1 \
    || echo "images missing — building (first run takes a few minutes)"
docker compose -f compose.yaml build

# .runs must exist and be OURS before compose runs: the daemon auto-creates
# missing bind sources as root (hit once — runner then couldn't write results)
mkdir -p .runs
if [ "$(stat -c %u .runs)" != "$(id -u)" ]; then
    echo "fixing .runs ownership (root-owned from docker auto-create)"
    docker run --rm -v "$PWD/.runs:/x" python:3.12-slim \
        chown -R "$(id -u):$(id -g)" /x
fi

docker compose -f compose.yaml down >/dev/null 2>&1 || true   # clear stale stacks

# ---------- runs ----------
seq_pairs=()
if [ "${WARMUP:-1}" = "1" ]; then seq_pairs+=(warm); fi
for i in $(seq 1 "$ROUNDS"); do seq_pairs+=("m$i"); done

for tag in "${seq_pairs[@]}"; do
    for arm in upstream refine; do
        round="${tag}$( [ "$arm" = upstream ] && echo u || echo r )"
        if [ -f ".runs/$round/$arm/result.json" ]; then
            echo "== $round ($arm) already has result.json — skip (resume) =="
            continue
        fi
        echo "== $round ($arm) =="
        ROUND="$round" python3 lib/runner.py --arm "$arm"
    done
done

# ---------- report ----------
python3 lib/report.py
echo
echo "raw rounds under .runs/; report at .runs/report.md"
