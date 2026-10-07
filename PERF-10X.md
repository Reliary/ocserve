# PERF-10X — make refine ≥10× faster than upstream opencode (L1 read paths)

Pre-registered contract. Derived from baseline `bench/load/.runs/20261006T225404Z`
(env + numbers + confounds: `bench/load/BASELINE.md`) and `bench/load/thresholds.json`.
Status labels are current as of the commit carrying this file; update on state change.

## 0. Target

| | freeze 1.18.31 (recorded) | refine (baseline) | refine (goal) |
|---|---:|---:|---:|
| throughput @25VU closed | 424–454 rps | 53–60 rps | **≥4,480 rps plateau** (ladder, below) |
| pooled p95 | 91.8–103.9 ms | 1198–1256 ms | **≤104 ms at plateau** |
| error rate | 0.0000 | 0.0000 | ≤0.005 |

**"10×" definition (locked):** refine plateau **≥4,480 rps** = 10× freeze's *recorded*
454 rps @25-VU closed baseline. Freeze re-run same configs = context, **no denominator shift**.

## 1. Acceptance (pre-registered)

- **A — capacity:** closed ladder plateau ≥4,480 rps, pooled p95 ≤104 ms at that rung, err ≤0.5%.
- **B — coordinated-omission-free latency:** k6 `constant-arrival-rate` run, `rate=4480`,
  ≥60 s: achieved ≥99% of offered, p95 ≤104 ms, `dropped_iterations=0`, err ≤0.5%.
  (Closed-model runs prove capacity only; open-model run carries the latency claim —
  Tene/k6 open-vs-closed: `ramping-vus` = throughput probe, arrival-rate = CO-free.)
- **C:** A+B + all gates green (46 suites, guards, replay 26/0, pair 22/0), thresholds
  re-derived from the **final binary's** baseline and committed before any `GATED=1` run.
- **D — cache honesty:** every report that includes memoized paths also carries the
  cache-off variant (`REFINE_SEARCH_MEMO=0` / `REFINE_LIST_MEMO=0`) + the fixed-query
  caveat (k6 repeats `"the"`; real interactive hit rates are lower).
- **Stop rule:** if ceiling < A/B → ship the honest frontier (rps, p95, per-route, both
  models, cache on/off) + named next levers. No threshold-gaming, no claim laundering.

## 2. Evidence base (measured, this box / fixture)

- Exact `search_parts` SQL on fixture: **7.05 s** (6.7 warm) — match set `"the"` =
  **58,434/178k parts**; EQP = 58k PK lookups + per-row join + TEMP B-TREE sort carrying
  `ps.text` payloads. `temp_store` pragma value `2` = **MEMORY** (pragma.rs:48 comment
  says FILE — code/comment mismatch; sorts never hit disk).
- Deep-session page service: msg-select 0.000 s + 51 part-queries **0.011 s** → the
  1.2 s tails are **queueing**, not query cost.
- Convoy root: `worker_threads(8)` hardcoded; `spawn_blocking` ×1 in refine-http;
  all store calls block tokio workers inline (search holds a worker 1–7 s → every
  other route queues behind it — matches MS/tokio starvation signature: tail explodes,
  CPU ≪100%).
- 12× `payloads.read().<x>.clone()` deep `serde_json::Value` clones per request
  (config/agent/command/config_providers…).
- No prepared-statement cache (21 `prepare(` sites; search SQL `format!`-built/request).
- fts5 supports `ORDER BY rowid [ASC|DESC]` + rowid `> < =` range constraints
  (fts5_main.c) — but `part_search` rowids are **insert-order** (INSERT has no rowid).
- SQLite bundled 3.53.2 (clang-22.1.3): `ENABLE_FTS5`, `ENABLE_STAT4`, `DBSTAT`,
  `DIRECT_OVERFLOW_READ` present; `SQLITE_DEFAULT_MEMSTATUS` not disabled.
- No `.cargo/config.toml`, zero RUSTFLAGS → Rust **and** cc C code (sqlite3.c, zstd)
  emit baseline SSE2 only. Profile already maxed (opt3/fat LTO/cgu=1/panic=abort/strip).
- `TCP_NODELAY` never set explicitly; hot-path debug!/trace! ≈0 (release_max_level moot).
- Live upstream thread (sqlite forum Jan–Feb 2026): FTS5 plan regression from check-in
  2025-09-15 with **our exact EQP shape** (M1 scan + TEMP B-TREE) — E2 bisect decides.
