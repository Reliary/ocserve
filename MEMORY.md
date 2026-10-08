# ocserve — memory management spec ("so lightweight")

Companion to `PLAN.md`. Hard contract: **RSS < 300 MB steady-state (ocserve process only;
+80 MB if the Node plugin sidecar is active — declared boundary, both measured),
zero swap growth over 24 h**.

## 1. Reconciled budget (fixes the fatal cache×pool arithmetic)

The first draft gave `cache_size=-262144` (256 MB) — *per connection*; with a default pool
that alone exceeds the entire budget. Corrected line items:

| Line item | Budget | How it's enforced |
|---|---|---|
| Binary + code pages + static | 25 MB | measured at boot, `rss_bytes{phase="boot"}` |
| SQLite page caches: writer 16 + 4 readers×4 | **32 MB** | role-scoped `cache_size` (STORAGE.md §2); summed at boot, asserted ≤32 MB |
| tokio: 8 workers × 1 MB stacks + blocking pool 8 × 1 MB | 16 MB | `thread_stack_size(1MB)`, `max_blocking_threads(8)` |
| hyper/reqwest/rustls: ≤16 pooled conns | 12 MB | `pool_max_idle_per_host(4)`, `pool_idle_timeout`, global conn semaphore |
| SSE fanout: bounded per-subscriber ring (4096 events × ~300 B avg) + disk spill pointer | 2 MB | `broadcast` capacity bound; overflow policy below |
| ~~rquickjs isolates~~ → **rejected at gate** (PLAN §10 gate result); Node sidecar lives *outside* this budget | 0 MB | child RSS scraped via `ocserve_sidecar_rss_bytes` (160 MB hard cap; measured 106 MB live 2026-10-04) |
| zstd/sha256/import chunk buffers (≤1 MB × 2 concurrent) | 12 MB | fixed-size buffer pool, `BytesMut` reuse |
| Allocator retention headroom | (unmeasured) | **glibc/system allocator — the old "mimalloc purge / MIMALLOC_PURGE_DELAY=500" row was FALSE (no `#[global_allocator]` exists in any crate; corrected 2026-10-04).** Wiring-vs-adopt decision deferred to bytehound profiling (phase 1); retention watched via soak `rss_delta` columns |
| **Unallocated headroom** | **~146 MB** | absorbs spikes; soak asserts the *slope*, not the peak |
| **Total** | **300 MB** | |

Node plugin sidecar (PLAN §10 gate result — quickjs rejected on import audit): declared
*outside* ocserve's 300 MB; `ocserve_sidecar_rss_bytes` scraped from the child each sample
interval so the combined number is always visible.

**Measured boundary (2026-10, 4 configured plugins, node v25.9.0, this host):** node
baseline 40 MB (`--max-old-space-size=64 --max-semi-space-size=2`, isolated probes:
context-mode 59 MB, reliary8 54 MB alone) → **146 MB combined sidecar steady-state**,
uncapped same configuration was 148 MB (the heap cap contains growth, not baseline).
Live cgroup series under load (release build, systemd unit): **steady 341-347 MB**
(ocserve + sidecar together). **Orphan incident (2026-10-04): replay/gate tooling left 34
plugin-host nodes / 717 MB orphaned when the parent ocserve exited without reaping —
fixed in the tooling (setsid + group-kill in `replay-check.sh`, `start_new_session` +
killpg in `run_gate.py`); service-side reap-on-exit ships with phase 1.**
A first-run long generation burst tripped a 480M cap →
systemd oom-kill → clean restart; the unit cap was raised to **600M** with the
margin documented in `deploy/ocserve.service` (fail-fast worked; the cap was tight
for allocator page retention during delta bursts).
The original ≤80 MB target is **not achievable with these artifacts** on any single
node process; boundary set to measured + headroom: sidecar **hard cap 160 MB** (systemd
`MemoryMax` in the unit), combined ocserve+sidecar budget **460 MB**.

