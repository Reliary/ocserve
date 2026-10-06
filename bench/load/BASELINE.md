# Load baseline — refine vs upstream freeze (.227 runner, 2026-10-06)

**Run**: `bench/load/.runs/20261006T225404Z` (report regenerated locally with
the v2-aware report.py). Environment: `cachyos-x8664`, i7-1165G7 4C/8T
homogeneous, pinning `refine=[0,4] freeze=[1,5] k6=[2,6]` (1 whole physical
core each), quiet gate 2.5 passed (load 0.00), ROUNDS=2 interleaved
(r1 refine→freeze, r2 freeze→refine), real fixture: 201 sessions /
48,505 msgs / 178,141 parts + the 32k-msg deep session, zero provider
traffic. Co-tenants: `llama-server` 4.5 GB resident (MemAvailable min
0.8 GB during run; guard refused <1.5 GB at start with operator override
MIN_MEM_KB=786432 recorded in the run log).

## Headline (n=1: capacity + error-rate valid; latency deltas INDICATIVE)

| | freeze (1.18.31) | refine | note |
|---|---:|---:|---|
| error rate | **0.0000** (all 4 runs) | **0.0000** (all 4 runs) | after session_status fix |
| throughput | 424–454 req/s | 53–60 req/s | ~7–8× at 25 VUs |
| pooled p50 | 22.7–25.4 ms | **10.1–12.9 ms** | refine faster at the median |
| pooled p95 | 91.8–103.9 ms | 1198–1256 ms | refine's tail is the gap |
| peak RSS | 662.9 MB | **284.6 MB** | refine leaner |
| CPU (window) | 660 s | 1068 s | whole-sample window, both r's |

## Where refine's tail lives (per-endpoint p95, informational)

- **search** 1357–2462 ms (freeze 69–76 ms) — largest gap
- **message_page / message_cursor** ~1200 ms (freeze 84–114 ms)
- **config** ~1000–1105 ms (freeze 74–80 ms)
- **session_list** 75–554 ms (spread mode worse)
- agent / command / file_list / session_status: 23–132 ms — at parity

## Confounds kept honest (read before citing)

1. **n=1 latency**: claim classes in every report — rerun with `ROUNDS>=3`
   before publishing arm-to-arm latency deltas.
2. **Storage-cache asymmetry**: same data, different representation —
   freeze's native db is 485 MB (fits the ~0.8–1.0 GB available page
   cache next to llama-server), refine's schema is 1.7 GB (mostly cold
   reads). Part of the tail is page-cache misses; this is also a *real*
   product property (storage efficiency → cache fit) but it is not a
   pure code-path measurement.
3. Fixture lever 3 (global-project rewrite, cwd=/) verified 201/201
   before every run; sessions are real user data (ephemeral, `--clean`).

## Lessons from runs 1–2 (why they are not the baseline)

- run1: all 8 k6 invocations died at `new Trend` (k6 v2 moved Trend to
  `k6/metrics`; barrel export is null) — zero data.
- run2: 10.8% "failures" on refine were **both arms lacking the
  per-session status API** — refine 404s honestly, freeze 200s SPA
  catch-all HTML (status-code checks passed on an error page); endpoint
  corrected to the global `GET /session/status`.