- FTS5 `detail=` reference: index 743→340→134 MB (full→column→none); we never call FTS
  auxiliary functions (snippets come from `part_search.text`).

## 3. Design (fixes, ranked)

**F1 — offload blocking DB (convoy killer).** Every store call in async handlers via
`spawn_blocking` (bounded `max_blocking_threads` sized against TLS reader-cache budget:
threads × 4 MB vs .227 MemAvailable w/ llama co-tenant). + bounded TLS prepared-statement
cache (2–3 stmt variants). Pre-registered fallback if post-fix p99 shows shared-pool
contention: dedicated reader threads (tokio-rusqlite pattern; benchmark evidence:
spawn_blocking ≈ throughput, dedicated ≈ better p99).

**S-A — search rewrite (E1 decides shape).** Candidates:
- **P-A:** time-derived rowids (`rowid = dense_rank(time_created, tiebreak)`) + pure
  `MATCH … ORDER BY rowid DESC LIMIT` walk; payload+join for ≤51 only. Kills 58k
  lookups/joins/sort and the separate time column (rowid *is* the time). Contract-order
  verified differentially (old vs new full result lists, same-ms ties included).
- **P-B:** `time_created` column + `(time DESC)` covering index + ephemeral rowid-set
  membership + early stop. Keeps 58k set enumeration ⇒ ~50 ms floor (backup).
- **P-C:** rowid-windowed descent (bounded `rowid >= W` windows from top until LIMIT).
  P-A semantics, planner-independent, Rust-side loop.
Gates: all search contract tests; differential ORDER diff vs current query = 0 on
fixture battery; planted control (drop rowid-order/index → timing red).
Schema step + idempotent boot backfill (W1 pattern).

**F5 — pre-serialized wire bytes (definite).** `payloads` → `Arc<Bytes>` serialized at
reload; handlers serve `Body::from(Bytes)` (zero-copy). serde deterministic ⇒ corpus
byte modes stay byte-identical. Kill: `REFINE_WIRE_CACHE=0`.

**F3** bulk page-parts (`message_id IN (…)` 51→1). **F4** pool `/message`'s per-request
`std::thread::spawn` → spawn_blocking. **F7/F8** write-epoch memoization (single writer,
epoch bump in-transaction ⇒ exact, zero staleness): search results + session list/config
rebuilds. Planned (not trigger-gated): the 50 ms FTS floor alone exceeds the VU50 mean
budget (11.2 ms) — arithmetic, not gaming. Dual reporting per §1-D.

**A1 — SATISFIED BY EXISTING CODE (no change):** `PRAGMA optimize 0x10002`
already runs at every writer open and `PRAGMA optimize` hourly
(main.rs optimize tick) — analyze-if-stats-thin semantics, zero new code.

**B1 — TRIGGERED AND SHIPPED (428d13f):** deep-page probe on the fixture =
13 blob reads per deep page (245 parts, 13 >8 KiB) ≥ 1 ⇒ condition met.
Process-global byte-capped FIFO (32 MiB, `REFINE_BLOB_CACHE_MB`, oversized
entries never admitted); content-addressed keys = immutable = exact by
construction.

**temp_store — DONE (428d13f):** value was `2` (= MEMORY) miscommented as
FILE; set to `1` (FILE) per the S-A follow-through above.

**C1+C4 A/B — FAIL KEEP-RULE, REVERTED.** Sandwich at VU50/ROUNDS=1 on
.227 (pinning + quiet gate + both arms each run): A1 pre-C 2,612/2,868
req/s p95 68/62 ms → B post-C 1,811/2,063 req/s p95 101/90 ms → A2 pre-C
1,809/2,067 req/s p95 101/91 ms. Post-C ≈ pre-C **to 0.1%** (B vs A2) —
measurement proven valid (release `sqlite3.o` rebuilt 09:39, after the
config at 09:31 ⇒ CFLAGS reached sqlite3.c; full Rust recompile also
observed). Keep-rule "A/B measurable" not met ⇒ `.cargo/config.toml`
removed. A1's 40% lead over A2 (identical binary) = the fixture
asymmetry in warm-page-cache form: A1's refine db was still resident
from its own import; B/A2 read cold (refine db 1.7 GB vs freeze 485 MB —
freeze held ~470 req/s across all three runs, the stable control). C5
(worker floor) kept as declared-trivial (no-op at 8 logical cores on
.227). C6 already skipped with floor-probe evidence. C2 (PGO) parked
pending Phase-III plateau — revisit only if the ladder stalls short of A.

