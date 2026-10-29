# Mutation baseline — ocserve-store + ocserve-core

First full-scope `cargo-mutants` baseline (TESTING §1 rule 1, K-MUTANTS).

- **Run host:** i9-13900E (32 threads), `CARGO_MUTANTS_JOBS=8`,
  `RUSTFLAGS=-C link-arg=-fuse-ld=mold`, `TMPDIR=/tmp` (32 G tmpfs).
- **Scope:** full test set (`-p ocserve-store -p ocserve-core`, both lib unit
  tests and the store integration binaries). `--lib` is **not** used — see
  "Method" below.
- **Result:** 2330 mutants in **34 min** — 939 caught, 1221 missed, 24 timeout,
  146 unviable → **43.5 % mutation score** (939 / 2160 scored).
- **Gate:** `scripts/check-mutants.py` ceiling = 1221 missed (ratchet down only).

## Correction (2026-10-28): the scope was wrong

The baseline above was produced WITHOUT `--test-workspace true`, so
cargo-mutants ran only each **mutated package's** tests. `ocserve-store`
functions are largely covered by `ocserve-http`'s integration tests (the fork
and revert routes), so the package default reports them as survivors.

Measured directly on 2026-10-28, same host, same tool:

Measured twice, and the two runs DISAGREE. That disagreement is the finding.

| Module | scope | caught / missed | note |
|---|---|---|---|
| `ocserve-store/src/fork.rs` | package default | 0 / 26 | the original baseline |
| `ocserve-store/src/fork.rs` | workspace tests | **13 / 13** | 2026-10-29, post-fix |
| `ocserve-store/src/fork.rs` | workspace tests (first attempt) | 23 / 3 | **RETRACTED**, see below |
| `ocserve-core/src/revert.rs` | workspace tests | 10 / 16 | 2026-10-29, post-fix |

The 23 / 3 figure was published on 2026-10-28 and is **withdrawn**. Every one
of those 23 "caught" mutants was caught by the SAME test:
`sigkill_fuzz_blob_db_protocol`, which at that moment was failing spuriously
because of the non-atomic-migration bug fixed the same day (a kill between a
DML step and its `user_version` stamp left a database that could not boot, so
the crash fuzz aborted on `table session already exists` regardless of the
mutant). A mutation run whose failures come from a broken harness measures the
harness, not the tests. Re-measured after the fix: **13 caught / 13 missed**.

The injected-mutant check still holds and is independent of that bug:
replacing `fork_session`'s body with `Ok(Default::default())` leaves three
`ocserve-http/tests/fork.rs` tests red, so the workspace tests DO cover the
function. The package default is still wrong; it just is not worth 23 catches.

Consequences, stated plainly:

- The per-module `0 %` rows for `fork.rs` and `revert.rs` in the table below
  are an ARTIFACT of the wrong scope, not coverage gaps. Everything else in
  that table was measured at the same scope and is likewise understated.
- The aggregate ceiling (1221 missed) is therefore **loose, not tight**: it was
  ratcheted from a number that over-counted survivors. It is still a valid
  ratchet, but it must not be cited as evidence of coverage quality.
- A full-scope run costs ~130 s per mutant on the 22-thread laptop (measured:
  9 mutants in 20 min), so all 2707 mutants is roughly **100 hours**. It is not
  a nightly job at that cost with the current workspace-wide test scope, and
  nothing in this file claims a completed re-measurement.
- A corrected full-scope baseline is 2707 mutants and did not finish inside
  40 minutes on `CARGO_MUTANTS_JOBS=8`. Until it is re-run to completion and
  committed (with `--test-workspace true`), this document is the older,
  mis-scoped measurement plus this correction rather than a green claim.
