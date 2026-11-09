#!/usr/bin/env bash
# Offload cargo work to the build box.
#
# Why: the laptop's target/ grew to ~912 GB on a 90%-full filesystem, so
# linking and test-binary builds fail with ENOSPC far more often than with a
# compiler error. The build box is a homogeneous 32-thread box with mold,
# sccache and clang. This script makes it the cargo host for one command:
#
#   scripts/remote-build.sh test --workspace
#   scripts/remote-build.sh clippy --workspace --all-targets -- -D warnings
#   scripts/remote-build.sh build --release
#   scripts/remote-build.sh gates            # guards + docs + matrix
#
# The tree is rsynced to the host, the command runs there, and the exit status
# is returned verbatim, so this is safe to use in place of a local run.
#
# Deliberate properties:
#   * The rsync EXCLUDES build artefacts and multi-GB benches. bench/parity/
#     .runs* holds SQLite DBs from past rounds (174 MB) and bench/load holds
#     the load-test fixture; neither is an input to `cargo test`.
#   * `--delete` is used so a file removed locally cannot linger and be
#     compiled remotely. The remote directory is OURS alone.
#   * `.git` IS synced: build.rs stamps the commit and ~10 suites shell out to
#     `git`. Without it a remote binary would report `unknown`.
#   * No network is required for the build itself; cargo may fetch crates on
#     the first run (the box has crates.io access) and reuses them after.
#   * The remote's ~/.cargo/config.toml supplies sccache + mold + native CPU;
#     `-j` is capped so the co-tenant `llama-server` is never starved.
#
# Flags:
#   --host USER@HOST   build host (default: $OCSERVE_BUILD_HOST or the
#                      configured default)
#   --jobs N           cargo -j (default: 16 on a 32-thread box)
#   --no-sync          skip rsync (iterate on the same tree)
#   --pull PATH...     copy result paths back after a successful run
#   --keep             leave the remote tree in place (default: it stays
#                      anyway; this flag only suppresses the size report)
#   --print-host       print the resolved host and exit (for other scripts)
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${OCSERVE_BUILD_HOST:-john@100.74.133.54}"
REMOTE_DIR="${OCSERVE_BUILD_DIR:-ocserve-build}"
JOBS="${OCSERVE_BUILD_JOBS:-16}"
SYNC=1
PULL=()
KEEP=0

_need_arg() { # $1 = flag, $2 = remaining count
  [ "$2" -ge 2 ] || {
    echo "remote-build.sh: $1 requires a value" >&2
    exit 2
  }
}

while [ $# -gt 0 ]; do
  case "$1" in
    --host) _need_arg "$1" "$#"; HOST="$2"; shift 2 ;;
    --jobs) _need_arg "$1" "$#"; JOBS="$2"; shift 2 ;;
    --no-sync) SYNC=0; shift ;;
    --keep) KEEP=1; shift ;;
    --pull)
      shift
      while [ $# -gt 0 ] && [ "${1#--}" = "$1" ]; do PULL+=("$1"); shift; done
      ;;
    --print-host) echo "$HOST"; exit 0 ;;
    --) shift; break ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done

[ $# -gt 0 ] || {
  echo "usage: scripts/remote-build.sh <cargo-verb|gates> [args...]" >&2
  exit 2
}
VERB="$1"; shift

SSH=(ssh -o BatchMode=yes -o ConnectTimeout=15 "$HOST")

# Fail fast with a clear message instead of a 5-minute rsync that then dies.
if ! timeout 20 "${SSH[@]}" true 2>/dev/null; then
  echo "remote-build.sh: cannot reach $HOST over ssh" >&2
  exit 3
fi

# The gates need tools the remote may lack; check the verbs that use them.
REMOTE_TOOLS_OK=1
if [ "$VERB" = "gates" ]; then
  missing=$(timeout 30 "${SSH[@]}" 'bash -lc "for t in python3 rg grep git; do command -v \$t >/dev/null || printf \"%s \" \$t; done"' 2>/dev/null || echo "ssh-failed")
  if [ -n "$missing" ]; then
    echo "remote-build.sh: gates need these on $HOST: $missing" >&2
    REMOTE_TOOLS_OK=0
  fi
fi

