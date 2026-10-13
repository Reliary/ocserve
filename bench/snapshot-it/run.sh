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

echo "== build ocserve image (in-container, bookworm glibc) =="
docker compose -f "$HERE/compose.yaml" build

# work/freeze-work are shared with in-container root; recreate as root-owned.
docker run --rm -v "$HERE:/x" alpine:3 sh -c \
  'rm -rf /x/work /x/data /x/freeze-work && mkdir -p /x/work /x/data /x/freeze-work' >/dev/null 2>&1 || true

echo "== single-arm integration (server only) =="
docker compose -f "$HERE/compose.yaml" run --rm driver
i1=$?

docker compose -f "$HERE/compose.yaml" down >/dev/null 2>&1 || true
docker run --rm -v "$HERE:/x" alpine:3 sh -c \
  'rm -rf /x/work /x/data /x/freeze-work && mkdir -p /x/work /x/data /x/freeze-work' >/dev/null 2>&1 || true

echo "== freeze vs ocserve differential (selftest then real) =="
docker compose -f "$HERE/compose.yaml" run --rm differential --selftest
i2a=$?
# down before recreating: the server/freeze bind mounts must be released, or
# they keep pointing at the deleted inode and read stale state.
docker compose -f "$HERE/compose.yaml" down >/dev/null 2>&1 || true
docker run --rm -v "$HERE:/x" alpine:3 sh -c \
  'rm -rf /x/work /x/data /x/freeze-work && mkdir -p /x/work /x/data /x/freeze-work' >/dev/null 2>&1 || true
docker compose -f "$HERE/compose.yaml" run --rm differential
i2b=$?
i2=$([ "$i2a" = "0" ] && [ "$i2b" = "0" ] && echo 0 || echo 1)

if [ "${KEEP:-0}" != "1" ]; then
  docker compose -f "$HERE/compose.yaml" down -v >/dev/null 2>&1 || true
fi

echo "== integration exit $i1 / differential exit $i2 =="
[ "$i1" = "0" ] && [ "$i2" = "0" ]
