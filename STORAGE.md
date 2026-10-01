# refine — SQLite storage spec ("tuned within an inch of its life")

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
index builds*. refine is a long-running server under a hard RSS cap → smaller caches, mmap off.
The bulk profile is used **only by the importer**, in a throwaway process (dual-profile pattern
from `reliary8/.../schema.rs:45-52`).

**Rules learned from your repos (adopt verbatim):**
- Set pragmas **once at connection open**; never re-issue `journal_mode` on a live handle
  (reliary8 `mcp.rs` deadlock trap).
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
  if an arm64 host with 16K pages ever runs refine, creation profile uses 16384 (documented
  in `doctor --show-pragmas`).
- No `target-cpu` effects on SQLite; compile flags are the bundled defaults + our needs
  (FTS5, JSON1, STAT4 all default-on in bundled).
- SIMD: SQLite's built-in; we do not hand-roll. zstd and sha2 dispatch to CPU features
  internally (cpuid) — verified by `refine bench --blob` before/after, no code required.

## 7. CI gates (storage)

1. `EXPLAIN QUERY PLAN` audit: any `SCAN` on a table projected to exceed 10 k rows = fail.
2. Full-scan detector runtime: any query > 1 s recorded → fail (metric `query_ms{max}`).
3. p95 session-list < 50 ms (≥1000 samples; fail > 60 ms to absorb runner noise).
4. 10-minute accelerated soak: RSS < 300 MB, `wal_bytes` < 128 MB, freelist ratio stable.
5. SIGKILL fuzz (blob/DB protocol) ×1000 iterations: boot clean, no dangling refs.
6. Backup/restore drill green.
7. Import fixture (incl. 122 MB row): peak RSS < 300 MB, canonical hash equality.
