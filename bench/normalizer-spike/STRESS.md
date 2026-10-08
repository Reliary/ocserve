# Stress + decision battery — pre-registered (2026-10-06, BEFORE any run)

Answers: "what works best" (full 6-cell decision matrix) and "how do we
stress test it". S5 soak SKIPPED by user decision (production soak covers
raw-bun; normalized soak deferred to adoption).

## Cells

6 cells = {raw, norm} × {bun, node, deno}. Raw = original entries (deno's
magic-context baseline = index.deno.js, pre-existing patched raw). Norm =
index.normalized.mjs (K5b shim). Each cell: fresh process per run.

## Phase 1 — decision matrix (2 rounds/cell; anomaly re-run ×3 to confirm)

Per cell, 50 ms sampler thread:
- **warm-up peak** RSS (spawn → settle)
- **post-load** RSS (3 plugins loaded)
- **post-burst** RSS (after 500 mixed triggers)
- **settled** RSS at t+60 s and t+180 s after last activity
- plus boot/load/trigger medians (cross-check vs perf_compare)

**Kill criteria (pre-registered):**
- M1: norm settled ≤ same-runtime raw settled +10 % (else norm loses its
  adoption case on memory)
- M2: no cell's post-burst RSS fails to come back within 20 % of post-load
  (GC/retention red flag — the glibc lesson, per-runtime)
- M3: capability re-assert: all 3 plugins load in every cell (any load
  failure = cell dead, not "known issue")

## Phase 2 — stress battery

- **S1 sustained (all 6 cells)**: 2,000 mixed triggers (chat/xform/event
  rotation), windowed p50/p95/p99 in 500-trigger windows.
  PASS: no panic/hang/error; window-4 p95 ≤ window-1 p95 × 3 (no monotonic
  creep); zero protocol desync.
- **S2 payload scaling (all 6 cells)**: conversations 10 KB → 100 KB →
  1 MB → 8 MB (history-budget ceiling), 20 triggers each.
  PASS: all succeed; per-size RSS peak recorded; RSS returns within 20 %
  of baseline after each level (trim/GC works); report the knee (first
  size where p50 > 50 ms or RSS fails M2-style recovery).
- **S3 concurrency (top-3 cells by Phase 1+2)**: 4 threads × 500 triggers,
  one host, id-matched multiplexer.
  PASS: zero errors/desync; throughput ≥ sequential baseline × 1.5
  (concurrency actually helps — if not, record that production's mutex
  serialization is free); peak RSS ≤ sequential peak × 2.
- **S4 chaos (top-3 cells)**:
  1. SIGKILL mid-trigger → restart host → reload norm → trigger OK
  2. Truncate .normalized.mjs mid-flight → load must FAIL LOUD (error,
     no hang, no partial-hook silent success) — expected FAIL-LOUD today
     (no fallback in spike; proves integration fallback is mandatory)
  3. Entry content edit → warm cache must go cold (re-normalize) → restore
  4. Delete outputs+hash while host running → respawn path rebuilds
  5. bun absent from PATH → selection falls to node (ocserve's
     plugin_runtime preference, re-asserted at driver level)
  6. Normalizer input adversarial: syntax-error entry, import cycle,
     binary garbage as .js, 50 MB file → normalize returns Err (NOT a
     panic — panic = KILL for adoption)
  PASS = each behaves as its expected definition; unexpected behavior in
  ANY item = finding blocks adoption until dispositioned.

## Phase 3 — integration gates (build-only experiment, reverted after)

Add rolldown+rolldown_common to a workspace member behind a real (feature-
gated) call so LTO cannot dead-strip it; measure: release binary size vs
ceiling 10,485,760 B, build-time delta vs baseline, dep-count delta.
PASS: size ≤ ceiling (else adoption needs size-gate decision) + report.

## Decision rule (scores, not vibes)

Adopt norm only if: M1–M3 pass AND S1–S4 pass as defined AND Phase 3 size
≤ ceiling. Ranking among runtimes uses: capability (gate) → memory → CPU →
load → hot-path (expected tie) → robustness. Default hypothesis to verify
or kill: **bun+norm default, node+raw fallback, deno proven third.**

## Non-goals

- No ocserve code changes (spike layer only; adoption = separate decision).
- No live-fire against opencode cache beyond the additive .normalized.mjs
  artifacts already in place.

## Verdict (appended after execution — see STRESS-RESULTS.md for data)

- Phase 1: M1 PASS, M2 PASS, M3 PASS (12/12)
- S1: 6/6 PASS · S2: 6/6 PASS (GC-window dispositions: node @10s, deno @30s)
- S3: 3/3 PASS (after test-harness fix: dedicated-reader multiplexer)
- S4: 6/6 PASS as defined; zero panics on adversarial normalizer input
- Phase 3: **FAIL as gated** — linked rolldown = 20,309,584 B vs ceiling
  10,485,760 B; probe reverted to exact baseline. Dispositions D1–D4 in
  STRESS-RESULTS.md; **recommendation: D2 helper-binary architecture**
- Best cell: **bun-norm** (lowest memory, sustained ~top, chaos-clean)
