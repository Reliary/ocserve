# ocserve — SQLite storage spec ("tuned within an inch of its life")

Companion to `PLAN.md`. Every setting here has a rationale and a source. Empirical findings
marked **[TESTED]** were verified on this machine (2026-10-01, sqlite3 3.53.0, Debian build)
by direct execution — not from documentation.

## 0. Version gate (non-negotiable)

- `rusqlite` **≥0.40 with `features = ["bundled"]`** (0.40.2 current) → bundles SQLite ≥3.53.
- Boot assert: `SELECT sqlite_version()` ≥ **3.51.3** (WAL-reset corruption fix, 2026-03-13).
  Failure = refuse to start with actionable message (fail-fast).
- System `libsqlite3` is never used (variability, unknown compile flags).

## 1. FTS5 design — corrected by empirical test

**[TESTED] findings on sqlite 3.53.0:**

| Experiment | Result |
|---|---|
| External-content FTS5 in **attached** file, `content='main.m'` | ✗ create may succeed, but `rebuild`/`MATCH` fail: content name is qualified with the FTS table's schema (`b.main.m`) — **[M0-VERIFIED]** by full lifecycle test |
| Trigger on main table writing attached FTS | ✗ `qualified table names are not allowed ... within triggers` |
| Contentless FTS5 (`content=''`) local, `:memory:` and file | ✗ `vtable constructor failed` on system sqlite 3.53.0 **and** rusqlite bundled 3.53.2 (**[M0-VERIFIED]**, `contentless_fts_status_recorded`) — broken across 3.53.x builds, not a distro quirk; contentless designs are off the table |
| Plain FTS5 local + in attached file | ✓ works |
| External-content FTS5 **local** (same file): create → insert → rebuild → match | ✓ works |

**Decision: FTS5 lives in the main DB**, external-content over a slim local projection table
(`search_doc(session_id, title, excerpt, updated_at)`), synced by the writer task in the same
transaction as the domain write. No attached search file — cross-file atomicity under WAL is
not transactional (SQLite docs) and would need a rebuild-parity protocol for zero benefit at
this scale. Write amplification is absorbed by the ≤50 ms writer batching.

- Tokenizer: `unicode61` for titles; add a **trigram** index only if code-fragment search is
  needed later (reliary8 *removed* FTS5 trigram for its workload — measured, not dogma; here
  titles/messages are the corpus, so external-content + unicode61 is the default).
- `detail=column` if index size shows up in soak; measure before changing.
- Compaction: incremental `INSERT INTO f(f, rank) VALUES('merge', ±N)` in idle windows —
  never the all-btree `optimize` (long transaction).

### 1.1b Read-path JSON assembly (PERF-10X Phase I/II L1)

`msg.info` and `msg_part.inline` are stored JSON blobs; the response merges
three column keys into each. That was done with `serde_json::Value`
(parse → insert → re-serialize), which bytehound attributed **86% of
read-path allocations**. `ocserve-store/src/splice.rs` now compacts the
stored bytes in one pass into a **reused** buffer and splices the column
keys — no `Value`, no `HashMap`, no per-row re-serialization.

**Not a passthrough, on purpose:** serde's compact formatter writes `,`
and `":"` with no whitespace (`serde_json-1.0.150/src/ser.rs:1884-1893`),
and re-formats numbers through ryu, so the DOM path *normalizes*. Upstream's
Bun writer emits `": "`/`", "` for 39% of stored rows, so copying bytes
verbatim would change the wire. The splicer therefore compacts whitespace,
re-emits non-canonical escapes/floats through serde, and **refuses** (DOM
fallback) on anything unprovable: non-object top level, unbalanced brackets,
duplicate top-level keys, non-canonical integers > 19 digits, malformed
values.

### 1.1d Boot must not die on write-lock contention (found by the load harness)

The optimize-placement fix (this file §1.2) moved `PRAGMA optimize` onto the
boot path, where it runs on **every** writer spawn. The A/B run immediately
exposed the consequence: with a second connection holding the write lock (the
load harness wipes a stale fixture while another handle lingers),
`pragma_update` returned `SQLITE_BUSY` → `migrate` failed → `ocserve serve`
exited 1 → "ocserve never healthy". `busy_timeout` does not cover it: that
applies to lock acquisition, and a statement-level BUSY on a pragma can
surface immediately.

Maintenance in this path is **statistics work, not schema**, so:

- transient `database is locked` → bounded retry (5 attempts, 100/200/300/400
  ms backoff = 1 s budget), then WARN and continue. Losing stats costs only a
  stale planner until the next hourly tick; losing the boot costs the server.
