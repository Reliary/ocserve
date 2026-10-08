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

---

# Phase-III campaign addendum — 2026-10-07 (PERF-10X)

Same runner/fixture/method as above unless noted. **New in this campaign:**
allocation `LOAD_REF_CORES=0,4,3,7` (refine gains the previously
unallocated physical core 3; freeze keeps [1,5], k6 keeps [2,6] — per-core
comparisons remain the fair view), final binary at commit 7f48949 (S-A,
F1, F3/F4, F5, F7/F8, F9/F9c, B1, temp_store FILE; C1/C4 reverted after a
failed keep-rule A/B).

## The optimization ladder (all zero-error, both arms)

| stage | binary | refine hot | refine spread | notes |
|---|---|---:|---:|---|
| original baseline (06-22:54) | pre-work | 53–60 | — | p95 1,198–1,256 ms |
| post F1–F8 (A/B, 2 threads) | prec | 1,809 | 2,067 | p95 101/91 ms |
| + F9 list/page wire memos (R1, 2 threads) | f9 | **8,971** | **5,653** | p95 12.5/27.2 ms |
| + idle core (R2, 4 threads, 50/75) | f9 | 9,483 | 6,277 | p95 16.3/32.6 ms |
| + F9c memo caps (50/75/150, 4 threads) | f9c | **9,234** | **8,140** | p95 25.3/32.4 ms |
| freeze (same runs) | 1.18.31 | 367–484 | — | p95 234–708 ms |

**Headline (closed model, pooled ladder): 9,234 vs 466 ≈ 19.8×; spread
8,140 vs 367 ≈ 22.2×** — against freeze's *recorded* 454 baseline: 20.3×.
CPU/req: 1.09 ms (pre-F9) → 0.47 ms (post-F9) → arrival-efficient
post-F9c.

## Route isolation (LOAD_ROUTES, single-route runs; cpu from report)

| route | refine solo req/s | p95 | cpu ms/req | freeze req/s |
|---|---:|---:|---:|---:|
| config | 24,308 | 4.4 ms | **0.011** (wire cache) | 4,395 |
| search | 6,459 | 13.5 ms | ~0.94 (incl. FTS miss path) | ~5,486 |
| session_list | 1,518 | 52 ms | 1.33 → wire-memo'd | ~145 (E0) |
| message_page | 834 | 93 ms | **2.54 → F9 memoised** | 216 |
| message_cursor | run1 invalid (0 reqs — filter blocked the priming page; fixed 52f15bf) | — | — | — |

## Open-model acceptance (arrival, rate=4,480, 60 s, 2 rounds) — four attempts

| attempt | achieved | p95 | drops | context |
|---|---|---|---|---|
| A1 13:43 | 4,371 / 4,378 (97.6–97.7%) | **88.9 / 92.5 ms** | 6.5k / 5.7k | right after closed verify (warm) |
| A2 14:14 | 4,416 / 4,407 (98.4–98.6%) | 119.8 / 128.3 ms | 3.5k / 4.1k | after 999-VU prelude |
| A3 14:38 | 3,699 / 3,933 (82.5–87.8%) | 546 / 470 ms | 43k / 32k | box polluted (freeze RSS 7.6 GB) |
| A4 15:20 | 3,364 / 4,345 (75.1–97.0%) | 362 / 142 ms | 94k / — | drift; freeze itself fell to 220 req/s |

**Pre-registered acceptance B (≥99% achieved AND p95 ≤104 ms AND
dropped=0 AND err ≤0.5%): NOT MET.** Closest = A1 (97.7%, p95 92.5 ✓,
err 0 ✓, drops 5.7k ✗) and A2 (achieved 98.6%, p95 120 ✗). Attempts
degrade monotonically with run-hours — freeze's own arrival collapsed
432→220 req/s with p95 up to 13.8 s and 7.6 GB RSS in the same runs —
so later attempts measure runner drift (thermal + co-tenant memory), not
binary changes (identical binary across A1–A4). All four recorded; none
discarded.

## Acceptance scorecard (PERF-10X 1, as pre-registered)

- **A (closed plateau ≥4,480 @ p95 ≤104): PASS** — 9,234/8,140 pooled,
  p95 25.3/32.4 ms (run 132925).
- **B (arrival): FAIL as written** — see four-attempt table; best joint
  97.7% / 92.5 ms.
- **C (10× vs freeze recorded 454): PASS on closed** — 20.3× (arrival
  best 9.6×, its own table stands).
- **D (gates):** thresholds.json re-derived (7c1d031, pre-gated, formula:
  refine ≤65 ms / freeze ≤1417 ms / err ≤0.5%) from run 132925; GATED
  run recorded in the section below.
- **Dual-variant (PERF-10X 1-D):** cache-off run (R4, all four kill
  switches, VU75, 4 threads): 2,198/2,708 req/s, p95 116/106 ms — the
  memos are worth ~3–4× of throughput; cache-on cells above.