**Bun runtime (2026-10-05, TI-86 pass):** the plugin host now prefers **bun**
(`~/.bun/bin/bun --smol`; upstream's own plugin runtime — real `bun:sqlite`, native
hash/crypto, TS plugins load without the Node loader shim). `OCSERVE_PLUGIN_RUNTIME=node`
forces the old path (lab A/B control); Node remains the fallback when bun is absent.
**Live A/B, same 3 plugins, same box — honest settled numbers (30 s snapshots lie):**
steady state is a **wash** — bun and node both settle ≈97–101 MB (3–5 min). The
difference is WARM-UP: bun peaks ~260 MB vs node ~137 MB while magic-context lazily
initializes its embedding stack (@huggingface/transformers + onnxruntime + sharp,
87 MB Xenova model on disk), then returns to baseline. The original short-settle
"87 vs 138" was a mirage and is retracted. Bun remains the default on fidelity
grounds (upstream runtime; gemini-plugin class loads without the Node TS limitation).
Hook dispatch verified under bun end-to-end (chat.message ×2, transform ×2,
text.complete ×2; live prompt answered). `MemoryMax` headroom absorbs the warm-up
peak; cold-start budget note: first 3–5 min after boot may sit ~260 MB sidecar.
Node 128 MB heap cap no longer applies under bun (`--smol` instead);
`OCSERVE_PLUGIN_HEAP_MB` is the Node-fallback knob.

**OOM postmortem (2026-10-05, kernel memcg kills ×6: 10:36/11:16/12:20/14:12/15:12/16:05):**
the *unit* cap is what binds (main + ALL children), and the measured warm daytime
composition blew past the old budget: **main ≈ 487 MB (flat plateau) + plugin host ≈
145 MB + browser-harness python ×2 ≈ 78 MB ≈ 680 MB against `MemoryMax=750M`** —
~118 MB headroom, and `ocserve_sidecar_rss_bytes` counts the plugin host only (bh/shim
invisible to the old soak). Spike classes: catalog reload (their opencode writes
`models.json` ~hourly; write → kill +6 s — the reload parsed the 5.3 MB file **twice**,
now once via `Runtime.catalog`) and activity-composite crossings (morning kills; node
allocating at 16:05). Equal `oom_score` meant the kernel always killed MAIN (full
service death) even when a child allocated — children now raise their own
`oom_score_adj=500` at spawn (A3 wrapper) so a future kill takes a respawnable child.
  A3 alone proved half the story (2026-10-06 11:00:16: kernel killed ONLY bun, yet
  default `OOMPolicy=stop` bounced the whole unit) — `OOMPolicy=continue` completes
  it: child dies → logged → unit stays up → `ensure_alive` respawns. Control A/B
  evidence: FINDINGS "D1 executed" addendum; guard rule 9.