**E0 — FOUND (initially mis-searched at `refine-src`; the `.227` checkout
is `~/src/refine`) — RESULT: the config mystery is SOLVED and F1's
hypothesis is PROVEN.** Run `20261007T004510Z` (config-route isolation,
single round, both arms, pinned homogeneous cores): **refine 9,655 req/s,
p50 0.90 ms, p95 4.42 ms** vs **freeze 3,152 req/s, p50 4.31 ms, p95
11.14 ms**, 0% errors both arms. The baseline's config p95 of 998 ms was
therefore ~99.6% queueing (convoy behind the multi-second search/page
work on shared workers) — the route itself was always ~4 ms. Refine is
3.1× faster than freeze on this isolated route. Remaining E0 steps
as separate run dirs... FULL TABLE (chain `e0-chain.log`, k6 summary
extraction from the per-route logs — pre-F1 binary, both arms, pinned):

| route | refine req/s | refine p95 | freeze req/s | freeze p95 |
|---|---:|---:|---:|---:|
| config | 9,656 | 4.42 ms | 3,152 | 11.14 ms |
| search (selective query) | 5,422 | 7.85 ms | 5,486 | 7.86 ms |
| session_list | 1,099 | 31.6 ms | 145 | 172.0 ms |
| message_page | **incomplete** (chain log truncated before k6 summary; progress lines only) — superseded by Phase III per-route tables |

Reading: refine already led config (2.8×) and session_list (7.6× rps,
5.4× p95); search tied at ~5.4k req/s both arms (selective query — the
6-7 s figure in E1 was the `"the"` match-set pathology, not this shape);
the pooled-baseline pain (config p95 998 ms) was queueing behind those
routes on shared workers, exactly F1's diagnosis.
**temp_store** decision: after S-A removes large sorts → set `1` (FILE) per the stated
bounded-memory intent, fix the comment either way.

## 4. Compiler track (Phase IV — before final acceptance runs)

| ID | Change | Keep rule |
|---|---|---|
| C1 | `.cargo/config.toml`: `-Ctarget-cpu=x86-64-v3` + `CFLAGS_…=-march=x86-64-v3` (Rust+sqlite3.c+zstd). **Explicit v3, never `native`** (MTL-built native ≠ TGL-safe; v3 = AVX2/BMI2/FMA, safe both) | A/B measurable; min-ISA documented in SRE (both targets qualify) |
| C4 | add `-DSQLITE_DEFAULT_MEMSTATUS=0` via CFLAGS env (build.rs does not override → no libsqlite3-sys patch). `SQLITE_OMIT_GET_TABLE` **evaluated and rejected**: rusqlite never exposes `sqlite3_get_table`, but an omitted symbol is a link-time risk for any C dep that might reference it, for negligible size/win | A/B measurable |
| C5 | `worker_threads` → `max(8, available_parallelism)` | trivial |
| C6 | `.set_nodelay(true)` on listener (floor probe decides need) | probe — **SKIPPED with evidence**: baseline summary `http_req_blocked` p99 = 0.014 ms, `http_req_sending` p95 = 0.018 ms, `http_req_connecting` p95 = 0 ⇒ loopback floor already sub-millisecond, nothing for nodelay to win |
| C2 | PGO script (`scripts/pgo-build.sh`): profile-generate → training (replay + pair + local single-route k6 + import smoke) → profile-use. Requires `rustup component add llvm-tools-preview` (matched llvm-profdata; system LLVM-22 one risky) | re-runnable, documented cadence |
| C7 | size ceiling: PGO +10–30% `.text` likely trips 20,971,520 B → bump only by standing rule (measurement + provenance comment) | standing rule |
| C3 | BOLT: **deferred** — tooling absent (needs perf + llvm-bolt install approval) | — |

Sequence forced: Phase II code → Phase IV compiler (profile the *shipped* code, one
build, A/B each keep) → Phase III acceptance on the optimized binary → thresholds from
that baseline → GATED.

## 4b. Phase-III results (2026-10-07, full tables in bench/load/BASELINE.md addendum)