## C-track A/B (keep-rule)

pre-C 2,612/2,868 → post-C 1,811/2,063 → pre-C 1,809/2,067 (VU50, one
sandwich): post-C ties pre-C to 0.1% with the build proven to have
applied both flag sets → **C1+C4 reverted** (4acd0fd); C5 kept
(trivial), C6 skipped (blocked p99 = 0.014 ms floor evidence), C7 size
ceiling re-baselined to 21,500,000 from measurement.

## GATED runs (acceptance D)

- **G1** `20261007T155504Z`: refine cells **25.97 / 32.95 ms** (bound
  either generation: pass); freeze cells 549.6 / 587.6 ms breached the
  **stale 208 ms** threshold — the committed re-derivation (7c1d031,
  refine ≤65 / freeze ≤1417) had not been rsynced to the runner. Run
  recorded, not discarded: the gate machinery worked (k6 rc=99
  per-cell), the artifact was stale.
- **G2** `20261007T163704Z` (pre-declared single re-run after cooldown,
  same config, committed thresholds): **0 breaches — PASS**. refine
  8,988/7,948 req/s, p95 25.96/33.08 ms (<=65), freeze 477/432, p95
  497/568 ms (<=1417), 0% errors all cells. Acceptance D: GREEN (both
  G1 and G2 recorded; G1's failure dispositioned as stale runner
  artifact, not a result).

## Stmt/pragma audit confirmation (ac8bb70 binary, VU50, ROUNDS=1, 4-thread)

`20261007T174649Z`: refine **9,228 / 9,099 req/s, p95 13.1 / 13.6 ms**
(hot/spread), freeze 460 / 420, 0% errors, **0 breaches**, peak RSS
182.7 MB. vs G2 (8,988/7,948 @ 25.96/33.08): p95 roughly halved,
throughput at or above — no regression from the stmt/pragma changes, and
**the fixture's first-ever planner stats confirmed post-boot**
(`sqlite_stat1` = 2 tables / 15 rows — the optimize placement fix
exercised through the real load-test spawn path).

## Lean sweep — Phase V A/B (pre-registered gate: CPU/req)

Runs `20261007T224955Z` (base, pre-branch `55c7262` binary) and
`20261007T223621Z` (lean, `47cb9ea`). Same box, same pinning
(refine `[0,4,3,7]`, freeze `[1,5]`, k6 `[2,6]`), VU50, ROUNDS=1,
identical 201-session fixture, 0% errors both arms.

| | base | lean | delta |
|---|---:|---:|---:|
| **CPU/req (arm-level)** | 0.2915 ms | **0.2496 ms** | **−14.4%** |
| rps hot | 10,404 | 11,618 | +11.7% |
| rps spread | 11,913 | 12,423 | +4.3% |
| p95 hot | 9.56 ms | 8.85 ms | −7.4% |
| p95 spread | 8.70 ms | 8.63 ms | −0.8% |
| peak RSS | 77.8 MB | **62.1 MB** | **−20.2%** |
| failed | 0.0% | 0.0% | — |

**Gate (pre-registered in `bench/perf/LEAN-PLAN.md` §3 Phase V: CPU/req
−10% or better): PASS — −14.4%** (0.2915 → 0.2496 ms/req; total CPU −7.7%
while serving +7.7% more requests).

> **Correction (same day, found by re-reading `report.py:83-93`).** An
> earlier version of this table published *per-mode* CPU/req
> (0.6253→0.5165 hot, 0.5461→0.4831 spread, −17.4%/−11.5%). That divided
> **arm-level** CPU — `cpu_delta` is keyed by arm and covers the whole
> sampled window — by a **single mode's** request counts, inflating every
> absolute and overstating the hot delta. The arm-level numbers above are
> the correct ones. The gate still passes (−14.4% vs −10%); only the
> magnitudes changed. The freeze control moved **+2.7%** (2.8180 → 2.8942
> ms/req) in the same rounds.

**Co-tenant validity:** the freeze control arm ran in both rounds and moved
only −1.5% / −3.1%, so the refine delta is not machine drift.

**Honest caveats.**
1. Two runs, not four. The 2nd/3rd interleaved rounds were cut when the
   harness consumed the time budget (a bare `REFINE_BIN` in its import path,
   then two orphaned imports pegging a core for 121 minutes while holding the
   write lock). One interleaved pair satisfies the methodology; a wider
   spread is not measured.
2. CPU/req is derived from `cpu s / reqs` in the harness sampler, so it
   includes the sampler's own accounting error (~±2%).
3. rps is reported, never gated (LEAN-PLAN §1 C2: the closed-model number is
   queueing-inflated).
4. The fixture carries 201 sessions / 48.5k messages / 178k parts; a corpus
   with more sessions would weight `list_wire` more heavily.

