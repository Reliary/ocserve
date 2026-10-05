# OOM-reload report — leak vs fragmentation, and the fix (K-OOM-RESILIENCE B1/B2)

Date: 2026-10-05. Question: why did main RSS step up ~+220 MB on every hourly
`models.json` reload with zero user activity (8 kernel memcg kills that day,
last at 21:13:19 before this fix)?

## Method (`oom_reload.py`, re-runnable)

Isolated fake HOME (no MCP/plugin children → bytehound stays in refine),
**full-size real catalog fixtures** (a tiny fixture would make the profile
lie — 5,319,988 bytes, one field mutated so content-hash differs = the exact
production trigger), forced reloads via fixture swap, step metric = A2's
`config hot-reload ok … rss X → Y` log. Tunables A/B via env; attribution via
bytehound (`top-alloc.rhai`).

## Results

| run | reload 1 | reload 2 | reload 3 | converged main RSS |
|---|---|---|---|---|
| plain (glibc default arenas) | **+220 MB** | +68 | +1 | **526 MB** |
| `MALLOC_ARENA_MAX=2` | +220 | +68 | +1 | 526 |
| **`MALLOC_ARENA_MAX=1`** | **+69** | **0** | +1 | **308 MB** |

bytehound (profiled run): 25,029,004 allocations — **24,310,340 temporary
(churn), 718,664 live at exit**. The largest "leaked-at-exit" group is
`load_for → build_providers → serde_json::to_value` — i.e. **the live
payload Value itself, by design**. No unbounded holder exists.

## Verdict

**Fragmentation, not a leak.** Each reload parses/transforms ~millions of
small serde JSON nodes; glibc's per-thread arenas free them into fragments
that cannot satisfy the next reload's layout → RSS steps to a per-process
high-water (+~290 total, converging) and **resets only on restart** — which
is why the hourly production pattern looked unbounded across the day
(264→478→715→783 across restarts+reloads) and why the kills stopped once
the warm base was high. `MALLOC_ARENA_MAX=1` forces single-arena reuse:
converged plateau drops 526→308 MB and the step collapses (+69, 0, +1).
Reload wall-time unchanged (~2.7 s in all variants → no contention signal
at current load; soak `kids`/slope columns monitor this).

## Production confirmation (after unit wiring)

Unit: `Environment=MALLOC_ARENA_MAX=1` + `MemoryMax=750M` (both live and
`deploy/refine.service`; stale dead MIMALLOC env removed from the live unit):

- idle main **231.7 MB**, cgroup **402 MB** (budget: combined ≤460 ✓)
- reload #1 (cold): `rss 231.7 → 322.2` (**+90**, 735 ms)
- reload #2: `322.7 → 322.9` (**+0.2 MB**, 771 ms) — converged, lab pattern
  confirmed live
- cap returned **1536M → 750M** (the number the OOMs used to blame; the
  mechanism is what was wrong, not the size)

## Acceptance (tiny-footprint gates)

| gate | target | measured |
|---|---|---|
| idle cgroup | ≤460 MB | 402 MB ✓ |
| reload net growth (warm) | ≤20 MB | +0.2 MB ✓ |
| final MemoryMax | ≤750 M | 750M ✓ |
| reload wall-time | no regression | 735–771 ms vs 2.7 s lab ✓ |
| kids | ≤~180 steady | ~170–214 steady (warming spikes transient, soak-tracked) |

## Follow-ups

- Tomorrow's hourly catalog reloads should log ≤+90 cold (on a restarted
  process) then ~0; `refine_config_reload_total{result}` + A2 lines watch it.
- Catalog growth (5.3 → 6–8 MB over months) raises the cold parse transient —
  re-check boot margin if it does.
- bytehound `.dat` lives in `/tmp/opencode/oom-lab/work/` (ephemeral);
  key numbers are extracted above.


## Tier 1 (same night): where the *size* comes from — and the trim fix

The arena1 fix removed the per-reload *step*, but a fresh lab run showed the
full catalog still settles at **237 MB** vs a 92-byte-catalog control at
**19 MB** — so ~218 MB of freed-but-retained glibc heap comes from the
build_providers/transform/parse pipeline itself, not from live data.

| lab variant (full 5.3 MB catalog) | boot settle | 3 reloads | final |
|---|---|---|---|
| arena1 only | 237 MB | +70/0/0 | 307 MB |
| arena1 + LD_PRELOAD `malloc_trim(0)` shim | 91 MB | +3/0/−1 | 91 MB |
| **arena1 + `watch::trim_heap()` (shipped)** | **89 MB** | **+3/0/+2** | **94 MB** |
| control: `REFINE_TRIM=0` (killswitch) | 237 MB | +70/0/0 | 307 MB |
| control: 92-byte catalog | 18–19 MB | — | 19 MB |

Fix shipped in commit `31b90c1`: `malloc_trim(0)` at boot, after every
reconcile swap, and every `REFINE_TRIM_SECS` (default 300 s); `REFINE_TRIM=0`
is the killswitch the lab control uses. `tests/trim_contract.rs` pins the
killswitch contract in its own process (env-race rule, TESTING §1).

**Production confirmation after deploy (22:32):** main **98 MB**, cgroup
**312 MB** (budget ≤460; was 321/517 before, 774/1000+ at the worst),
and the first production reload logged `rss 98.9 → 96.7 MB` — trim returns
pages between reloads, so RSS *shrinks* on reload instead of stepping.

**Deferred (measured, not built):** Tier-1.1 bytes-ify the `/provider` and
`/config/providers` payload DOMs (they are retained ~15–20 MB and deep-cloned
per request via `Json(…clone())`). Trim already covers the current target;
the DOM work is the next lever if main needs to go under ~60 MB.