- any **non**-contention error → still propagates. The swallow is scoped to
  the exact `database is locked` string.

Tests: `tests/lock_retry.rs` — `boot_survives_a_contended_optimize` (real
`BEGIN EXCLUSIVE` blocker, asserts success AND a bounded runtime),
`stats_are_collected_when_the_lock_is_free`, and
`non_contention_errors_are_not_swallowed` (drops `part_search_fts` so the
maintenance statement fails hard). **Planted negative control**: making the
retry absorb *every* error turns the third test red — proven. A first attempt
at that control used `PRAGMA <unknown>`, which SQLite silently ignores, so it
was vacuous and was replaced.

**Gate:** `tests/splice_parity.rs` proves byte-parity against the DOM as
oracle, and `splice_parity_over_corpus` runs the differential over **every**
stored row (`OCSERVE_SPLICE_DB=<db>`): 218,393 rows, 0 refused, 0 mismatched
on the fixture. Live: 5,900 rows spliced, 0 fallbacks. `splice_rows()` /
`splice_fallbacks()` expose the counters; a non-zero fallback count is a
signal, not a silent degradation.

### 1.1c Session-list wire bytes (M1)

`GET /session` (201 rows on the fixture) was serialized through
`Vec<serde_json::Value>`: build 201 trees, then `to_vec`. M1
(`build_sessions_wire_bytes`) writes the members straight from the columns
into one buffer — no `Value` tree, one pass. Numbers and the `model` blob go
through serde itself, so ryu/itoa/escaping is serde's by construction rather
than reimplemented.

**Attribution** (`examples/list_split.rs`, fixture, memo off): SQL row read
74 µs, JSON build 238 µs — so JSON was ~3× the SQL, and M1 cut that build
from ~412 µs (DOM) to 238 µs. The full cold call measures ~312 µs here and
~660 µs in `sqlite_tune_bench`; the difference is `open_reader` (parked
connection checkout + pragmas), which neither path changes.

**Parity gate** (`tests/list_wire_parity.rs`): byte-exact against
`to_vec(&load_sessions_wire())` over NULL/empty/unicode/quote/backslash/
control-character columns, negative and fractional costs, i64 extremes,
`model` stored with non-compact whitespace, `model` that is not JSON (the
fallback), and a post-write round trip. Corpus run on the fixture:
**201 sessions, 105,475 bytes, byte-exact.** A planted extra member turns 3
of the 5 tests red, so the gate is not vacuous.

### 1.2 SQLite tuning audit — every pragma decided by evidence (PERF-10X stmt pass, 2026-10-07)

Method: live probe of our bundled build (rusqlite 0.40.2 / SQLite 3.53.2,
`examples/sqlite_info.rs`), official pragma docs fetched, decisive A/Bs on a
scratch copy of the `.227` fixture db (`examples/sqlite_tune_bench.rs`,
memos off, interleaved where noisy). Warm µs figures from `prep_A/B` and
`m0–m2` runs in this session.

