# Upgrade-machinery test — `bump.sh` vs real latest releases (2026-10-18)

**Goal:** exercise the upgrade (`bump.sh`) pipeline against the real latest upstream
releases to prove the machinery — **without adopting** anything. Pins stayed frozen
(`FREEZE_VERSION=1.18.31`, `V2_TAG=2.0.28`); all artifacts were produced in a throwaway
worktree (`git worktree add --detach /var/tmp/bumptest HEAD`) and discarded.

Targets (npm-latest at run time):

| line | package | latest | pinned |
|---|---|---|---|
| v2 | `@opencode/cli` | **2.0.32** | 2.0.28 |
| v1 | `opencode-ai` | **1.19.3** | 1.18.31 |

## Verdict

The pipeline **runs and produces correct artifacts**, but shipped with **four real
tooling defects** that were found and fixed in this pass. Detection (nightly drift) was
already working; *adoption* (`bump.sh`) was largely a checklist printer.

## Defects found + fixed

1. **`bump.sh` printed steps 2-4 instead of running them** (`scripts/bump.sh`).
   The header claimed "runs steps 1-4 (mechanical, safe)" but the body ran only the
   `npm pack` acquire step; `extract-spec.sh`, `extract-app.sh`, and `contract-diff.py`
   were `echo`'d as text. **Fixed:** it now invokes all three, with a stable scratch dir
   and `--dry-run` / `--clean`.
   - Sub-defect: the acquire `mktemp` dir was removed by an `EXIT` trap that *also*
     printed "kept for the session" — so step 3's `<cli-binary>` had nothing to point
     at. Fixed: a fixed `OCSERVE_BUMP_SCRATCH` dir, CLI tarball extracted to
     `<scratch>/cli/package/bin/opencode`.

2. **`contract-diff.py` was hardcoded to one cross-line pair** (`V1=1.18.31`,
   `V2=2.0.28`, no argv). A `2.0.28 → 2.0.32` bump would diff the *unchanged* v1↔v2 pair
   and report nothing about what actually changed upstream. **Fixed:** added
   `--v2 <tag>` (cross-line against a non-default asset) and `--delta <prev> <next>`
   (same-line — the bump's real question).

3. **`drift-watch.sh` probed the wrong package.** It ran
   `npm view 'opencode-ai@>=2.0.0'` → E404 → reported "v2 not on npm (pre-release tags
   only)", while `@opencode/cli@2.0.32` is published (PLAN §8). **Fixed:** query
   `@opencode/cli`, validate the version like the v1 probe, render the real note.

4. **`acquire-freeze.sh` ignored a positional version argument.** `bump.sh v1` prints
   `acquire-freeze.sh $VER`, but the script read only `OCSERVE_FREEZE_VERSION`/default —
   so `acquire-freeze.sh 1.19.3` silently staged 1.18.31. **Fixed:** `$1` wins.

## What the (fixed) pipeline produced for 2.0.32

```
step 1  acquire  → protocol + schema + cli(@opencode/cli-linux-x64) tarballs
step 2  regen    → 2.0.32.json: 119 paths / 143 ops / 311 schemas (2.0.28: 118/142)
step 3  pack     → 2.0.32 web pack: 1675 files, 87 MB raw → 25.6 MB OCAP2
                   skew guard (rule 17): PASS
                   pack/manifest consistency (rule 17b): PASS
step 4a cross-line (v1 vs 2.0.32): conflict 0, new-v2 97, renamed 30, superset 1
step 4b delta (2.0.28 → 2.0.32):   ops 142 → 143; identical 139, additive 3,
                                    NEW-V2  POST /api/experimental/voice/session
```

The delta is the machine answer to "what did upstream change": **one new op** (voice
session, experimental) and three additive response changes, **zero conflicts** — i.e.
2.0.32 is a safe superset of 2.0.28 (adoption is a human call, per PLAN §8; not made here).

## Negative controls (prove the pipeline bites)

| planted fault | expected | observed |
|---|---|---|
| `extract-spec.sh 99.0.0 --regen` (bogus tag) | fail, write nothing | exit 1, no `99.0.0.json` |
| `contract-diff.py --delta 2.0.28 9.9.9` (missing asset) | fail | exit 1 |
| `contract-diff.py --v2 9.9.9` (missing asset) | fail | exit 1 |
| `bump.sh v2 99.0.0` (bogus version) | fail at acquire | exit 1 (npm ETARGET) |
| `contract-diff.py --selftest` | planted new-op + planted conflict both detected | PASS |

## Guard added

**Rule 37** (`scripts/check-guards.sh`): static — `bump.sh` must `run` steps 2-4 and
expose `--dry-run`/`--delta`, and `contract-diff.py` must accept `--v2`/`--delta`. The
planted control runs the *same predicate* on a sabotaged copy (removed `extract-spec`
line) and requires it to go false — so the check can't pass vacuously.

## Not done (by instruction / scope)

- **No adoption:** pins unchanged, no committed 2.0.32/1.19.3 assets; the worktree was
  removed and `~/src/ocserve` stayed clean throughout.
- **v1 freeze line:** `acquire-freeze.sh 1.19.3` was exercised (staged correctly in the
  isolated worktree) but the downstream v1 steps (golden-corpus re-record, full oracle
  re-baseline) re-pin the freeze every v1 gate measures against — deferred, as the
  higher-risk line.
- **Steps 5-9** remain operator-driven (need a running server + human D-rows/allowlists).
