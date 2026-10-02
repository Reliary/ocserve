# refine — memory management spec ("so lightweight")

Companion to `PLAN.md`. Hard contract: **RSS < 300 MB steady-state (refine process only;
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
| ~~rquickjs isolates~~ → **rejected at gate** (PLAN §10 gate result); Node sidecar lives *outside* this budget | 0 MB | child RSS scraped via `rss_bytes{component="sidecar"}` (≤80 MB target) |
| zstd/sha256/import chunk buffers (≤1 MB × 2 concurrent) | 12 MB | fixed-size buffer pool, `BytesMut` reuse |
| Allocator retention headroom (mimalloc purge) | 25 MB | `MIMALLOC_PURGE_DELAY=500` |
| **Unallocated headroom** | **~146 MB** | absorbs spikes; soak asserts the *slope*, not the peak |
| **Total** | **300 MB** | |

Node plugin sidecar (PLAN §10 gate result — quickjs rejected on import audit): declared
*outside* refine's 300 MB; `refine_sidecar_rss_bytes` scraped from the child each sample
interval so the combined number is always visible.

**Measured boundary (2026-10, 4 configured plugins, node v25.9.0, this host):** node
baseline 40 MB (`--max-old-space-size=64 --max-semi-space-size=2`, isolated probes:
context-mode 59 MB, reliary8 54 MB alone) → **146 MB combined sidecar steady-state**,
uncapped same configuration was 148 MB (the heap cap contains growth, not baseline).
Live cgroup series under load (release build, systemd unit): **steady 341-347 MB**
(refine + sidecar together). A first-run long generation burst tripped a 480M cap →
systemd oom-kill → clean restart; the unit cap was raised to **600M** with the
margin documented in `deploy/refine.service` (fail-fast worked; the cap was tight
for allocator page retention during delta bursts).
The original ≤80 MB target is **not achievable with these artifacts** on any single
node process; boundary set to measured + headroom: sidecar **hard cap 160 MB** (systemd
`MemoryMax` in the unit), combined refine+sidecar budget **460 MB**. Any future plugin
addition must re-measure (gate: sidecar ≤160 MB after load, else a plugin is dropped or
loads lazily).

## 2. Global allocator

- **mimalloc** (`#[global_allocator] mimalloc::MiMalloc`) — matches both tuned repos
  (`reliary8/main.rs:5-6`, `stria/main.rs:1-2`), eager page purge is the point: freed pages
  return to the OS on a timer instead of arena hoarding (glibc default: never).
- `MIMALLOC_PURGE_DELAY=500`, `MIMALLOC_ARENA_EAGER_COMMIT=0` set in the systemd unit.
- Never rely on `malloc_trim` / glibc returning memory; if profiling is ever needed, the
  jemalloc profile build is an opt-in feature flag, not the default.

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
- `clippy::unwrap_used` denied in `refine-http`/`refine-core` (panic=abort ⇒ a panic is an
  outage — fail fast with context, not silently die on a poisoned row).

## 5. Measurement and gates

- `/proc/self/status` VmRSS polled every 10 s into `rss_bytes` gauge; per-phase markers
  (`boot`, `import`, `steady`, `soak`).
- Import KPI measured as: import → idle 10 min → RSS ≤ steady budget (mimalloc purge has run;
  raw peak during import may transiently exceed — reported separately as `rss_peak_bytes`).
- CI soak gate (10 min accelerated per PR; 24 h nightly):
  - every sample < 300 MB; `VmSwap` == 0; slope after hour 1 < 1 MB/h;
  - `sum(sqlite cache_size) ≤ 32 MB` asserted at boot.
- Weekly `dhat` profile run attributes any growth to a call site (advisory, then gate).

## 6. Streaming invariants (what makes the budget possible)

1. No code path materializes a full message-part payload > 8 MB — enforced by the parse cap
   and by the 122 MB row never being readable as one value (blob store chunking).
2. No connection checkout across `.await` (also a WAL invariant — STORAGE.md §3).
3. No `SELECT *` on parts/events; range reads only (`substr` windows / `blob_open`).
4. Every channel bounded; every cache capped; every pool sized; documented in code next to
   the bound (a bound that isn't named is a bug waiting to happen).