Reconcile is now INFO-logged (files + duration + RSS delta), soak gained
`cgroup`/`cgroup_peak`/`kids` columns, and journald retention was pruned to ~3 h by
the root fs sitting at 6.8% free (< default `SystemKeepFree`10%) — retention floor
staged at `deploy/journald-retention.conf` (500M cap, keep-free override). The
+146 MB main-vs-341 delta is plateau-stable and attributed AFTER the A-data (Phase B).
**Same-day outcome:** warm reload proved lethal — a forced catalog refresh OOM-killed the
service 2 s later (kill #7, journal 17:01:33, +237 MB measured by the new A2 log) →
(A5) `watch` now content-hashes on tuple-diff: identical-bytes rewrites (their hourly
identical fetch) advance the tuple with `result="skipped"` and never reload (plant-proven);
and `MemoryMax` raised **750M → 1024M** as a measured interim (predicted peak ≈951 MB =
warm 500 + reload build 237 + children 214; both live and repo units). Phase B re-measures
and may lower it again.
**Phase B verdict (same night): FRAGMENTATION, not a leak** — lab A/B
(`bench/profiling/oom_reload.py`, full-size fixtures): plain arenas step
+220/+68/+1 to a 526 MB plateau; `MALLOC_ARENA_MAX=1` collapses it to
+69/0/+1 at **308 MB** (bytehound: 24.3M of 25M allocs = temporary churn;
leaked-at-exit = the live payload `build_providers` Value, by design).
Wired as `Environment=MALLOC_ARENA_MAX=1` in the unit (zero-code; children
inherit — reload wall-time unchanged); **cap returned to 750M** (measured:
idle main 231 / cgroup 402 ≤460 budget; production reloads +90 cold then
+0.2 warm). 8 kernel kills total that day (last 21:13, before the fix).
Full evidence: `bench/profiling/OOM-RELOAD-REPORT.md`.

**V8 heap cap (2026-10-03, P0 battery):** at `--max-old-space-size=64` the real
5-plugin set OOM-killed the sidecar once at boot (observed: `FATAL ERROR: Reached
heap limit`, child zombie, every subsequent hook `Broken pipe` — fail-open hid it
inside prompts). Controlled re-measure (4/5 plugins loading — gemini fails
independently —10s+ settle, trigger+event exercised): RSS **flat 144-148 MB at
64/128/192/256** — the V8 cap never binds baseline RSS (native/WASM dominates), so
headroom is free. Default raised to **128** (`OCSERVE_PLUGIN_HEAP_MB` overrides), and
`Sidecar::ensure_alive` now respawns + replays all loads on the next RPC after any
death (`sidecar_respawns_after_death_and_reloads_plugins`). The160 MB systemd
`MemoryMax` remains the hard boundary. Any future plugin
addition must re-measure (gate: sidecar ≤160 MB after load, else a plugin is dropped or
loads lazily).

## 2. Global allocator

**Declared runtime setting (2026-10-05): glibc with `MALLOC_ARENA_MAX=1`**
(single arena — unit `Environment`). Justification + A/B numbers:
`bench/profiling/OOM-RELOAD-REPORT.md`. glibc stays the allocator; mimalloc
remains a contingency only if single-arena contention shows up in soak CPU
(lab showed no wall-time regression).

- **glibc/system allocator (FACT, 2026-10-04)** — `grep -rn "global_allocator|mimalloc"`
  across every crate: **no `#[global_allocator]` exists**. The previous claim that mimalloc
  was wired "like reliary8/stria" was a false status label (their main.rs lines are real;
  ours never landed). Behavior today = glibc malloc (arena hoarding possible; the
  step-function RSS retention seen in the 2026-10-04 morning soak predates the drain fix
  and must be re-measured before any allocator choice). **Decision deferred to bytehound
  profiling (phase 1): wire mimalloc as designed, or adopt glibc honestly and delete the
  aspiration.** Either way the unit's dead `MIMALLOC_*` env is removed until then.
- ~~`MIMALLOC_*` env lines in the unit~~ — **dead config until an allocator is
  actually linked**: no `#[global_allocator]` exists, so the unit's env never
  reached an allocator (corrected 2026-10-04; lines removed from the unit,
  allocator wiring decision = phase 1 profiling output).
- Never rely on `malloc_trim` / glibc returning memory. Profiling = bytehound via
  LD_PRELOAD against glibc (works today, no code change); a jemalloc-profile build
  feature does NOT exist yet — do not reference it as if it does.

## 3. Tokio runtime (explicit, not defaults)

```rust
tokio::runtime::Builder::new_multi_thread()
    .worker_threads(8)            // default = 22 CPUs = 44 MB of stacks alone; 8 is ample for a proxy/agent server
    .thread_stack_size(1 << 20)   // 1 MB (default 2 MB); async tasks are shallow
    .max_blocking_threads(8)      // DEFAULT IS 512 × 2 MB = 1 GiB worst case — the #1 RSS bomb
    .enable_all().build()
```
- rusqlite calls are blocking → they go through a **dedicated small blocking pool** guarded by
  a semaphore; never `spawn_blocking` unbounded in loops.
- Rayon (import/hashing paths): capped `min(8, parallelism)` with env override — the exact
  reliary8 pattern (`main.rs:2043-2059`, WSL2/ARM oversubscription lesson).

## 4. Allocation discipline (lints + review checklist)

- No `unbounded_channel` on any request/event path — `tokio::sync::mpsc` with bounds,
  SSE via `broadcast` with bounded capacity.
- Large payloads: `bytes::Bytes` / `BytesMut` from a small reuse pool; `freeze()` → zero-copy
  fanout; never `Vec<u8>` copies of tool output across layers.
- JSON: `serde_json::from_slice` only under a **8 MB per-parse cap** (bigger → chunked path);
  borrowed `&str`/`Cow` fields on hot read paths (session list, event encode).
- Strings: `Arc<str>` for shared session/project ids; `internment` only for genuinely
  repeated identifiers (tool names, agent names) with a hard intern-table cap.
- Collections: `SmallVec` for ≤4-element hot vectors; bounded `LruCache` (never `HashMap`
  that only grows); caches hold `Weak` where ownership allows (no `Arc` cycles).
- `clippy::unwrap_used` denied in `ocserve-http`/`ocserve-core` (panic=abort ⇒ a panic is an
  outage — fail fast with context, not silently die on a poisoned row).

## 5. Measurement and gates

- `/proc/self/status` VmRSS polled every 10 s into `rss_bytes` gauge; per-phase markers
  (`boot`, `import`, `steady`, `soak`).
- Import KPI measured as: import → idle 10 min → RSS ≤ steady budget (allocator purge/decay observed;
  raw peak during import may transiently exceed — reported separately as `rss_peak_bytes`).
- CI soak gate (10 min accelerated per PR; 24 h nightly):
  - every sample < 300 MB; `VmSwap` == 0; slope after hour 1 < 1 MB/h;
  - `sum(sqlite cache_size) ≤ 32 MB` asserted at boot.
- Reload gate (`bench/profiling/oom_reload.py`, run on allocator/reload
  changes): full-catalog boot settle ≤ 120 MB; `OCSERVE_TRIM=0` control must
  show the unfixed behavior (boot ≥ 230 MB) — a control that stops stepping
  means the lab lost its trigger, not that the bug vanished.
- Weekly `dhat` profile run attributes any growth to a call site (advisory, then gate).

## 6. Streaming invariants (what makes the budget possible)

1. No code path materializes a full message-part payload > 8 MB — enforced by the parse cap
   and by the 122 MB row never being readable as one value (blob store chunking).
2. No connection checkout across `.await` (also a WAL invariant — STORAGE.md §3).
3. No `SELECT *` on parts/events; range reads only (`substr` windows / `blob_open`).
4. Every channel bounded; every cache capped; every pool sized; documented in code next to
   the bound (a bound that isn't named is a bug waiting to happen).

## 7. Phase-1 backlog (after the 24 h soak gate) + phase-0 profile

Phase-0 evidence: `bench/profiling/REPORT.md` (bytehound over glibc, debug
build, 40 s mixed load — **4.48M allocs / 99.5% churn**, top groups =
per-fetch SQLite pcache in `get_messages` + `serde_json::to_string` frame
building). Ordered by measured leverage:

1. ~~Reused serialization buffers~~ **SHIPPED** — `/message` frames serialize
   into one reused `Vec<u8>` per fetch (+ blob `from_slice`, no lossy copy);
   prompt request bodies pre-sized via `estimate_request_bytes` (one exact
   alloc instead of doubling-growth per round). Tests: `estimate_tests`.
2. ~~Reader-connection reuse~~ **SHIPPED** — `pragma::Reader` parks the
   connection thread-locally (Deref-transparent: zero call-site churn);
   fresh-open accounting proves checkout-vs-open. Test:
   `reader_reuses_parked_connection_per_thread`. Budget note: parked readers
   are per-thread (cache_size unchanged, page residency lazy — pages, not
   4 MB × threads, actually touch).
3. ~~History byte budget~~ **SHIPPED** — `PROMPT_HISTORY_MAX_BYTES = 8 MB`,
   tail-weighted (oldest dropped first, newest exchange always kept),
   `ocserve_history_truncated_total{dropped}` + warn log; under-budget =
   byte-identical. Tests: `history_budget_tests` ×3. Divergence: TESTING §1.6
   D-PROMPT-BUDGET.
4. **Service-side plugin-host reap** — re-verified COVERED without new code:
   `kill_on_drop(true)` already set at spawn; systemd cgroup covers OOM/stop;
   non-systemd exits swept by the tooling fixes. Residual = SIGKILL-embedded
   (swept next run). **Item closed.**
5. **Allocator decision** — glibc adopted honestly (§2); wire mimalloc ONLY
   if the post-fix soak still shows step-retention (pre-fix morning soak
   had +76/+58 MB steps — unproven after the drain fix).
6. Gauges: `ocserve_db_bytes`, opencode-mirror RSS column in soak (native
   opencode = the future-usage ceiling model, currently ~1.7 GB RSS).
7. Per-prompt rss_delta stays last-prompt-honest (concurrent prompts
   contaminate it — documented, not "fixed" by bucketing).

