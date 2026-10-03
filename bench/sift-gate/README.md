# sift-at-boundary gate — Arc49-style interleaved battery (P1c)

Pre-registered criteria. **This file is written BEFORE any run**; results may
not change these criteria (anti-fitting, TESTING §1).

## Conditions

- **baseline**: `REFINE_SIFT` unset (current default: off)
- **gate**: `REFINE_SIFT=auto` (reliary sift --stdin at the provider boundary)

Model: `opencode-go/mimo-v2.6-flash` (both conditions). Same host, same
driver, same prompts, same fixtures. Runs are **interleaved** (standing
2.7×-variance rule): for each task×seed, baseline and gate run back-to-back,
with first-mover alternating by `(task+seed) % 2`.

## Battery

3 tasks × 3 seeds × 2 conditions = 18 runs. One fresh refine process per run
(cwd = pristine fixture copy, throwaway data dir, port unique). A run is one
`prompt_async` (the agent tool-loop runs inside it).

| task | engagement surface | scoring (mechanical, post-session) |
|---|---|---|
| T1 fix-tests | `python3 -m unittest -v` fails: 7.2 KB (67 tests) | fraction of the **5 pristine-failing tests** (frozen at battery start: test_validate_empty, test_validate_missing_key, test_validate_none_value, test_user_age_none, test_table_short_row) that pass after the session + `test_pipeline.py` hash unchanged (edit-the-tests = 0) |
| T2 report-facts | `python3 t2_dump.py`: 110 KB stdout (prompt instructs: run directly, do not pipe/filter — task property, identical both conditions) | final assistant text contains `total=2500`, `RETRY=26`, `ERROR=11` (1/3 each) |
| T3 crash-fix | `python3 app.py data.jsonl` crashes after 92 KB progress output | 4 checks × ¼: process exit 0, `records=1500`, `FAIL=300`, `OK=1200` (`total_latency` excluded: skip-vs-zero latency fix semantics are both legitimate) + `data.jsonl` hash unchanged (edit-the-data = 0) |

Score per run ∈ [0,1] = mean of its checks. No run is excluded except
pre-registered infra failure (boot timeout / provider error): retried once,
both attempts logged.

## Gate (pass = all four)

1. **Engagement**: gate condition compresses ≥1 tool output in ≥ 2/3 of gate
   runs (otherwise the battery did not exercise the feature → INVALID, not pass).
2. **Cost**: total weighted tokens `input + 4×output` (session-row totals,
   standing weighted-cost rule) gate ≤ 0.85 × baseline (≥15% drop).
   Per-run paired ratios reported regardless of pass/fail.
3. **Score**: median score(gate) ≥ median score(baseline) (non-regression).
4. **No silent blowups**: zero panics/restarts; every run's sift outcome
   (`compressed`/`raw`) and byte totals recorded from `/metrics`.

If gate passes → flip `REFINE_SIFT` default to `auto` in a follow-up commit
with tests updated. If it fails → default stays off, results committed as-is.

## Known limits (recorded, not criteria)

- n=3 per cell: paired ratios reported, medians are coarse; this is a
  pre-registered screening gate, not a publication-grade estimate.
- Provider-side prefix cache is shared across conditions (same account);
  interleaving is the control, not isolation.
- Score checks are ground-truth mechanical — no LLM judge.
