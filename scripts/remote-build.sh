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
#   --clean            delete the remote target/ after a successful run
#   --prune            prune incremental/scratch before running, then continue
#   --keep             suppress the target-size line (the tree still stays)
#   --print-host       print the resolved host and exit (for other scripts)
#
# Disk discipline (standing rule, 2026-11-24): this box is the ONLY build
# host. A local `target/` reached ~912 GB here and btrfs snapshots pinned the
# deleted extents, so deleting it freed nothing and the machine crashed. Every
# run therefore reports free space, and refuses to start without headroom.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${OCSERVE_BUILD_HOST:-john@100.74.133.54}"
# Absolute by default: /var/tmp is a SEPARATE btrfs subvolume that snapper does
# not snapshot, so a 100 GB target/ can actually be reclaimed by deleting it.
# Under $HOME every build artefact gets pinned by the hourly timeline snapshot
# and a `rm -rf target` frees nothing (measured 2026-11-09: 160 GB deleted,
# free space unchanged, because five snapshots still referenced the extents).
REMOTE_DIR="${OCSERVE_BUILD_DIR:-/var/tmp/ocserve-build}"
JOBS="${OCSERVE_BUILD_JOBS:-16}"
SYNC=1
PULL=()
KEEP=0
CLEAN=0
PRUNE=0
# Refuse to start below this much free space on the build host. A release
# build needs ~8 GB and the test suite ~12 GB of scratch; 60 GB leaves room for
# the co-tenant workload (llama-server) without ever risking its disk.
MIN_FREE_GB="${OCSERVE_BUILD_MIN_FREE_GB:-60}"

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
    --clean) CLEAN=1; shift ;;
    --prune) PRUNE=1; shift ;;
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

# Disk preflight: never start a build that could fill the host. Reported on
# every run so a slow leak is visible before it becomes a crash.
#
# Parsed with awk on the DATA line, not `tr -dc 0-9` over the output: the
# remote login shell is fish and a multi-line df made the digit-strip concatenate
# header and row numbers into a bogus value (measured: "01295469625574G").
free_gb=$(timeout 20 "${SSH[@]}" "df -Pk '$REMOTE_DIR' 2>/dev/null | awk 'NR==2{print int(\$4/1048576)}'" 2>/dev/null | tr -dc '0-9' || true)
if [ -z "$free_gb" ]; then
  free_gb=$(timeout 20 "${SSH[@]}" "df -Pk /var/tmp 2>/dev/null | awk 'NR==2{print int(\$4/1048576)}'" 2>/dev/null | tr -dc '0-9' || true)
fi
if [ -n "$free_gb" ] && [ "$free_gb" -lt "$MIN_FREE_GB" ]; then
  echo "remote-build.sh: $HOST has only ${free_gb}G free (need ${MIN_FREE_GB}G)." >&2
  echo "  free space with: scripts/remote-build.sh --clean <same command>" >&2
  echo "  or raise the floor: OCSERVE_BUILD_MIN_FREE_GB=<n>" >&2
  exit 5
fi
echo ">> $HOST free: ${free_gb:-?}G (floor ${MIN_FREE_GB}G)"

# Scratch prune: incremental artefacts are pure rebuild cost, and the test
# suite leaves temp trees in /var/tmp. Both are regenerable by definition.
if [ "$PRUNE" = 1 ] || [ "$CLEAN" = 1 ]; then
  timeout 600 "${SSH[@]}" "bash -lc 'rm -rf \"$REMOTE_DIR\"/target/*/incremental; find /var/tmp -maxdepth 1 -user \"\$(id -un)\" -mmin +120 -name \"ocserve*\" -exec rm -rf {} + '" 2>/dev/null || true
  echo ">> pruned remote incremental + stale scratch"
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
set +e
timeout 7200 "${SSH[@]}" "bash -lc $(q "mkdir -p '$REMOTE_DIR' && cd '$REMOTE_DIR' && $RUN")"
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
  # `if ! cmd; then rc=$?` reports 0 (the negation's status), which made a red
  # remote run print "exited 0" and exit 0 — a silent green gate.
  echo ">> remote-build.sh: remote command exited $rc" >&2
  exit "$rc"
fi

for path in "${PULL[@]+"${PULL[@]}"}"; do
  [ -e "$path" ] || mkdir -p "$(dirname "$path")"
  rsync -a "$HOST:$REMOTE_DIR/$path" "$path"
  echo ">> pulled $path"
done

if [ "$CLEAN" = 1 ]; then
  # VERIFY, do not announce. `rm -rf ... || true` printed "cleaned" even when
  # the timeout killed it and 42 GB was still there (measured 2026-11-25) —
  # the swallowed-result class this repo bans. A 40 GB btrfs tree can exceed
  # any fixed timeout, so this runs with a generous budget, then asserts the
  # directory is GONE and reports the real free-space delta.
  # MUST go through bash: the remote login shell is fish, and fish ABORTS the
  # whole command line when a glob matches nothing. `rm -rf target mutants.out*`
  # therefore never ran `rm` at all when there was no mutants dir, and the old
  # `2>/dev/null || true` reported success over a 42 GB tree (measured
  # 2026-11-25). No glob at all now: find with -exec.
  timeout 1800 "${SSH[@]}" "bash -lc 'rm -rf \"\$1\"/target; find \"\$1\" -maxdepth 1 -name \"mutants.out*\" -exec rm -rf {} +'" _ "$REMOTE_DIR" >/dev/null 2>&1 || true
  if timeout 20 "${SSH[@]}" "bash -lc 'test -e \"\$1\"/target'" _ "$REMOTE_DIR" 2>/dev/null; then
    echo "remote-build.sh: --clean FAILED, '$REMOTE_DIR/target' still exists (deletion timed out or was refused)" >&2
    exit 6
  fi
  # Our own scratch dirs (the test suite creates thousands per workspace run).
  timeout 600 "${SSH[@]}" "bash -lc 'find /var/tmp -maxdepth 1 -user \"\$(id -un)\" -name \"ocserve-*\" ! -name \"ocserve-build\" -exec rm -rf {} + 2>/dev/null || true'" >/dev/null 2>&1 || true
  leftover=$(timeout 30 "${SSH[@]}" "bash -lc 'find /var/tmp -maxdepth 1 -user \"\$(id -un)\" -name \"ocserve-*\" ! -name \"ocserve-build\" | wc -l'" 2>/dev/null | tr -dc '0-9' || echo 0)
  echo ">> cleaned remote target/ and scratch dirs (${leftover:-0} scratch dirs left)"
elif [ "$KEEP" = 0 ]; then
  size=$(timeout 30 "${SSH[@]}" "du -sh '$REMOTE_DIR/target' 2>/dev/null | cut -f1" 2>/dev/null || true)
  [ -n "$size" ] && echo ">> remote target/ is $size (kept for incremental reuse; --clean to remove)"
fi

free_after=$(timeout 20 "${SSH[@]}" "df -Pk '$REMOTE_DIR' 2>/dev/null | awk 'NR==2{print int(\$4/1048576)}'" 2>/dev/null | tr -dc '0-9' || true)
[ -n "$free_after" ] && echo ">> $HOST free after: ${free_after}G"
