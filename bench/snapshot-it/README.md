# bench/snapshot-it — snapshot/revert/vcs-apply integration (SNAPSHOT.md §7.5)

Containerised end-to-end proof of the file-snapshot surface. Two suites:

1. **Single-arm** (`driver.py`) — boots ocserve in-container against a real git
   worktree and asserts the observable effects: `/vcs/apply` changes the file
   bytes on disk; a stub-provider turn's `edit` tool lands on disk and is
   captured as a `patch` part; `revert` restores the pre-turn bytes; `GET
   /session/{id}/diff` is JSON; `unrevert` restores the post-turn bytes.
2. **Differential** (`differential.py`) — runs the same `/vcs/apply` mutation
   against **freeze 1.18.31 and ocserve** under identical fixture worktrees and
   asserts the resulting file bytes are identical (the freeze is the oracle —
   Godefroid ISSTA'20 pair-check applied to the mutation surface). `--selftest`
   sabotages the ocserve bytes and requires the comparator to flag them.

Usage:

```sh
./run.sh              # both suites
KEEP=1 ./run.sh       # keep containers/worktrees for inspection
```

Isolation: containers only; the shared worktrees live under `work/` and
`freeze-work/` (gitignored, ephemeral). No live service ports are touched
(server 4925, freeze 4926, stub 8080 — container-internal only).

Runtime dependency note: the ocserve container installs **git** because the
snapshot engine and `/vcs/*` shell out to it (`bench/parity/Dockerfile.ocserve`).
