#!/usr/bin/env bash
# Build + run zen-probe scenarios. This is the ONLY launcher — everything is
# container-side; host services stay read-only (AGENTS §2 rule 11).
# Usage: ./host.sh s0|s1|r1|r3|boot-check
set -euo pipefail
cd "$(dirname "$0")"
IMAGE=zen-probe:latest

docker build -q -t "$IMAGE" . >/dev/null

# runs/ is written by root-owned containers; heal ownership before host writes.
mkdir -p runs
if ! touch runs/.write-test 2>/dev/null; then
  docker run --rm -v "$PWD:/p" debian:bookworm-slim \
    chown -R "$(id -u):$(id -g)" /p/runs >/dev/null 2>&1 || true
  rm -f runs/.write-test 2>/dev/null || true
fi

# The exact ELF their opencode runs (bun-compiled, glibc >= 2.17).
OC_BIN="${OC_BIN:-$(dirname "$HOME")/linuxbrew/.linuxbrew/bin/opencode}"
[ -x "$OC_BIN" ] || { echo "opencode ELF not found: $OC_BIN" >&2; exit 1; }

mounts=(
  --cap-add NET_RAW --cap-add NET_ADMIN
  -v "$OC_BIN:/opt/oc/opencode:ro"
  -v "$PWD:/probe"
  -v "$HOME/.config/opencode/opencode.json:/mnt/cfg/opencode.json:ro"
  -v "$HOME/.local/share/opencode/auth.json:/mnt/cfg/auth.json:ro"
  -v "$HOME/.cache/opencode/models.json:/mnt/cfg/models.json:ro"
)
# Optional state files — only mount what exists (-v creates dirs otherwise).
for extra in "$HOME/.local/state/opencode/model.json" "$HOME/.local/share/opencode/account.json"; do
  if [ -f "$extra" ]; then
    mounts+=(-v "$extra:/mnt/cfg/$(basename "$extra"):ro")
  fi
done
# Plugin/MCP package cache: opencode hits registry.npmjs.org during session
# create if the cache is absent (observed 6.4 MB+ fetches = hung POST).
# Mounted read-only at the exact runtime path — host cache is never written.
if [ -d "$HOME/.cache/opencode/packages" ]; then
  mounts+=(-v "$HOME/.cache/opencode/packages:/root/.cache/opencode/packages:ro")
fi
# bun — the runtime their ELF is compiled from, for stack-parity replica runs.
if [ -x "$HOME/.bun/bin/bun" ]; then
  mounts+=(-v "$HOME/.bun/bin/bun:/opt/bun:ro")
fi
# `{file:...}` references in opencode.json must resolve inside the container
# (boot fails fast on missing refs). Currently exactly one: ~/nube-api (a file).
if [ -e "$HOME/nube-api" ]; then
  mounts+=(-v "$HOME/nube-api:/root/nube-api:ro")
fi

# --- R2: MITM the working flow through a local CA (plaintext ground truth) ---
if [ "${1:-}" = "r2" ]; then
  shift
  SCEN=("$@")
  if [ ${#SCEN[@]} -eq 0 ]; then SCEN=(s0); fi
  docker network create zenprobe >/dev/null 2>&1 || true
  docker rm -f zenprobe-mitm >/dev/null 2>&1 || true
  mkdir -p runs
  docker run -d --name zenprobe-mitm --network zenprobe \
    -v "$PWD/runs:/dump" mitmproxy/mitmproxy \
    mitmdump --listen-port 8080 --set confdir=/tmp/conf -w /dump/r2.flows >/dev/null
  for i in $(seq 1 30); do
    docker exec zenprobe-mitm true 2>/dev/null && break; sleep 0.5
  done
  sleep 2
  docker exec zenprobe-mitm cat /tmp/conf/mitmproxy-ca.pem > runs/mitm-ca.pem 2>/dev/null || true
  if ! grep -q "BEGIN CERTIFICATE" runs/mitm-ca.pem 2>/dev/null; then
    echo "R2_FAIL no-ca"
    echo "-- exec ls --"; docker exec zenprobe-mitm ls -la /tmp/conf 2>&1 | head -8
    echo "-- logs --"; docker logs zenprobe-mitm 2>&1 | tail -8
    echo "-- ca file --"; ls -la runs/mitm-ca.pem 2>&1; head -c 80 runs/mitm-ca.pem 2>/dev/null; echo
    docker rm -f zenprobe-mitm >/dev/null; exit 1
  fi
  set +e
  docker run --rm --network zenprobe \
    -e HTTPS_PROXY=http://zenprobe-mitm:8080 -e HTTP_PROXY=http://zenprobe-mitm:8080 \
    -e https_proxy=http://zenprobe-mitm:8080 -e http_proxy=http://zenprobe-mitm:8080 \
    -e NO_PROXY=127.0.0.1,localhost -e no_proxy=127.0.0.1,localhost \
    -e SSL_CERT_FILE=/probe/runs/mitm-ca.pem \
    -e NODE_EXTRA_CA_CERTS=/probe/runs/mitm-ca.pem \
    -e CURL_CA_BUNDLE=/probe/runs/mitm-ca.pem \
    "${mounts[@]}" "$IMAGE" "${SCEN[@]}"
  rc=$?
  set -e
  docker rm -f zenprobe-mitm >/dev/null 2>&1 || true
  echo "R2 oc rc=$rc (S0_PASS above = TLS trusted through MITM)"
  exit $rc
fi

exec docker run --rm "${mounts[@]}" "$IMAGE" "$@"