- **A PASS**: closed pooled ladder 50/75/150 @4-threads = refine
  9,234/8,140 req/s, p95 25.3/32.4 ms vs freeze 466/367 — 19.8-22.2x;
  vs freeze recorded 454 = **20.3x** (acceptance C passes on closed).
- **B FAIL as pre-registered**: four arrival attempts at rate=4,480,
  64.7-98.6% achieved, p95 89-546 ms; never >=99% AND <=104 ms
  together. Best joint = attempt 1: 97.7% @ p95 92.5 ms. Later attempts
  degraded with runner state (freeze itself fell 432->220 req/s, RSS
  7.6 GB) — all four recorded, none discarded.
- **D**: thresholds re-derived pre-gated (edeac9f, refine <=65 /
  freeze <=1417 / err <=0.5, formula on run 132925); G1 gated ran with
  a STALE runner copy of thresholds (refine cells 25.97/32.95 pass;
  freeze 549/587 breached the old 208) — synced + one pre-declared
  cooldown re-run (G2), both recorded.
- **Dual-variant (1-D)**: cache-off (all kill switches, VU75, 4T):
  2,198/2,708 req/s p95 116/106 — memos = ~3-4x of throughput; every
  claim above is cache-on and labelled as such.
- Isolation table (config 24.3k @ 0.011 ms CPU/req; page 2.54 ms
  cpu/req -> F9; list 1.33 -> wire memo) and C-track A/B disposition
  (C1+C4 reverted) in the BASELINE addendum.

## 5. Phases

**Phase I — decide by measurement (one session):**
- E0 `LOAD_ROUTES` single-route k6 isolation on .227 (search/page/config/list × arms) —
  service-time vs convoy; config-vs-agent anomaly resolves here.
- E1 P-A/P-B/P-C shootout on a **scratch copy** of the fixture db (cold+warm, EQP each,
  differential ORDER diff) → S-A shape chosen by number.
- E2 FTS5 version bisect (3.50.4 vs bundled 3.53.2 on our query, scratch builds) —
  only decision-relevant if S-A keeps an order-by-time+join plan shape.
- E3 `detail=none|column` rebuild on scratch + 50-query differential battery
  (single/multi-word/edge) byte-diff vs detail=full; trigram×detail token-length rule
  checked. Any diff → reject.
- stat4 row counts post-import; blob-reads-per-page probe (B1 decision); single-VU
  floor probe (C6 decision).
Checkpoint: Phase I results to user before Phase II code.

**Phase II — fixes (each own commit, full gates + planted controls):**
F1+stmt-cache → S-A → F5 → F3/F4 → F7/F8 → A1 → temp_store → B1/E3 only if passed.

**Phase IV — compiler track:** C1+C4+C5+C6 → A/B → C2 PGO → A/B (each keep-rule).

**Phase III — prove it (.227):** closed ladder `10,25,50,75` (k6 client CPU in sampler;
≥80% client-cpu ⇒ widen client before declaring server ceiling) + open arrival headline
runs + freeze same-config context runs → re-derive `thresholds.json` (both models,
commit) → `GATED=1` → report (both cache variants, claim classes printed).

## 6. Non-goals (measured reasons)

jemalloc/mimalloc reopen · io_uring (SQLite is pread-bound) · sonic-rs SIMD JSON (trigger:
only if post-C2 profile still shows serde on top) · `mmap_size=0` change (STORAGE
rationale first) · `target-cpu=native` (cross-machine ISA risk) · BOLT until tooling ·
`-Zbuild-std`/nightly flags · HTTP/2+compression (h1 loopback) · async-SQLite rewrite ·
system tuning (governor/THP/swappiness) · contentless FTS5 (M0-proven broken on 3.53.x)
· jemalloc-sized claims without A/B.

## 7. Research provenance

Tene coordinated-omission / k6 open-vs-closed + arrival executors (B gate, dropped gate) ·
Little's Law (ladder budgets: VU25=5.6ms, VU50=11.2ms, VU75=16.8ms mean @4480) ·
VLDB'25 Selective Late Materialization + ParadeDB #4154 + top-k survey (S-A: payload after
LIMIT, sort keys only in walk) · SQLite fts5 docs/detail table + fts5_main.c rowid-ORDER
support (P-A/E3) · sqlite forum FTS5 3.51.0 regression thread (E2) · rust-sqlite-async
benchmark + tokio/MS starvation docs (F1 + fallback) · WiscKey/size-as-cache (fixture
confound stays disclosed, E3 attacks it structurally).
