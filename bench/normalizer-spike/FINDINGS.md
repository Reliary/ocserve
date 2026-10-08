# R3 normalizer spike — findings (2026-10-06)

Question (README, pre-registered): can a Rust-side normalizer (rolldown
crate as a library) convert each opencode plugin into ONE self-contained
ESM file that loads with **zero runtime-specific patches** on bun, node,
AND deno — making runtime choice irrelevant?

## Verdict: criteria met — K5 failed as registered, K5b (post-hoc) rescued it

| # | Criterion | Result |
|---|---|---|
| K1 | all 3 plugins bundle to single ESM, no errors | **PASS** — codex-auth 154 modules/870 KB/70–153 ms, magic-context 75 modules/4.2 MB/256–478 ms, reliary8 2 modules/4.4 KB/3–12 ms; exactly-1-chunk enforced in code |
| K2 | hook-set parity raw↔norm within each runtime | **PASS** bun/node/deno (bun's first run failed via K5 → after K5b: PASS) |
| K2b | normalized hook sets identical across runtimes | **PASS** — same file, same hooks on all three |
| K3 | trigger outputs byte-identical raw↔norm | **PASS** bun/node/deno (chat.message, tool.execute.after, event ×2) |
| K4 | computed dynamic import survives + resolves from output location | **PASS ×3** — `` import(`@huggingface/${"transformers"}`) `` survives (folded to a literal template but left runtime — not bundled); direct probe from beside the output: K4_OK 932 exports on bun/node/deno (73–151 ms). Embedding-init log: present on node/deno raw AND norm (parity); absent on bun raw AND norm (pre-existing runtime behavior, not a regression) |
| K5 | node:sqlite shim serves bun | **FAIL as registered** — see below |
| K5b (post-hoc) | dual-runtime shim | **PASS** — standalone ×3 + full parity matrix green |
| K6 | cost | **PASS** — cold ≤478 ms (limit 2 s), warm = instant content-hash hit, invalidation proven (hash-input change → cold), trigger latency raw↔norm within 2×. Full interleaved comparison below |
| K7 | treeshake off, side effects survive | **PASS** — `TreeshakeOptions::Boolean(false)` asserted in code; proven by K2/K3 |

## K5 failure (recorded exactly as it happened)

K5's premise was **factually wrong**: it claimed bun's `require("node:sqlite")`
fails but ESM import works. Truth: **bun 1.3.14 has no `node:sqlite` in file
context at all** (both static and dynamic reject; my original "ESM OK" evidence
came from `bun -e` eval context, which resolves differently). The
node:sqlite-only shim therefore could not serve bun: bundled magic-context
failed to load (`ResolveMessage: No such built-in module: node:sqlite`),
cascading to bun K2/K2b/K3 FAIL while node+deno passed clean.

## K5b (post-hoc rescue, registered in README before testing)

Dual-runtime shim (`shim-k5b.mjs`): prefer native `bun:sqlite` via a
**computed** dynamic import (`"bun:" + "sqlite"` — non-analyzable, the same
trick magic-context itself uses for transformers, so no bundler alias
recursion and rejections are catchable promises), fall back to computed
`node:sqlite` (dynamic because a static `node:sqlite` import fails bun's
graph link before any code runs), TLA live-binding `export let Database`
(the only name magic-context imports — 4 aliases). The node path is the
shipped shim's classes **verbatim** (byte-parity: including its duplicate
`query` definition where the second wins). Bundler-side: alias plugin gains
an importer guard (`bun:sqlite` from inside the shim → external runtime
import) so constant-folding cannot create a self-import. Result: full
matrix green.

## K6 performance — interleaved raw vs norm (5 rounds, 30 fresh processes)

`perf_compare.py` mirrors the deno-spike methodology (variant order rotated
per round, fresh process per run, 10-warmup then 100× chat RTT + 50× 120 KB
transform RTT, host utime+stime). Medians of 5 runs:

| runtime | load raw→norm | chat p50 raw→norm | xform p50 raw→norm | CPU raw→norm |
|---|---|---|---|---|
| bun | 400 → 472 ms (+18%) | 0.262 → 0.276 ms | 0.599 → 0.629 ms | 0.67 → 0.68 s |
| node | 556 → **409 ms (−26%)** | 0.206 → 0.302 ms | 0.634 → 0.737 ms | 0.88 → **0.65 s** |
| deno | 1722 → **1481 ms (−14%)** | 0.237 → 0.207 ms | 0.603 → 0.770 ms | 1.88 → **1.62 s** |

- **Hot path: parity.** Every trigger median stays sub-millisecond either
  way — the NDJSON pipe dominates, exactly as the runtime round established.
  Worst observed ratio is node chat +0.096 ms absolute (well inside K6's 2×).
- **Load/CPU: normalized wins on node and deno** (one file parsed instead
  of a 154-module resolution graph; CPU −14% to −26%), pays +18% on bun
  (bun's lazy per-file resolution was already fast; parsing the 870 KB
  single file costs slightly more) — absolute delta 72 ms, once per boot.
- Boot ~30–100 ms all variants — irrelevant.

## Architecture facts worth keeping

- **Output MUST sit beside the entry**: the computed transformers import is
  resolved at runtime against the entry's `node_modules` ancestry. A cache
  dir elsewhere would break K4. Content-hash sidecar file (`.mjs.hash`)
  invalidates on entry/shim/tool-version change; opencode cache wipe =
  fail-soft rebuild.
- **Rolldown as a library works** (crate 1.2.12, plugin trait via RPITIT):
  resolve_id/load hooks, `platform: node` externalizes builtins, static
  bare specifiers (`@opencode-ai/plugin`) bundle in — killing the deno
  "declared dependency" failure class entirely — while non-analyzable
  dynamics stay runtime by construction.
- **`import.meta`: 0 uses** across all 3 plugins (attack ledger A5 dead).
- **`treeshake: Boolean(false)`** is load-bearing: top-level side effects
  are the registration contract.
- **Zero runtime-specific patches remain**: the `index.deno.js` sed hack
  from the deno spike is superseded — one `.normalized.mjs` serves all
  three (deno still needs its spawn flag `--node-modules-dir=manual` for
  the *runtime-external* transformers import; spawn flags are per-runtime
  config, not per-plugin patches).

## Integration gate (if adopted — separate decision)

- K5b shim must replace/augment ocserve's shipped shim (default
  `NORMALIZER_SHIM` currently points at the shipped one, which fails bun —
  the experiment is explicit via env).
- Release ceiling ≤ 10,485,760 B + build-time delta — **measured 2026-10-06
  (STRESS Phase 3): linking rolldown reachable from `ocserve` = 20,309,584 B
  (+10.39 MB, over ceiling by 9.82 MB; 455 s cold compile). FAIL as gated;
  disposition = helper-binary architecture (D2) keeps `ocserve` at
  9,918,024 B — see bench/normalizer-spike/STRESS-RESULTS.md.**
- Load-path wiring: normalize-before-load with warm cache (measured
  warm ≈ 0), fallback to direct entry load if normalization fails.
- Nightly: parity driver ×3 runtimes (the conformance suite).

## Repro

```
cargo build --release              # spike crate, detached workspace
./target/release/normalizer-spike normalize <entry> [--out p]
NORMALIZER_SHIM=$PWD/shim-k5b.mjs ./target/release/normalizer-spike normalize <entry>
python3 parity_driver.py           # K2/K2b/K3/K4 matrix → parity-results.json
```

Additive artifacts in the opencode cache: `index.normalized.mjs` +
`index.normalized.mjs.hash` per plugin (removable; raw entries untouched).


## D1 executed (2026-10-06) — normalizer linked into `ocserve`

