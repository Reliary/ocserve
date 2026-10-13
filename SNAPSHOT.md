# SNAPSHOT.md — file snapshots, revert, and `/vcs/apply`

Authority for the snapshot/revert/vcs-apply surface. Precedence: `TESTING.md` gates >
this doc > `PLAN.md`. Disagreements are resolved by editing the document, never silently
in code (`AGENTS.md §1`).

## 1. Why this exists

A mutation endpoint that returns a success shape without performing the mutation is a
data-integrity lie, not a missing feature. Before this work ocserve had two:

- `POST /vcs/apply` always returned `400 VcsApplyError reason:"not-clean"` — a *fabricated*
  reason (it never attempted an apply). Upstream runs `git apply` and only returns
  `non-git`/`not-clean` on real failure (`project/vcs.ts:399-416`).
- `POST /session/{id}/revert` wrote an inert marker: files were never restored,
  `GET /session/{id}/diff` was `[]`, and the `session.revert.*` events the web UI refreshes
  on were never emitted. `revert_commit` was a literal `Ok(NoContent)`.

This document specifies the faithful port of upstream's snapshot engine
(`packages/opencode/src/snapshot/index.ts`), the revert flow
(`packages/opencode/src/session/revert.ts`, `.../summary.ts`), and a mechanical guard so
the "shape without side effect" bug class cannot return (§8).

## 2. Upstream mechanism (line-cited)

A **shadow git repository** per (project, worktree) lives at
`<global.data>/snapshot/<projectID>/<hash.fast(worktree)>`. All snapshot git commands run
as `git --git-dir=<shadow> --work-tree=<worktree> …`:

- `track()` — ensure the shadow repo exists (init + config tuning + **seed**), `git add
  --all` the changed/untracked candidate set, `git write-tree` → a tree hash (the snapshot
  ID). (`snapshot/index.ts` `track`/`add`/`stage`/`seed`.)
- `seed()` — share the source repo's object database via `objects/info/alternates` and copy
  its index, so already-hashed blobs are reused (huge repos: `git add --all` otherwise
  rebuilds hashes for minutes). Config: `core.autocrlf=false`, `core.longpaths=true`,
  `core.symlinks=true`, `core.fsmonitor=false`, `feature.manyFiles=true`,
  `index.version=4`, `index.threads=true`, `core.untrackedCache=true`.
- Ignore rules: candidates that the **source** repo's `git check-ignore --no-index` marks
  ignored are `git rm --cached`'d out of the shadow index and never staged; untracked files
  larger than 2 MiB are excluded via `info/exclude`.
- `patch(hash)` — `add()` then `git diff --cached --name-only <hash> -- .` → the changed
  file list since `hash` (absolute paths), ignored files filtered out.
- `restore(snapshot)` — `git read-tree <snapshot>` + `git checkout-index -a -f`.
- `revert(patches)` — for each `patch` part's `{hash, files}`, `git checkout <hash> -- <file>`;
  a file absent from `<hash>` is **deleted**; adjacent same-hash files are batched unless
  their paths clash (100-file batches). (`snapshot/index.ts` `revert`.)
- `diff(hash)` / `diffFull(from,to)` — `git diff --cached <hash>` text; `diffFull` produces
  `FileDiff[]` (`{file,patch,additions,deletions,status}`) via `--name-status` +
  `--numstat` + a batched `git cat-file --batch` content load, ignored files filtered.
- `cleanup()` — `git gc --prune=7.days` hourly.
- Non-git project or `snapshot:false` config ⇒ `track()` returns `undefined` (no-op). All
  snapshot work is serialized by a per-gitdir semaphore.

**Capture points** (`session/processor.ts`): `track()` BEFORE the model stream
(`:102`), then after each tool round `patch(ctx.snapshot)` → a `{type:"patch", hash, files}`
part (`:470`,`:555`) and `ctx.snapshot = track()`; the initial snapshot is stored on the
assistant message's `step-start` part and the post-turn one on `step-finish`.

**Revert** (`session/revert.ts`): find the target `{messageID, partID?}` by walking messages;
`rev.snapshot = session.revert?.snapshot ?? track()`; if a snapshot already existed,
`restore` it; `snap.revert(patches)` (the `patch` parts); `rev.diff = snap.diff(rev.snapshot)`;
`summary.computeDiff` over the message range → persisted per-user-message `summary.diffs`
and session `summary{additions,deletions,files}`; publish `session.diff`; `setRevert`.

**Unrevert** (`session/revert.ts`): `restore(rev.snapshot)` + `clearRevert`.

**Cleanup** (`session/revert.ts` `cleanup`, called from `session/prompt.ts:459/1056`): delete
messages from the revert target onward (or from `partID`) and clear the marker — applied at
the start of the next prompt.

## 3. Divergences from upstream (named, deliberate)

| ID | Divergence | Reason |
|---|---|---|
| D-SNAP-RUST-GIT | Snapshot uses the host `git` binary via `std::process::Command` (like `vcs.rs`), not libgit2. | upstream also shells to git; no new dependency. |
| D-SNAP-NOOP-NONGIT | Non-git project ⇒ snapshot disabled (no-op), so revert does not restore files there. | upstream parity (`location.vcs?.type !== "git"`). |
| D-SNAP-GC-PROC | `gc --prune=7.days` runs on an in-process hourly tick, not an Effect-scoped fiber. | no Effect runtime. |
| D-REVERT-EVENTS | The inner `session.revert.*` payload is `{sessionID, info}` (our shape); the outer event shape matches every other event (`{type,properties}`). The app only reads `event.type` to trigger a resync and derives the inner type from the event name. | observed client behavior (`app/.../server-session.ts:979`). |

