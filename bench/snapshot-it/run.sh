# Snapshot/revert/vcs-apply integration harness (SNAPSHOT.md §7.5).
#
# Boots ocserve inside a container against a real git worktree bind-mounted
# from the host, and drives the full mutation surface read from the host:
#   - POST /vcs/apply mutates a file -> assert the file bytes changed
#   - prompt (stub provider) orders an `edit` -> patch part captured
#   - POST /session/{id}/revert restores the file + slices history
#   - GET /session/{id}/diff?messageID= is JSON
#
# This is the containerised end-to-end proof (host build/run gives the same
# result; the container proves it holds under the deployed glibc/paths).
#
#   ./run.sh        # build + run + report
#   KEEP=1 ./run.sh # keep the worktree for inspection
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

if ! command -v docker >/dev/null; then
  echo "docker missing"; exit 1
fi

# The server arm carries basic auth (F5: auth-off refuses a 0.0.0.0 bind inside
# a compose bridge). The password is generated per run rather than committed —
# a checked-in credential is a checked-in credential even in a harness — and
# the file must be mode 600 or the server correctly refuses to read it.
PWFILE="$HERE/config/password"
umask 077
head -c 24 /dev/urandom | base64 | tr -d '\n=+/' >"$PWFILE"
chmod 600 "$PWFILE"
export SNAPSHOT_AUTH="opencode:$(cat "$PWFILE")"
cleanup_pw() { rm -f "$PWFILE"; }
trap cleanup_pw EXIT

echo "== build ocserve image (in-container, bookworm glibc) =="
docker compose -f "$HERE/compose.yaml" build

# Reset the scratch dirs. They are written by in-container root, so they are
# ROOT-OWNED afterwards and a plain host-side `rm -rf` fails. The alpine helper
# runs as root and CAN delete them — but this used to end in `|| true`, so a
# failed reset left the previous run's state in place and the next run silently
# measured stale data. It also left root-owned dirs inside the repo, which the
# repo-root Docker build then choked on (2026-10-17..27: nightly red with
# "error from sender: permission denied"). Reset failure is now LOUD.
reset_scratch() {
  docker run --rm -v "$HERE:/x" alpine:3 sh -c \
    'rm -rf /x/work /x/data /x/freeze-work && mkdir -p /x/work /x/data /x/freeze-work' \
    >/dev/null || {
    echo "snapshot-it: could not reset work/data/freeze-work" >&2
    echo "  these are written by in-container root; run:" >&2
    echo "  docker run --rm -v \"$HERE:/x\" alpine:3 sh -c 'rm -rf /x/work /x/data /x/freeze-work'" >&2
    return 1
  }
}

reset_scratch

echo "== single-arm integration (server only) =="
docker compose -f "$HERE/compose.yaml" run --rm driver
i1=$?

docker compose -f "$HERE/compose.yaml" down >/dev/null 2>&1 || true
reset_scratch

echo "== freeze vs ocserve differential (selftest then real) =="
docker compose -f "$HERE/compose.yaml" run --rm differential --selftest
i2a=$?
# down before recreating: the server/freeze bind mounts must be released, or
# they keep pointing at the deleted inode and read stale state.
docker compose -f "$HERE/compose.yaml" down >/dev/null 2>&1 || true
reset_scratch
docker compose -f "$HERE/compose.yaml" run --rm differential
i2b=$?
i2=$([ "$i2a" = "0" ] && [ "$i2b" = "0" ] && echo 0 || echo 1)

if [ "${KEEP:-0}" != "1" ]; then
  docker compose -f "$HERE/compose.yaml" down -v >/dev/null 2>&1 || true
fi

echo "== integration exit $i1 / differential exit $i2 =="
[ "$i1" = "0" ] && [ "$i2" = "0" ]
