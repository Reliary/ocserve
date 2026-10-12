# capdiff — provider-boundary differential

The one boundary every client-facing gate is blind to: **what we send to the
model.** The 2026-10-12 "session won't continue" bug (raw-vs-filtered history)
lived here; so did the system-prompt blurb, the `edit` param names, the missing
`webfetch`/`skill` tools, and the un-templated bash description.

## How it works

Boots the vendored freeze (1.18.31) and ocserve **once each**, under identical
fixture HOMEs with the provider `baseURL` redirected to a local capture sink.
Drives a data-driven scenario matrix as fresh sessions (both arms in parallel)
and diffs, per scenario:

1. **provider-request sequence** — every request (not just the last), title-gen
   classified by its frozen system prompt and dropped;
2. **SSE event-type set** — a missing event class (e.g. `message.part.delta`)
   is red; exact counts are informational (freeze emits redundant updates);
3. **cost** — request bytes / round-trips; ocserve **inflation** is red,
   leaner is reported.

## Files

| File | Role |
|---|---|
| `scenarios.json` | the matrix: each row = ordered steps (`prompt`/`summarize`) |
| `stub_rules.json` | deterministic tool_call rules (keyed on last user text) |
| `allow.txt` | cited `D-*` divergences (path-segment match) |
| `validate.py` | matrix validator (guard rule 27) |

**Extending coverage = a JSON row.** No code.

## Run

```sh
./scripts/capdiff-check.sh --scenario fresh      # one scenario
./scripts/capdiff-check.sh --full                # whole matrix
./scripts/capdiff-check.sh --full --reverse      # determinism (must match)
./scripts/capdiff-check.sh --selftest            # comparator non-vacuity
./scripts/capdiff-check.sh --full --no-parallel  # K-EXT-4 fallback
```

Exit: `0` green · `1` divergence · `2` infra · `3` selftest failed.

## SRE notes

- Fail-fast preflight (infra exit ≠ divergence exit); no invented thresholds —
  report spreads, gate only stable axes.
- Determinism is a gate: run `--full` and `--full --reverse`; captures must
  match (a flaky capture is a defect).
- ocserve exposes `/metrics` (db-opens, rounds, llm requests, rss); freeze's
  `/metrics` is the SPA → `n/a`, never 0 (`D-PARITY-METRICS`).
- Cost/latency **speed** claims live in `bench/parity` + `bench/load`; here the
  byte/round-trip axis is a cost regression guard.

## Kill criteria (pre-registered)

K-EXT-1 summarize payload · K-EXT-2 stub tool loop terminates · K-EXT-3 tool
captures stable in the fixture worktree · K-EXT-4 parallel arms flake.