D-REVERT-NOSNAP and D-VCS-NOAPPLY are **removed** by this work.

## 4. Configuration & bounds

- Default **on** (upstream default). Opt-out: config `"snapshot": false` (upstream key) or
  env `OCSERVE_SNAPSHOT=0`.
- Untracked file cap 2 MiB; ignored-file filter on capture and revert; `diffFull` content
  load batched (`cat-file --batch`).
- Metric `ocserve_snapshot_ops_seconds{op}` (histogram) + `ocserve_snapshot_capture_total{result}`.
- All git invocations run off the tokio workers (`spawn_blocking`); history/DB work obeys
  the standing storage/discipline rules (`AGENTS.md §2`).

## 5. `/vcs/apply`

`git apply -` with the patch on stdin in the session directory. Non-git ⇒
`VcsApplyError reason:"non-git"`; non-zero exit ⇒ `reason:"not-clean"`; success ⇒
`200 {applied:true}`. Patch size capped. The route **does** mutate the worktree, and that is
asserted (`SNAPSHOT §7`).

## 6. Surfaces changed

| Route | Change |
|---|---|
| `POST /session/{id}/revert` | real: busy-gate, restore files, `rev.diff`, `summary.diffs`, `session.diff`, marker |
| `POST /session/{id}/unrevert` | real: restore snapshot, clear marker |
| `GET /session/{id}/diff?messageID=` | real per-message `summary.diffs` (was `[]`/echo) |
| `POST /api/session/{id}/revert/stage\|clear\|commit` | real; `commit` removes the range; emit `session.revert.*` |
| `POST /vcs/apply` | real `git apply` |
| prompt start | `revert.cleanup` inside the write txn |
| prompt loop | capture `step-start`/`step-finish` snapshots + `patch` parts |

## 7. Test plan (truth of the side effect, not the shape)

1. **Engine unit tests** (temp git repo): track/diff/restore/revert — modified, added,
   deleted files; subdir scoping; ignored files never resurrected; 2 MiB cap; unicode paths;
   seed reuse; non-git no-op.
2. **E2E against a real git repo with the stub provider** (`compaction_e2e.rs` pattern):
   - turn mutates a file → the assistant message carries a `patch` part with that file;
   - revert → **file bytes on disk are the pre-turn content**, messages after the target are
     gone, `session.revert.staged` emitted, `GET /diff?messageID=` non-empty;
   - unrevert → file bytes restored to the post-turn content;
   - each with a planted negative control (capture disabled ⇒ no patch part / no restore).
3. **`/vcs/apply` E2E**: POST a modifying patch → 200 and the file changed on disk; POST a
   conflicting patch → `not-clean` and file unchanged; non-git dir → `non-git`.
4. **Side-effect differential** (`bench/sidefx`): boot freeze + ocserve under identical
   fixture git worktrees, run a mutating scenario, compare the resulting **worktree state
   (path→sha256) and message graph** between arms. Non-vacuous self-test plants a divergence.
5. **Containerized integration**: `bench/snapshot-it/` docker-compose — a repo fixture, both
   servers, the differential driver; runs in CI-as-local (no hosted runner).
6. Standing gates + perf gate re-run (snapshot adds per-turn git cost — measured).

## 8. Class closure (guard rule 29)

Every OpenAPI operation with a mutating method (POST/PUT/PATCH/DELETE) must be either in a
read-only allowlist or referenced by a `bench/sidefx` scenario. A planted shape-only route
must turn the guard red (non-vacuous). This makes the "returns a success shape, mutates
nothing" class a build failure for any current or future endpoint, because the differential
compares observable effects rather than a hand-maintained route list.

## 9. Antagonism ledger

| # | Attack | Disposition |
|---|---|---|
| A1 | per-turn `git add --all` expensive on huge repos / blocks prompt start | upstream also blocking; seed+alternates+tuning; `snapshot:false`; measured by histogram + perf gate. Measured 2026-10-13 on this repo (408 files): `track()` ≈4 ms warm, ≈128 ms cold (first shadow-repo seed), via `ocserve_snapshot_ops_seconds{op="track"}` |
| A2 | destructive message removal deletes wrong rows | upstream-exact index math; partID-vs-full boundary tests; marker cleared only after success |
| A3 | `git apply` from HTTP mutates user tree / path escape | confined to session dir; git rejects out-of-tree; size cap; side-effect test both ways |
| A4 | stage→prompt race | busy-gate 409 on stage/clear/commit; cleanup under the prompt lock in one txn |
| A5 | snapshot DB growth | hourly `gc --prune=7.days` |
| A6 | diffFull spawns hundreds of `git show` | batched `cat-file --batch` |
| A7 | ignored files resurrected | `check-ignore` filter on capture + revert |
| A8 | wrong `reason` still fabricated | real apply; both reasons reachable and tested |
| A9 | guard vacuous | rule 29 planted shape-only route must go red |
| A10 | revert on a session with no snapshot (never prompted) | `track()` at revert time (upstream `… ?? track()`); test |
| A11 | concurrent prompts on one session | existing per-session prompt lock; revert takes the lock (busy→409) |
| A12 | worktree is deleted between capture and revert | git ops are best-effort; failure logs + does not panic (server paths never panic) |
| A13 | snapshot of a worktree with no commits (fresh git init) | `write-tree` works with no HEAD; test |
| A14 | `diffFull` on binary files | `--numstat` `-` ⇒ additions/deletions 0, patch `""` (upstream parity) |
| A15 | large patch to `/vcs/apply` | size cap → `not-clean` |
| A16 | restore when files were removed post-capture | `checkout-index -a -f` recreates; `revert` deletes files absent from the snapshot |
