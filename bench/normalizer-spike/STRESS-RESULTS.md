# Stress + decision battery — results (2026-10-06)

Pre-registration: `STRESS.md` (written before any run). S5 soak skipped by
user decision. All runs: `stress.py` / `matrix6.py` in this directory;
evidence in `matrix-results.jsonl`.

## Phase 1 — decision matrix (2 rounds × 6 cells, 50 ms sampler)

Medians across rounds:

| cell | warm-up peak MB | post-load MB | post-burst MB | settled +180s MB | CPU s |
|---|---|---|---|---|---|
| bun-raw (prod) | 186–190 | 138–145 | 144–160 | 88.7 | 1.37–2.01 |
| **bun-norm** | **165–168** | 118–124 | 120–147 | **76.3** | 1.03–1.18 |
| node-raw | 146–149 | 142–144 | 146–149 | 111.1 | 0.83–1.20 |
| node-norm | 147–152 | 144–148 | 147–152 | 96.8 | 0.95–1.00 |
| deno-raw | 167–169 | 163–164 | 167–169 | 96.8 | 1.73–1.98 |
| deno-norm | 166–167 | 157–158 | 166–167 | 102.4 | 1.75–1.85 |

- **M1 PASS** (norm settled ≤ raw+10%): bun 76.3 ≤ 97.6 · node 96.8 ≤ 122.2
  · deno 102.4 ≤ 106.4. **bun-norm is the lowest-memory cell of all six.**
- **M2 PASS** all runs (settled+60 ≤ post-load×1.2 — burst always returns).
- **M3 PASS** 12/12 (every cell loads all 3 plugins).

## S1 — sustained 2,000 mixed triggers (all 6 cells)

| cell | throughput | window1 p95 | window4 p95 | errs | verdict |
|---|---|---|---|---|---|
| bun-raw | 1,681/s | 1.47 | 1.45 | 0 | PASS |
| bun-norm | 1,471/s | 1.75 | 1.48 | 0 | PASS |
| node-raw | 1,143/s | 2.02 | 2.16 | 0 | PASS |
| node-norm | 1,256/s | 1.81 | 1.80 | 0 | PASS |
| deno-raw | 283/s | 5.32 | 12.56 (2.4×, inside 3× bound) | 0 | PASS (worst cell) |
| deno-norm | **865/s** | 2.89 | 2.57 | 0 | PASS — **norm makes deno 3× faster sustained** |

## S2 — payload scaling 10 KB → 100 KB → 1 MB → 8 MB (all 6)

Knee at 8 MB in every cell (the history-budget ceiling): p50 126–139 ms
(bun), 406–516 ms (node), 145–375 ms (deno). ≤1 MB everywhere: ≤18 ms.
Recovery disposition (anomaly → re-run per pre-registration): the first
pass used a 1 s GC window and flagged node-norm/deno-raw/deno-norm —
re-runs: node-norm **PASS @10 s**, deno both **PASS @30 s** (slow V8 GC
after 8 MB payloads — settles *below* baseline, 115–133 MB; not retention).
**Final: 6/6 PASS**, with documented GC window: deno needs ~30 s after 8 MB.

## S3 — concurrency: 4 threads × 500 triggers, one host (top-3 cells)

| cell | conc throughput | sequential | speedup | errs | verdict |
|---|---|---|---|---|---|
| bun-raw | 1,600/s | 829/s | 1.93× | 0 | PASS |
| bun-norm | 1,407/s | 753/s | 1.87× | 0 | PASS |
| node-norm | 1,123/s | 713/s | 1.57× | 0 | PASS |

Test-design finding (recorded): the first S3 hung — four threads doing
naive `readline()` on one pipe misattribute responses (idle-timeout
deadlock). Fixed per the pre-registered design with a dedicated-reader
id-matched multiplexer; hosts were never at fault.

## S4 — chaos battery (6/6 PASS, each per its pre-defined expectation)

1. SIGKILL mid-run → restart → reload normalized entries → trigger OK.
2. Truncated `.normalized.mjs` → load **fails loud** ("no server
   factory"), no hang, no partial hooks — confirms loud-fail contract for
   integration (fallback optional, not required for safety).
3. Entry content edit → cache goes cold → rebuild → stable warm after.
4. Outputs deleted while host running → respawn path rebuilds.
5. bun absent from PATH → selection falls to node (env probe; the
   preference rule itself is unit-tested in refine).
6. Adversarial normalizer inputs (syntax error, import cycle, binary
   garbage, 50 MB file): graceful `rc=1` diagnostic or valid bundle —
   **zero panics**.

## Phase 3 — build gate: **FAIL** (the one red)

| measurement | value |
|---|---|
| baseline `refine` release | **9,918,024 B** / 502 deps / cached build 0.32 s |
| probe with rolldown reachable from main | **20,309,584 B** (+10.39 MB) / cold dep compile 455 s |
| ceiling (nightly `size_gate`, stats `target/release/refine`) | 10,485,760 B → **over by 9,823,824 B** |
| slimming options | rolldown features = `serde`/`testing` only — **no slim path** |
| revert | tree = HEAD, binary restored to exactly 9,918,024 B |

Disposition options (user decision, all honest):
- **D1 — raise the ceiling** to ~21 MB (gate is self-imposed from a
  2026-10-03 measurement; provenance rule allows an evidenced raise).
- **D2 — helper binary architecture** (recommended): `refine` spawns
  `refine-normalize <entry>` when the content-hash is stale; the main
  binary links nothing rolldown-related (stays ~9.9 MB, passes gate as
  measured), helper rides the release package (~10–20 MB, exact number
  measured at adoption). Bonus: a rolldown crash lands in the helper
  (respawnable), matching the sidecar-failure model.
- **D3 — park adoption**: production stays bun-preferred raw; spike
  remains a bench tool. Forgoes the memory win + runtime-agnostic class.
- **D4 — custom mini-bundler on oxc**: rejected pre-registered (weeks of
  correctness risk reimplementing module ordering/hoisting/collisions).

## Decision matrix + recommendation

Capability gate: all 6 cells pass. Ranking axes (memory → CPU → load →
sustained → hot path):

**Best cell: bun-norm** — lowest settled memory (76.3 MB; raw-bun 88.7),
lowest warm-up peak (165 vs 186), sustained 1,471/s, hot-path tie,
chaos-clean. Second: bun-raw (current production, no integration cost).

Normalization's measured benefits: +memory on bun, +load/CPU on node
(−26%/−26%), +load/CPU on deno (−14%/−14%) and **+3× sustained on deno**
(283→865/s), one file per plugin, zero per-runtime patches, quirk class
eliminated.

**Recommendation: D2.** It satisfies every pre-registered gate without
moving goalposts — `refine` stays at 9,918,024 B, the helper isolates
the 10.4 MB dependency, and the measured wins become available. D1 is the
fallback if single-binary distribution matters more than the ceiling.
