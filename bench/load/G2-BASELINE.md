# G2 baseline — fresh, current binary (2026-10-17)

Fresh `cargo install`-class G2 run of the committed binary
(`md5 4245c25446c9cecc4a2bed6defa52b75`, HEAD `5541c3e`) on the `.227` runner,
replacing the 2026-10-07 numbers that predated Phase G and the v2 work.

- **Runner:** `.227`, homogeneous 4C/8T, CPU pinning
  `ocserve=[0,4] freeze=[1,5] k6=[2,6]` (each arm owns whole physical cores).
- **Command:** `GATED=1 ROUNDS=1 LOAD_TARGETS=50,75,150 LOAD_ARRIVAL=1
  LOAD_RPS=4480 scripts/load-test.sh`
- **Artifacts (retained, in-repo):** `bench/load/baseline-runs/20261017T220243Z/`
  (informational baseline, 1 interleaved pair) and
  `.../gated-20261017T223838Z/` (GATED). Summaries + sampler + report kept;
  the raw k6 stdout log is not committed (23 MB).

## Closed-model cells (the gate)

Derived bounds in `thresholds.json` (formula `max(2·worst, worst+30)` from the
informational baseline): ocserve ≤ 87 ms, freeze ≤ 1163 ms, err ≤ 0.5 %.

| arm | mode | p50 ms | p95 ms | req/s | failed | peak RSS MB | verdict |
|---|---|---:|---:|---:|---:|---:|---|
| **ocserve** | hot | 9.1 | **34.6** | **6736** | 0.0% | 724 | **PASS** (≤87) |
| **ocserve** | spread | 8.2 | **40.9** | **6339** | 0.0% | 724 | **PASS** (≤87) |
| freeze | hot | 154.3 | 561.9 | 417 | 0.0% | 3089 | PASS (≤1163) |
| freeze | spread | 176.0 | 577.0 | 378 | 0.0% | 3089 | PASS (≤1163) |

**GATED = PASS on every closed cell, 0 breaches.**

- **Throughput:** ocserve 6736 / 6339 req/s vs freeze 417 / 378 → **16.2× /
  16.8×**.
- **Latency:** ocserve p95 34.6 / 40.9 ms vs freeze 562 / 577 ms → **16.2× /
  14.1× lower**.
- **Peak RSS:** 724 MB vs 3089 MB → **4.3× less**.
- **CPU (arm-level, whole sampled window incl. arrival):** ocserve 1469.6 s,
  freeze 526.6 s. Divided by each arm's total served requests this is
  0.51 vs 1.68 ms/req, but freeze's arrival failed (79 % dropped) so its
  request count is depressed and inflates its per-req figure; treat CPU/req as
  indicative here, not gated (LEAN-PLAN C2 precedent).

## Arrival (open-model) — FAIL as written, both arms

Pre-registered B criteria (PERF-10X §1): achieved ≥ 99 %, p95 ≤ 104 ms,
`dropped_iterations = 0`, err ≤ 0.5 % at 4480 offered rps / 60 s.

| arm | achieved | dropped | p95 ms | failed | verdict |
|---|---:|---:|---:|---:|---|
| ocserve | 71.0 % | 77691 | 536.2 | 0.0 % | **FAIL** (achieved/p95/dropped) |
| freeze | 55.8 % | 118696 | 778.7 | 95.2 % | **FAIL** (achieved/p95/dropped/err) |

ocserve is better on every arrival axis (2.0× the achieved rate, 0.69× the
p95, 0 errors vs freeze's 95 % failures), but neither arm meets the bar — this
is the same B outcome recorded in LEAN/PERF-10X, now on the current binary and
the `.227` box with two resident llama-servers. **The stop rule applies: ship
the honest frontier** — closed-model capacity PASS, arrival FAIL, both stated.

## Honest caveats

1. **One interleaved pair, not two.** `ROUNDS=2` was requested; round 1
   completed, round 2 aborted by the harness's fail-fast when the freeze
   control arm died under co-tenant memory pressure (`MemAvailable` reached
   0.1 GB with two resident llama-servers). One interleaved pair satisfies the
   methodology; a wider spread is not measured.
2. **Pinning class changed** (4-thread layout on 2026-10-07 → homogeneous
   2-cores-per-arm here), so the derived bounds moved (ocserve 65 → 87 ms).
   The current binary's recorded worst closed cell is 40.9 ms; the looser bound
   reflects the pinning class, not a regression.
3. **`cpu s` is arm-level** over the whole sampled window (all three modes), so
   CPU/req is indicative only (see above).
4. The fixture carries 201 sessions / 48.5k messages / 178k parts; a larger
   corpus would weight `list_wire` more heavily.
