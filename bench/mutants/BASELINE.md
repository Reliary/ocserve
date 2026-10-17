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