if [ "$SYNC" = 1 ]; then
  echo ">> syncing to $HOST:$REMOTE_DIR"
  rsync -a --delete --info=stats1 \
    --exclude='/target/' \
    --exclude='/.safety/' \
    --exclude='/mutants.out*/' \
    --exclude='/fuzz/target/' \
    --exclude='/fuzz/corpus/' \
    --exclude='/bench/load/.fixtures/' \
    --exclude='/bench/normalizer-spike/target/' \
    --exclude='/bench/parity/.runs*/' \
    --exclude='/bench/parity/vendor/' \
    --exclude='/bench/zen-probe/runs/' \
    --exclude='/bench/drift/cron.log' \
    --exclude='/testdata/sessions20/' \
    --exclude='*.rcgu.o' \
    --exclude='/rust_out.*' \
    --exclude='/.wrangler/' \
    ./ "$HOST:$REMOTE_DIR/"
fi

# The remote's shell is fish; every command goes through bash -lc so shell
# syntax (env prefixes, pipes, &&) behaves the same as locally.
q() { printf '%q' "$1"; }
CMD=""
for arg in "$@"; do CMD="$CMD $(q "$arg")"; done

# The embedded commit must describe THIS checkout, not the rsync copy: git's
# symbolic HEAD plus an rsync'd index can report "dirty" on the remote even
# when the local tree is clean. CI passes OCSERVE_BUILD_COMMIT for the same
# reason, so the offload path uses it too.
BUILD_COMMIT="$(git rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
  BUILD_COMMIT="$BUILD_COMMIT-dirty"
fi
export BUILD_COMMIT

case "$VERB" in
  gates)
    if [ "$REMOTE_TOOLS_OK" != 1 ]; then
      echo "remote-build.sh: refusing to run gates with missing tools" >&2
      exit 4
    fi
    # Gate-only python dependency (spec/event validators). A venv keeps the
    # remote system interpreter untouched; created on first use.
    RUN="test -x ~/.venvs/ocserve/bin/python || { uv venv ~/.venvs/ocserve >/dev/null && uv pip install --python ~/.venvs/ocserve/bin/python jsonschema >/dev/null; };"
    RUN="$RUN export PATH=\"$HOME/.venvs/ocserve/bin:\$PATH\";"
    RUN="$RUN TMPDIR=/var/tmp ./scripts/check-guards.sh && TMPDIR=/var/tmp ./scripts/check-matrix.sh && python3 scripts/check-docs.py"
    ;;
  test|build|clippy|run|check|fmt|nextest)
    # `test` and `clippy` use the workspace-wide test scope; keep TMPDIR off
    # the remote /tmp only if it is small (it is 32 GB there, so use it, but
    # the harnesses assume /var/tmp exists — both do on this host).
    # A caller's own --offline is honoured as-is (no duplicated flag); the
    # fallback drops it exactly once so a cold cache can fetch.
    OFFLINE_ARGS="--offline"
    case " $CMD " in *" --offline "*) OFFLINE_ARGS="" ;; esac
    RUN="OCSERVE_BUILD_COMMIT=$BUILD_COMMIT TMPDIR=/var/tmp cargo $VERB $OFFLINE_ARGS -j$JOBS$CMD"
    RUN="$RUN || { echo '>> offline build failed; retrying with network'; OCSERVE_BUILD_COMMIT=$BUILD_COMMIT TMPDIR=/var/tmp cargo $VERB -j$JOBS$(printf '%s' "$CMD" | sed 's/ --offline//g'); }"
    ;;
  *)
    RUN="OCSERVE_BUILD_COMMIT=$BUILD_COMMIT TMPDIR=/var/tmp cargo $VERB -j$JOBS$CMD"
    ;;
esac

echo ">> $HOST: $VERB"
# shellcheck disable=SC2029  # intentional: the command is expanded locally
if ! timeout 7200 "${SSH[@]}" "bash -lc $(q "cd ~/$REMOTE_DIR && $RUN")"; then
  rc=$?
  echo ">> remote-build.sh: remote command exited $rc" >&2
  exit "$rc"
fi

for path in "${PULL[@]+"${PULL[@]}"}"; do
  [ -e "$path" ] || mkdir -p "$(dirname "$path")"
  rsync -a "$HOST:$REMOTE_DIR/$path" "$path"
  echo ">> pulled $path"
done

if [ "$KEEP" = 0 ]; then
  size=$(timeout 30 "${SSH[@]}" "du -sh ~/$REMOTE_DIR/target 2>/dev/null | cut -f1" 2>/dev/null || true)
  [ -n "$size" ] && echo ">> remote target/ is $size (left in place for incremental reuse)"
fi