Decision: **D1**, superseding the D2 recommendation at the end of STRESS-RESULTS
(user call after the size-context analysis: the linked binary measured
**20,359,600 B** — still 9× smaller than upstream's 185 MB ELF; the size tripwire
was re-baselined *from that measurement* to 20,971,520 B in `scripts/nightly.sh`
with provenance, never moved to fit).

Shipped: `crates/ocserve-plugin/src/normalize.rs` — sha256 content hash (entry bytes
+ K5b shim + `normalize-v1` salt) warm-gating BOTH candidate destinations; rolldown
build inside `spawn_blocking` (A2 worker-blocking class); conditional emit via a
**code-text scanner** (chunk metadata `imports`/`dynamic_imports` provably misses
non-analyzable dynamic imports — rolldown `src/ast_scanner/impl_visit.rs:241` — so a
metadata-only scanner would have misclassified magic-context as clean and shipped a
broken plugin); `Sidecar::load_raw` insertion so boot AND respawn replay normalize
(raw entry stored — A1; wipe-while-running self-heals at respawn and next boot);
`OCSERVE_PLUGIN_NORMALIZE=0` checked before rolldown exists (R1 panic=abort gate);
`ocserve_plugin_normalize_total{result}` + A2 log (destination, ms, RSS before→after)
on built/error only; atomic pid-tmp + rename, hash-after-rename (guard rule 8).

**Ground-truth correction (evidence over intuition, both directions):** codex-auth
was assumed clean and the scanner flagged it needs-ancestry. The scanner was right —
`getLockFunction()` does `await import(specifier)` with specifier = `proper-lockfile`
(its package.json dep), a runtime-resolved bare package. Beside-entry emit in the
cache is required (and correct); the "clean" assumption was the error.

**Live battery 6/6** (2026-10-06, production unit, five boots):
- cold: `built 3` — codex 191 ms (beside, RSS 99,348→136,864 kB), magic 467 ms
  (beside, 137,600→184,188 kB), reliary8 4 ms (data-dir `8abd3fb9…/`, flat);
- restart ×2: `warm 3` each, zero normalize log lines (warm = metric-only by design);
- hook sets byte-identical 8/9/1 at first and last boot (K3 parity holds through D1);
- kill-switch boot: `disabled 3`, zero normalize lines, all plugins still loaded (raw);
- wipe → restart → `built 3` again; metric present on every boot; health 200;
- spike leftovers in the reliary8 repo auto-removed by stale-pair cleanup (repo
  `git status` clean); `<data>/normalized/` holds exactly one key (path-hash).

Residuals (accepted, not hidden): **R1** rolldown panic in-process aborts ocserve
(`panic = "abort"`) — gated by the kill switch; S4 fed syntax errors/cycles/garbage/
50 MB files with zero panics. **R2** beside-entry writes land in opencode's package
cache — same trust boundary as executing those entries (derived, never fetched).
**R3** cold rebuild after a plugin update ≈ 0.6 s, logged with RSS before/after.
Error-string retry across bun/node/deno dialects stays rejected; named trigger: a
future plugin load failing module-not-found ⇒ scanner rule or retry, then.

**Post-deploy incident (2026-10-06 11:00:16, during battery follow-up):** the unit
OOM-bounced once. Attribution — **not D1**: bundle is byte-identical (K2/K3), normalize
runs in ocserve-main not the sidecar; soak CSV shows the real mechanism: fresh sidecar
warm-up (365 MB @10:55) + magic-context `event`-hook embedding a large *synced* turn
(6 msgs / 15 parts at 11:00:09) stacked bun to 614 MB anon → 750M cap → kernel killed
bun (`oom_score_adj=500` = A3 correct). Gap: default `OOMPolicy=stop` then failed the
WHOLE unit ("oom-kill" → restart). Fix: `OOMPolicy=continue`, proven with synthetic
control units (default → `failed/oom-kill`; continue → `active`, MainPID alive;
journal 11:20:04). Sidecar episodes are transient (settles ~105 MB); cap unchanged at
750M — raised only with new evidence, per the B-phase rule.