- The scope is now mechanical: `scripts/run-mutants.sh` is the single
  invocation, nightly calls it, and guard rule 63 fails the build on any
  `cargo-mutants` invocation that omits the flag. Guard rule 63 also requires
  `fuzz/**` to stay excluded — `fuzz/` is its own workspace, so cargo-mutants
  copied its 17 GB `target/` into the sandbox and aborted with ENOSPC before
  testing a single mutant.

## Method (why these choices)

- `--lib` was tried and **removed**: it makes cargo-mutants drop
  ocserve-store's 14 integration test binaries, so any store function covered
  only by those binaries reads as a survivor. Measured: 933 false "missed" of
  1165 in a single shard. It ALSO silently lands `--lib` on the test *binary*
  because config test-args are appended after the post-`--` args. Full test
  scope is mandatory for an honest score.
- `[profile.mutants] debug = "none"`: link time dominates per-mutant cost and
  debug info is worthless when a mutant is expected to fail.
- `minimum_test_timeout = 45s`: the auto-set cap was ~2× baseline; the
  checkpoint showed test median 4.8 s with ~10 % of mutants hanging a test,
  so the old 120 s cap wasted 25× median on each.
- 24 `Timeout` results are reported but **not** counted as survivors. A mutant
  that hangs is a real signal (infinite loop / deadlock) but it is not a test
  gap; the ceiling budgets the missed count, and a run that flips a timeout to
  missed is triaged rather than re-run.

## Per-file scores (missed / caught / score)

Weakest modules first — these are the survivor backlog (TESTING §6: survivors
in store/llm are must-fix before milestone exit, triaged like a defect).

| File | Missed | Caught | Score |
|---|---|---|---|
| ocserve-core/src/compaction.rs | 96 | 14 | 13 % |
| ocserve-core/src/permission.rs | 29 | 7 | 19 % |
| ocserve-core/src/attempt.rs | 15 | 4 | 21 % |
| ocserve-core/src/prompt.rs | 171 | 55 | 24 % |
| ocserve-store/src/lib.rs | 551 | 186 | 25 % |
| ocserve-core/src/ids.rs | 22 | 12 | 35 % |
| ocserve-store/src/schema.rs | 13 | 12 | 48 % |
| ocserve-store/src/writer.rs | 12 | 12 | 50 % |
| ocserve-core/src/shell.rs | 12 | 16 | 57 % |
| ocserve-core/src/form.rs | 7 | 11 | 61 % |
| ocserve-core/src/coordinator.rs | 6 | 10 | 62 % |
| ocserve-core/src/snapshot.rs | 34 | 56 | 62 % |
| ocserve-store/src/splice.rs | 75 | 129 | 63 % |
| ocserve-core/src/system_prompt.rs | 9 | 16 | 64 % |
| ocserve-store/src/pragma.rs | 9 | 17 | 65 % |
| ocserve-core/src/sift_boundary.rs | 13 | 25 | 66 % |
| ocserve-store/src/blob.rs | 27 | 70 | 72 % |
| ocserve-core/src/event.rs | 16 | 45 | 74 % |
| ocserve-core/src/compact.rs | 37 | 127 | 77 % |
| ocserve-core/src/loop_guard.rs | 14 | 53 | 79 % |
| ocserve-core/src/wire.rs | 1 | 41 | 98 % |
| ocserve-core/src/question.rs | 0 | 12 | 100 % |
| ocserve-core/src/zen.rs | 0 | 9 | 100 % |
| ocserve-store/src/fork.rs | 26 | 0 | 0 % |
| ocserve-core/src/revert.rs | 26 | 0 | 0 % |

`fork.rs` and `revert.rs` at 0 % are the clearest gaps — whole modules whose
mutants no test kills. Next remediation pass targets these plus `compaction.rs`
and `permission.rs`.

The committed `mutants.out/` is gitignored; only this report and the outcomes
JSON (`bench/mutants/baseline-outcomes.json`, slimmed to summary + diff_path
per mutant — the full per-mutant logs/phases are not retained) are kept for
reproduction.
