#!/usr/bin/env bash
# Fetch locally, execute on the sandbox host.
#
# The sandbox host has no DNS/internet by design (it is a runner, not a
# gateway), so the corpus and the runtime image are built here and copied over.
# That split is deliberate:
#
#   local  — network ON. Fetch the pinned tarballs, install deps with
#            `--ignore-scripts` (no package code runs while the network is up).
#   remote — network OFF. Untrusted plugin code executes with a read-only root,
#            non-root user, tmpfs HOME, memory/CPU/PID caps and a hard timeout.
#
# Usage:
#   scripts/plugin-corpus-remote.sh [--host USER@HOST] [--reuse]
#
#   --reuse  skip re-fetching and re-transferring the corpus (iteration loop).
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${PLUGIN_HOST:-john@192.168.1.227}"
REUSE=0
# Same guard as install.sh/fetch-release.sh: a missing value must name the
# flag, not abort with "$2: unbound variable".
_need_arg() { # $1 = flag, $2 = remaining count
  [ "$2" -ge 2 ] || {
    echo "plugin-corpus-remote.sh: $1 requires a value" >&2
    exit 2
  }
}
while [ $# -gt 0 ]; do
  case "$1" in
    --host) _need_arg "$1" "$#"; HOST="$2"; shift 2 ;;
    --reuse) REUSE=1; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

REMOTE_DIR="ocserve-plugin-check"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST")
IMAGE="oven/bun:1-alpine"
say() { printf '%s\n' "$*"; }

if [ "$REUSE" = 0 ]; then
  say "== fetch locally (network ON, lifecycle scripts OFF) =="
  ( cd bench/plugins && "$HOME/.bun/bin/bun" fetch.mjs )
  say "== package corpus + runtime image =="
  tar -C bench/plugins -cf - .corpus | zstd -3 -q -T0 -o /var/tmp/plugin-corpus.tar.zst -
  docker save "$IMAGE" | zstd -3 -q -T0 -o /var/tmp/plugin-image.tar.zst -
  say "== transfer =="
  "${SSH[@]}" "rm -rf ~/$REMOTE_DIR && mkdir -p ~/$REMOTE_DIR"
  scp -q -o BatchMode=yes /var/tmp/plugin-corpus.tar.zst /var/tmp/plugin-image.tar.zst \
    "$HOST:~/$REMOTE_DIR/"
fi

say "== stage on $HOST =="
"${SSH[@]}" "set -e
  cd ~/$REMOTE_DIR
  if [ -f plugin-image.tar.zst ]; then
    zstd -dc plugin-image.tar.zst | docker load >/dev/null
  fi
  rm -rf bench && mkdir -p bench/plugins crates/ocserve-plugin/src out
  if [ -f plugin-corpus.tar.zst ]; then
    tar -C bench/plugins -xf plugin-corpus.tar.zst
  fi
  chmod 0777 out"
scp -q -o BatchMode=yes bench/plugins/check.mjs bench/plugins/manifest.json \
  "$HOST:~/$REMOTE_DIR/bench/plugins/"
scp -q -o BatchMode=yes -r crates/ocserve-plugin/src/host/. \
  "$HOST:~/$REMOTE_DIR/crates/ocserve-plugin/src/host/"

say "== run (network OFF, read-only, bounded) =="
"${SSH[@]}" "cd ~/$REMOTE_DIR && docker run --rm \
  --network none --read-only \
  --tmpfs /work:rw,exec,nosuid,size=64m,mode=1777 \
  --tmpfs /tmp:rw,exec,nosuid,size=512m \
  --user 65534:65534 \
  --memory 1g --memory-swap 1g --pids-limit 256 --cpus 1.5 \
  --cap-drop ALL --security-opt no-new-privileges \
  -e HOME=/tmp -e XDG_CONFIG_HOME=/tmp/.config -e XDG_DATA_HOME=/tmp/.local/share \
  -v \$PWD:/src:ro -v \$PWD/out:/out:rw -w /work \
  $IMAGE sh -eu -c '
    mkdir -p /work/bench/plugins /work/crates/ocserve-plugin/src
    cp /src/bench/plugins/check.mjs /src/bench/plugins/manifest.json /work/bench/plugins/
    cp -r /src/crates/ocserve-plugin/src/host /work/crates/ocserve-plugin/src/host
    timeout 900 bun bench/plugins/check.mjs --dir /src/bench/plugins/.corpus --out /out/report.json
  '"

say "== collect =="
scp -q -o BatchMode=yes "$HOST:~/$REMOTE_DIR/out/report.json" bench/plugins/report.json
"${SSH[@]}" "rm -rf ~/$REMOTE_DIR/plugin-corpus.tar.zst ~/$REMOTE_DIR/plugin-image.tar.zst ~/$REMOTE_DIR/bench ~/$REMOTE_DIR/crates ~/$REMOTE_DIR/out"
node -e 'const r=require("./bench/plugins/report.json");console.log("summary:",JSON.stringify(r.summary))'
say "report: bench/plugins/report.json"
