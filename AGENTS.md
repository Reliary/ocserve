# AGENTS.md — refine

Binding instructions for every agent (and human) working on this repository. The companion
documents are **specification, not suggestion** — code that contradicts them is wrong even if
tests pass.

## 1. Documentation map (read before touching the area)

| Doc | Authority |
|---|---|
| `PLAN.md` | scope, contract freeze, architecture, API surface, harness, milestones, standards (F1–F6, N1–N7), antagonism ledger |
| `STORAGE.md` | every SQLite PRAGMA, schema/blob rule, FTS5 design, storage CI gates |
| `MEMORY.md` | 300 MB budget lines, allocator/runtime settings, allocation rules, memory gates |
| `SRE.md` | fail-fast boot checks, metric-per-KPI table, config knobs, CPU policy, CI/CD, systemd |
| `TESTING.md` | test levels/techniques, traceability matrix, anti-theater rules, adversarial program, entry/exit criteria |
| `COMPACTION.md` | auto-compaction (M6): upstream algorithm line-cited, wire/compat surfaces, schema v8 projection, perf improvements (P1–P8), divergence ledger, test plan |

Precedence when documents disagree: `TESTING.md` gates always win (nothing ships untested);
then `PLAN.md`; then the companion spec for that domain (`STORAGE`/`MEMORY`/`SRE`). Any
resolution that changes a requirement must be committed as an edit to the document — never
resolved silently in code.

**Before writing code:** find the requirement(s) your change implements (usually a § kill
criterion, KPI, or F/N standard) and make sure a test ID exists or is created for it in the
traceability matrix. **Before claiming done:** the milestone exit criteria in `TESTING.md §8`
are the bar — "mostly passing" does not exit.

## 2. Hard rules (CI enforces all of these; violating them blocks the commit)

1. **Anti-theater** (`TESTING.md §1`): bug fixes land test-first with a negative control;
   mutation survivors in `refine-store`/`refine-core`/SSE encoder are must-fix; divergence
   from upstream is a *named exception test*, never a skipped assertion.
2. **No unmeasured claims**: never write a percentage, speedup, or "savings" figure anywhere
   (code, docs, commit messages) that wasn't produced by a committed, repeatable benchmark
   with its kill criteria passing.
3. **Bounded by construction**: every channel, cache, pool, queue, buffer, and intern table
   declares its bound at creation, in code next to the bound. An unbounded collection on a
   request/event/storage path is a defect, not a style choice.
4. **No `unwrap`/`expect`/panic in server paths** (`refine-*` runtime crates): fail fast with
   context (error + route/session), because `panic = abort` turns panics into outages.
   Tests may unwrap.
5. **Storage discipline** (`STORAGE.md`): pragmas only via the shared `open()` routine; never
   re-issue `journal_mode` on a live handle; never hold a connection across `.await`; never
   `SELECT *` on parts/events; never swallow a `PRAGMA`/`execute_batch` result (`let _ =` is
   banned — log and propagate).
6. **Memory discipline** (`MEMORY.md`): no `unbounded_channel` in runtime code; parse cap
   8 MB; no whole-payload materialization > 8 MB; rayon/tokio pools use declared settings,
   not defaults.
7. **Wire compatibility** (`PLAN.md §3`): any change touching HTTP/SSE shapes runs the
   differential replay; golden corpus updates are reviewed diffs in their own commit with
   justification — never silently regenerated.
8. **Secrets**: no credentials in code/logs/metrics; OAuth tokens on disk mode 600; recorded
   corpora pass the secret scanner before commit.
9. **Naming/paths**: no personal names or absolute home paths in any file (use `~`/`$HOME`);
   banned-string check runs pre-commit.
10. **Unsafe**: `forbid(unsafe_code)` per crate unless a crate explicitly opts into a
    reviewed `unsafe` module (justify in the crate README; blob/zstd interop likely candidates).

## 3. Commit strategy

**Style:** conventional commits, one concern per commit, imperative subject ≤72 chars, body
explains *why* + which requirement/kill criterion it serves (reference `PLAN.md` section or
test ID). No `WIP`/`fix stuff` subjects on `main`.

```
feat(store): chunked zstd blob writer with fsync+rename protocol   # PLAN §5, TC-STORE-014
fix(sse): drop subscriber on ring overflow                         # PLAN §4 divergence D-slowclient
test(harness): byte-golden comparison for first SSE frames         # TESTING §6
docs(storage): role-scoped cache sizes                             # MEMORY §1
chore(ci): mutation gate for refine-core                           # TESTING §1
```

**Atomicity rules:**
- Every commit compiles and passes the gates it can run at that point (no broken `main`).
  Docs-only and test-only commits are fine; never commit a failing test *without* marking it
  `#[ignore = "red"]` with an issue reference — red tests live on branches, not `main`.