| setting | value | decision + evidence |
|---|---|---|
| journal_mode / synchronous | WAL / NORMAL | keep — docs-standard for WAL durability |
| page_size | 4096 | **REJECT 8192**: A/B on a VACUUM-rebuilt copy (4.1 s rebuild) = flat on all 7 hot queries (page_window 7→8 µs even); write amplification up |
| mmap_size | 0 | **REJECT 256 MB**: A/B zero delta everywhere (page 7 µs, fts 110 µs, list 805 µs both ways); RSS/OOM history rationale intact (docs note no special caveats, but nothing to win) |
| temp_store | 1 (FILE) | fixed earlier (value was 2=MEMORY miscommented); `/tmp` is tmpfs here → sort spill = shmem, memcg-accounted, swappable — bounded either way |
| analysis_limit | 0 (default) | **no change** — since 3.46 `PRAGMA optimize` sets its own temporary limit (0x00010 bit, on by default); docs: "applications that use optimize … do not need to set an analysis limit" |
| optimize placement | **FIXED** | was only in `create_new` (runs before tables exist = perpetual no-op) + hourly tick needs 240 *write* batches → fixture dbs had `stat1=0` forever (`optimize(-1)` listed 7 pending ANALYZEs). `schema::migrate` is now a wrapper that runs `post_maintenance` on EVERY writer spawn incl. steady-state boots: `optimize=0x10002` (docs' verbatim long-lived-connection value; measured **6 ms live**, 0.09 s fixture) — with per-phase timing logs added after a 2026-10-07 deploy spent 5m40s silent with nothing to attribute it to. Test `steady_state_migrate_collects_planner_stats` + planted negative control (red→green) |
| FTS `'optimize'` maintenance | **REMOVED from the boot path (2026-10-08)** — kept only as documented offline SQL | fixture had said ADOPTED (−32% search, −15% LIKE-shaped walk, "sub-second at steady state") but the LIVE 1.6 GB db measured the same statement at **175,796 ms** with serve blocked (boot 3 min; an earlier untimed deploy showed 5m40s silence; second writer spawn 3 s later = 0 ms ⇒ the merge itself is the cost). FTS5 default automerge maintains segments during inserts and S-A keeps search ms-class, so the fixture win does not pay for a 3-minute boot. Offline re-run: `sqlite3 ocserve.db "INSERT INTO part_search_fts(part_search_fts) VALUES('optimize');"`. Re-add to boot only on measured search regression |
| threads | 0 | keep — auxiliary sorter threads only help the big sorts S-A removed; per-statement thread launch would be overhead |
| secure_delete | 0 | verified OFF in our build (compile_options lacks `SECURE_DELETE`; probe) — no rewrite amplification on prune/cascade |
| STAT4 | compiled in | no action — `ENABLE_STAT4` present; optimize writes stat4 when it analyzes |
| cell_size_check | 1 | keep (M0 integrity choice; docs confirm only "small hit") |
| cache_size | writer −16 MB / reader −4 MB | keep; docs: allocation is on-demand chunks, so 4 MB×16 parked readers is a ceiling not a usage; live cgroup stayed 282–450 MB |
| busy_timeout / wal_autocheckpoint / journal_size_limit | 5000 / 1000 / 64 MB | keep (docs defaults + WAL-reset history) |
| case_sensitive_like / automatic_index / cache_spill / locking_mode | untouched | case_sensitive_like is deprecated (docs) and LIKE contract tests pin default semantics; the rest are docs-recommended defaults |

**Prepared statements (cutting edge, rusqlite 0.40.2):** `prepare_cached`
is the modern path — it prepares with `SQLITE_PREPARE_PERSISTENT` and LRU-
reuses the VDBE (docs: "returns a cached statement … else prepares with
SQLITE_PREPARE_PERSISTENT"; cache key `sql.trim()`, default capacity 16 →
set explicitly to **32** in `apply_common`). Converted: 17 read-path sites
(list/page/for_each/session_wire/search/todos/children/compaction_rows/
last_message) + the writer's `bind()` choke (every WriteOp::Sql).
Skipped with reasons: 3 backfill one-shots (never reused) and the dynamic
`IN (…)` batch SQL in `pull_legacy_delta` (text varies per batch size —
LRU churn, sync cadence doesn't care). `query_row` sites unchanged (no
cached API; measured 2 µs). Measured A/B (interleaved ×3, t2): session_wire
**8→1 µs**, for_each_page **18→13 µs**, page_window 7→6 µs, list/search
neutral.

### 1.1 Search projection as shipped (W1 → v10, PERF-10X S-A)

Supersedes the pre-W1 sketch above (`search_doc` was dropped as dead in W1 — never
populated live). Current design: `part_search(rowid, part_id, session_id, message_id, text)`
+ external-content `part_search_fts` (`tokenize='trigram'`, `content='part_search'`),
synced by the ai/ad/au triggers; single writer-side choke point
(`part_search_upsert_ops`).

**Rowid contract (v10):** `rowid = msg.time_created * 1_048_576 + slot` where `slot` =
max slot already taken in that millisecond + 1 (`PART_SEARCH_SLOTS = 1<<20`;
max observed parts/ms = 22; overflow ⇒ `% slots` wraps into an occupied slot ⇒ loud PK
conflict). Consequences, all load-bearing:

- `ORDER BY rowid DESC` **is** the search contract order (time DESC, insert order within
  the ms) — the search query needs no sort and the fts walk early-terminates at LIMIT
  (E1: 5.9 s → 0.2 ms warm on the fixture).
- `message_id` carries `REFERENCES msg(id)` so a missing msg can never NULL the rowid
  (NULL ⇒ silent auto-assign ⇒ broken order); the `part_search_rt` trigger aborts any
  insert whose rowid encodes a different ms than its message's `time_created`.
- `(session_id)` index exists for the scoped search walk (reverse index scan =
  rowid DESC for free; per-row `EXISTS` probe bounds cost by session size).
- Migrations touching rowids are full table rebuilds (in-place reorder = PK collisions);
  the v9→v10 rebuild's fts `rebuild` re-tokenizes the whole corpus — **measured 551 s
  one-time on the 1.5 GB fixture (378 MB text; stepwise timings logged via
  `tracing::info`)**; fresh dbs (0→current) and fixture imports take the fast path and
  never pay it. `detail=column/none` cannot shrink this: E3 proved phrase queries error
  (or silently return 0 rows) without `detail=full`, and our queries are always phrases
  (quoted) under trigram.

## 2. Page/pragmas — exact set

**First connection, before any table exists (creation profile):**
```sql
PRAGMA page_size            = 4096;      -- x86 NVMe 4K sector; ARCH note below; WAL makes this immutable
PRAGMA auto_vacuum          = INCREMENTAL; -- MUST precede CREATE TABLE
PRAGMA journal_mode         = WAL;        -- persistent; set after page_size
PRAGMA synchronous          = NORMAL;     -- WAL-safe: only power-loss of last commit is at risk (accepted, documented)
PRAGMA wal_autocheckpoint   = 1000;       -- default; 4K pages ≈ 4 MB WAL segments
PRAGMA journal_size_limit   = 67108864;   -- WAL file capped 64 MB after checkpoint
PRAGMA cell_size_check      = ON;         -- early corruption detection
PRAGMA trusted_schema       = OFF;        -- hardening
PRAGMA foreign_keys         = ON;
PRAGMA optimize             = 0x10002;    -- long-lived-server recipe (3.46+)
```

**Per connection — role-scoped cache (aggregate ≤32 MB, part of the 300 MB RSS budget):**
```sql
-- writer:      PRAGMA cache_size = -16384;   -- 16 MB
-- each reader: PRAGMA cache_size = -4096;    -- 4 MB × pool(4) = 16 MB
-- all:         PRAGMA busy_timeout = 5000;
-- readers:     URI mode=ro  +  PRAGMA query_only = ON;
-- all:         PRAGMA mmap_size = 0;         -- CIDR 2022: mmap is not a buffer pool; keeps RSS honest
-- all:         PRAGMA temp_store = FILE;     -- bounded (MEMORY has no ceiling → OOM risk); SQLITE_TMPDIR on data dir
PRAGMA threads = 0;                           -- server owns threading
```
Per-repo precedent: reliary8/stria use `mmap_size=256MB` + `cache_size=-200000` for *bulk
index builds*. ocserve is a long-running server under a hard RSS cap → smaller caches, mmap off.
The bulk profile is used **only by the importer**, in a throwaway process (dual-profile pattern
from `reliary8/.../schema.rs:45-52`).

**Rules learned from your repos (adopt verbatim):**
- Set *configuration* pragmas **once at connection open**; never re-issue `journal_mode` on a
  live handle (reliary8 `mcp.rs` deadlock trap). *Operational* pragmas are exempt: the CLI
  sampler queues `PRAGMA incremental_vacuum(4096)` every 15s and `PRAGMA optimize` hourly
  onto the WRITER connection (drains freelist a bounded amount; no-op at freelist=0; never a
  full VACUUM online) — W3 amendment: the "never re-issue" ban targets configuration at open,
  not state-mutating maintenance (2026-10).
- `BEGIN IMMEDIATE` for every write; RAII guard that rolls back on early `?` return
  (`ingest.rs:493-508`).
- `prepare_cached` for every stable statement.
- `user_version` schema gate with actionable error (`schema.rs` pattern in both repos).
- Never `let _ = db.execute_batch(...)` — PRAGMA failures log a warning, never swallow
  (anti-pattern flagged in reliary8's own audit docs).

## 3. Write path and WAL discipline

- **One writer task** per process (SQLite single-writer); all app writes batched into
  transactions ≤50 ms via an mpsc channel.
- **Hard rule: no connection is held across `.await`** (a pinned read mark blocks
  `wal_checkpoint(TRUNCATE)` → the 888 MB WAL pathology). Enforced by type: pool checkouts are
  scoped guards; CI lints for `Send` futures holding `PooledConnection`.
- Checkpointing: writer runs `PRAGMA wal_checkpoint(PASSIVE)` opportunistically;
  `TRUNCATE` only from an idle detector (no active readers) — never from the request path.
- Alarm metric: `wal_bytes` > 128 MB → warn; sustained > 128 MB → `doctor` failure.
- Maintenance jobs (all bounded slices, all in idle windows):
  ```
  hourly:    PRAGMA optimize;
  when freelist_count > 10000 pages:  PRAGMA incremental_vacuum(10000);
  nightly:   PRAGMA quick_check;   (weekly: integrity_check — includes FTS5 since 3.44)
  idle:      FTS 'merge' compaction (see §1)
  nightly:   backup (see §5)
  ```

## 4. Schema principles

- Indexed tables = **fixed-size metadata only** (ids, seq, timestamps, lengths, sha256, enums).
- Payloads (parts, tool output, diffs, images) → **external chunked blob store**: content-
  addressed (`sha256/16` paths), ≤1 MB chunks, zstd level 3, each chunk an RFC 8878 frame set.
  Sizing: RFC 8878 blocks ≤128 KB inside 1 MB files; official SQLite guidance puts the
  in-DB/blob-file crossover at ~100 KB — everything above it is out-of-DB by design.
- `STRICT` tables everywhere (3.37+); `WITHOUT ROWID` for hot composite-key lookup tables
  (both repos do this for occurrence/join tables).
- JSONB columns (3.45+) only for small queried metadata with generated-column indexes —
  payload JSON never lives in SQLite (it lives in blobs).
- Event log: bounded ring (30 d / 200 k per session, config), with `event_ring_lag` metric.
- Transactional blob protocol: write chunk → fsync → rename → then commit DB row in the same
  writer batch; orphan blobs (crash between) are swept by boot-time GC; missing referenced
  blobs fail `doctor`. Test: SIGKILL fuzz loop in CI.

## 5. Backup / integrity / import

- Backup: `VACUUM INTO` (main DB; zero-lock, fsync'd) + blob store copied by generation
  manifest recorded inside the backup DB. Restore drill in CI: restore elsewhere →
  `doctor` (hashes, counts, `integrity_check`) must pass.
- Import (20 sessions): open source `mode=ro` only; **chunked read transactions** released
  between batches (a single long read txn would pin upstream's 888 MB WAL and stall the live
  server); abort guard if source WAL grows >500 MB during import. 122 MB row read via
  `substr(data, offset, len)` 4 MB windows straight into the zstd encoder — never
  materialized, never `serde_json::from_slice`'d whole (stream-deserializers do NOT stream a
  single 122 MB value). Verification by incremental canonical-JSON **hash**, not Value==Value.

## 6. Architecture notes

- `page_size` is immutable under WAL → decided per-machine at creation: x86 NVMe = 4096;
  if an arm64 host with 16K pages ever runs ocserve, creation profile uses 16384 (documented
  in `doctor --show-pragmas`).
- No `target-cpu` effects on SQLite; compile flags are the bundled defaults + our needs
  (FTS5, JSON1, STAT4 all default-on in bundled).
- SIMD: SQLite's built-in; we do not hand-roll. zstd and sha2 dispatch to CPU features
  internally (cpuid) — verified by `ocserve bench --blob` before/after, no code required.

## 7. CI gates (storage)

1. `EXPLAIN QUERY PLAN` audit: any `SCAN` on a table projected to exceed 10 k rows = fail.
2. Full-scan detector runtime: any query > 1 s recorded → fail (metric `query_ms{max}`).
3. p95 session-list < 50 ms (≥1000 samples; fail > 60 ms to absorb runner noise).
4. 10-minute accelerated soak: RSS < 300 MB, `wal_bytes` < 128 MB, freelist ratio stable.
5. SIGKILL fuzz (blob/DB protocol) ×1000 iterations: boot clean, no dangling refs.
6. Backup/restore drill green.
7. Import fixture (incl. 122 MB row): peak RSS < 300 MB, canonical hash equality.

## Schema versions (continued)
- **v6**: `import_sync(session_id TEXT PRIMARY KEY, cursor TEXT, last_sync_ms INT)` — legacy-delta
  sync state (dev bridge; plan §17). Migrations run as a step loop (v1-v2 chains no longer
  skip intermediate DDL — fixed with v6).

- **v7**: `part_search` + `part_search_fts` (FTS5 external-content, `tokenize='trigram'`)
  with `part_search_ai/ad/au` triggers; drops dead `search_doc`/`search_fts`. `text` stores
  the uncompressed part JSON (blobbed parts searchable). One-time boot backfill
  (`backfill_part_search`, idempotent via NOT EXISTS + upsert). **SQL strings are never
  split across lines** — literal-backslash-in-SQL is check-guards rule 2 (it shipped once in
  this very feature; see TESTING §1.6).
