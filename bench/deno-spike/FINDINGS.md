# Deno-as-sidecar spike — findings (2026-10-06)

Question: can Deno (one Rust-authored, checksum-provisionable runtime) replace the
bun/node duality as refine's plugin host? Pre-registered kill criteria from the
research plan: (1) `bun:sqlite` resolves, (2) N-API addons actually load, (3)
warm-up RSS ≤ node's, (4) hooks pass conformance.

## Verdict: PASS on all four criteria, with two integration deltas required

| # | Criterion | Result |
|---|---|---|
| 1 | `bun:sqlite` resolves | **PASS via load-time patch.** Deno 2.9 has no module-hook API for scheme imports (`node:module.register` does not intercept; import maps don't apply inside npm packages). Worked: rewrite the 4 `from "bun:sqlite"` occurrences in magic-context's single bundle (`dist/index.js`, 4.3 MB, zero relative imports) to `./bun-sqlite.deno.mjs` (copy of our shim), written as a **new sibling file** — originals untouched. All other imports are declared deps and resolve with `--node-modules-dir=manual`. |
| 2 | N-API addons load | **PASS, including real inference.** `onnxruntime-node`, `sharp`, `@msgpackr-extract` all load under `deno run -A --unstable-ffi`. Went further: loaded the local 86 MB Xenova ONNX model and ran a real session — `{"session":"OK","ms":242,"dims":[1,4,384]}` with actual tensor values. |
| 3 | Warm-up RSS | **PASS.** Full 3-plugin load: peak **165 MB** (node peak 137, bun peak 260), settles **133 → 115 MB** over 3 min (node settles ~97–101, bun ~98). Deno sits between node and bun — well under bun's warm-up, converging toward node. |
| 4 | Hook conformance | **PASS.** All 3 plugins load with production hook sets (codex-auth 8, magic-context 9, reliary8 1); `chat.message` 2 hooks, `tool.execute.after` 2 hooks, event `delivered:1`. Matches bun/node behavior exactly. |

## The two integration deltas (what shipping would take)

1. **Entry preprocessing**: at `load`, if runtime=Deno, emit
   `dist/index.deno.js` = `sed`-rewritten `bun:sqlite` → relative shim + copy
   `bun-sqlite.deno.mjs` beside it (idempotent, content-addressed; new files in
   the opencode cache are additive and harmless — node/bun paths never read them).
   Fallback: if rewrite unnecessary (future plugins), load as-is.
2. **Spawn flags**: `deno run -A --unstable-ffi --node-modules-dir=manual`.
   `-A` covers permissions (incl. FFI); `--node-modules-dir=manual` is required
   for Deno's strict bare-specifier rule (declared deps found in the hoisted
   node_modules tree).

## Honest deltas vs node/bun

- **Binary size**: deno 221 MB vs bun 92 MB vs node 55 MB (brew install; a
  pinned release download would be the provisioning source — no embed API exists
  for any of the three).
- **Settled RSS slightly above node**: 115–133 MB vs node ~97. Still inside the
  sidecar boundary (160 MB cap).
- **Spike cost**: `--unstable-ffi` is a Deno unstable surface (stable in practice
  for N-API since 2.x, but the flag name signals churn risk across Deno majors).
- **Strictness is real**: two distinct failures before PASS (dependency
  enforcement, scheme imports) — a future plugin using an undeclared dep or a
  novel scheme would need the same triage. `--node-modules-dir=manual` fixes the
  first class; the rewrite fixes `bun:` only, not other exotic schemes.
- Deno 2.9.7 (v8 15.0.245.2) installed via brew for the spike at
  `/home/linuxbrew/.linuxbrew/bin/deno` — **not yet a refine dependency**;
  `REFINE_PLUGIN_RUNTIME` only knows bun|node today.

## Performance (added 2026-10-06 — memory was never the whole story)

`perf_bench.py`, 5 interleaved rounds (rotation order, fresh process per
run), workloads: spawn→pong, load 3 real plugins, 100× `chat.message` RTT
after 10 warmup, 50× `experimental.chat.messages.transform` with a ~120 KB
conversation (the once-per-prompt payload), 20× event, host utime+stime.

| metric (median of 5 runs) | bun | node | deno |
|---|---|---|---|
| boot (spawn→pong) | **33 ms** | 62 ms | 49 ms |
| load 3 plugins | **397 ms** | 441 ms | **1,609 ms** |
| chat p50 / p95 | 0.25 / 0.46 ms | 0.20 / **0.39** ms | 0.22 / 0.41 ms |
| xform p50 / p95 (120 KB) | 0.59 / 1.35 ms | 0.55 / **0.90** ms | 0.63 / 1.27 ms |
| CPU for whole run | **0.71 s** | 0.73 s | **1.80 s** |

Per-round spreads were tight (deno load 1448–1656 ms, bun 370–438, node
418–494), so these are signal:

- **Steady-state trigger latency: three-way tie.** All sub-millisecond;
  the NDJSON pipe + hook dispatch dominates and that code is identical.
  No runtime wins the hot path.
- **Deno pays 4× on load** (module-graph resolution/transpile of the 4.3 MB
  bundles) and **2.5× on CPU** for the identical workload. Boot time is
  irrelevant for all three (~50 ms, once per service start).
- **Bun is the cheap runtime**: fastest boot, fastest load, lowest CPU —
  ties node on latency, wins load/boot/CPU. The earlier "steady-state RSS
  wash" now has a performance counterpart: bun is the *efficiency* winner,
  deno the clear loser on both CPU and load.

Verdict update: Deno's spike PASS stands on *capability* (it can run
everything), but performance makes it the **wrong default** — it would
trade bun's CPU/load advantages for a dependency-management story. If a
single provisioned runtime is ever wanted, bun pinned+provisioned is the
better candidate than deno on these numbers.

## Repro

Scripts in this directory (spike-only, not wired into refine):
- `importmap.json` (tried, insufficient — recorded for the negative result)
- `drive3.py` / `drive4.py` — full run + RSS sampling
- Patched files live in the opencode cache as `dist/index.deno.js` +
  `dist/bun-sqlite.deno.mjs` (additive; remove to return to pristine)

## Bottom line

Deno genuinely dissolves the bun/node duality: one runtime, Rust-authored,
upstream-agnostic, runs all three plugins *including native ML inference*,
at a warm-up between node and bun. **But the performance pass kills it as
a default**: 4× slower plugin load, 2.5× CPU for identical work, ties only
on steady-state latency (which is pipe-bound for everyone). Bun wins boot,
load, and CPU — so if the goal ever becomes "one pinned, provisionable
runtime", **pinned bun is the better candidate than deno** on measured
numbers, with deno retained as the proven fallback if bun's npm-semantics
strictness ever matters. Current recommendation stands: park with findings
recorded; bun-preferred dual runtime stays.