- Bug fix = **two commits** when practical: (1) failing regression test (+ negative control
  proof in the message), (2) the fix. This keeps bisect and mutation history honest.
- Golden corpus / contract recordings change in isolated commits (`test(corpus): …`) so
  contract drift is reviewable at a glance.
- Generated code (progenitor types) in its own commit with the regeneration command in the
  body.
- Never commit: fixtures with real secrets, `.db` files, recordings > agreed size (store
  under `testdata/` with LFS policy decided at M0), profiler dumps, `target/`.

**Before every commit:** `cargo fmt --check && cargo clippy --workspace -- -D warnings &&
cargo test --workspace && ./scripts/check-guards.sh && ./scripts/check-matrix.sh`
(plus the area's gate: storage → `STORAGE.md §7`, memory → `MEMORY.md §5`, wire →
differential replay). `check-guards.sh` is the static guardrail set (swallowed storage
writes, backslash SQL, session materialization in HTTP — see TESTING §1.6). Pre-commit hook runs the banned-string check
and the docs-required check (changing a behavior without touching the matrix fails).

## 4. Branching strategy

**Model: trunk-based, milestone-gated** — one long-lived `main`, short-lived topic branches,
milestone tags. Optimized for a solo builder + agents: minimal merge overhead, every merge is
a reviewed gate exit, history stays linear and bisectable.

```
main ─────────────●────●───────────●─────●──→  (always releasable at milestone exits)
                  ╲    ╲           ╲    ╱
              feat/x  fix/y     m3/*  feat/z     (topic branches, ≤ days, one concern)
```

1. **`main` is the only long-lived branch.** Protected by hooks: gates must pass, no direct
   pushes of red tests, no force-pushes, no history rewrite. Everything else is disposable.
2. **Topic branches**: `feat/<area>-<slug>`, `fix/<issue>-<slug>`, `test/<slug>`,
   `docs/<slug>`, `spike/<slug>`. Rule: **≤ 3 days of work or rebase** — long branches are
   how divergence from the freeze grows silently. Agents on parallel tasks each get their
   own branch; never two agents on one branch.
3. **Milestone branches** (`m0` … `m5`): only when a milestone needs a stable integration
   lane while topic work continues (M0 harness, M3 import — likely). Cut from `main`, merge
   back only when `TESTING.md §8` exit criteria are green; delete after merge.
4. **Tags**: `m0-exit`, `m1-exit`, … (milestone exits), `contract-1.18.31-rN` (golden corpus
   generations), `v0.x.y` (usable builds, cut from `main` only at milestone exits or later).
   A tag is the rollback unit — the systemd unit keeps the previous tag's binary path.
5. **Merge policy**: squash-merge topic branches (one concern = one `main` commit, keeps
   the requirement↔commit mapping clean); milestone branches merge with a merge commit
   (`--no-ff`) so the exit is a visible, revertable node.
6. **Spikes** (`spike/*`): throwaway, never merged — results come back as docs/tests. If a
   spike graduates, re-implement properly on a topic branch (spike code is not battle-tested).
7. **Rebases over merges on topic branches**: rebase on `main` before merge; conflicts are
   resolved in code review terms (spec wins over local habit).
8. **Release/rollback flow**: `main` → tag → CI builds binary → deploy. Regression found
   after a tag: revert-merge on `main` (or redeploy previous tag) first, root-cause second.
   Never hotfix on a tag.
9. **No remote pressure**: the repo may stay local; when a remote appears, the same rules
   apply as CI-required PRs (gates as required checks). Contract-freeze changes
   (`PLAN.md §3`) are *always* PR-reviewed even solo — a self-review with the diff of
   freeze artifacts attached.

**Sequencing from the ground up (fits the milestones):**
- Day 0: `git init`, this file + the five docs, `chore: initial spec set`, tag `m0-start`.
- M0: harness first, on topic branches; contract/corpus commits isolated; exit = `m0-exit` tag.
- Then M1→M5 in order: no milestone branch cuts from a non-exit point of its predecessor.
- Sidecar/native-profile experiments stay on `spike/*` until their gate (PLAN §10.4,
  MEMORY sidecar boundary) is decided.

## 5. Working agreement for agents

- Small diffs; one requirement at a time; never refactor + feature in one commit.
- Read the doc section for the area first; if the spec is wrong or silent, say so and update
  the doc in the same branch — don't invent silent behavior.
- Run the narrow test first, then workspace gates, before declaring anything finished.
- Report honestly: failing gates are reported as failing (including in commit bodies); no
  "should pass" — pasted results.
- When blocked on an open question in `PLAN.md §15`, stop and ask; don't pick a contract
  behavior by guess.
