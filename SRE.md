# refine — SRE / DevOps spec (fail-fast, metrics, tunability, CPU)

Companion to `PLAN.md`. Mindset: every failure is loud and actionable at boot; every KPI in
`PLAN.md §10` is a queryable metric; every knob is declared, typed, and validated; CPU work
is measured before it is specialized.

## 1. Fail-fast

**Boot self-check (`refine serve` refuses to start unless all pass):**
- `sqlite_version() ≥ 3.51.3` (WAL-reset fix) — bundled rusqlite ≥0.40.2, never system lib
- FTS5 create + insert + match roundtrip on temp file (catches broken builds — we hit exactly
  such a build during planning: `content=''` constructor fails on box's 3.53.0)
- Data dir + WAL dir writable; free disk > 2× WAL limit; `auto_vacuum=INCREMENTAL` present
  before first table (asserted via `PRAGMA auto_vacuum` on the created DB)
- `refine.toml`: `deny_unknown_fields` — unknown key = **fatal** (reliary8 warns and proceeds;
  we don't copy that)
- Plugin bundle manifest hash matches install-time record; MCP server configs parse
- Bind probe on the port (fail before systemd restart-loop)

**Runtime fail-fast:**
- `panic=abort` + `clippy::unwrap_used` denied in server crates: a bug crashes, not corrupts
- Panic hook: structured log with task context + last HTTP route + session id, then abort
  (systemd `Restart=on-failure` recovers; `RestartSec=5`, `StartLimitBurst` respected)
- Readiness vs liveness split: `/global/health` returns 503 with reason when the store
  degrades (quick_check fail, disk full) — systemd/`health` probe sees it
- Writer queue depth > hard cap → prompt admission returns 503 (backpressure), never unbounded
  memory growth
- Blob/DB inconsistency at boot → refuse to serve with `doctor` command printed (repair first)

## 2. Metrics

Exposed on an internal `127.0.0.1` `/metrics` (Prometheus text format, `metrics` + `metrics-exporter-prometheus`),
behind the same auth as the rest:

| Metric | Type | Feeds KPI |
|---|---|---|
| `http_request_duration_seconds{route,method}` | histogram **+ separate `*_max` gauge** | p95 list <50 ms; *max* <1 s (KPI is max, not p99) |
| `query_duration_seconds{query}` | histogram + max gauge | "no query >1 s" |
| `sse_clients`, `sse_events_total` | gauge/counter | soak, fanout sanity |
| `event_ring_lag{session}` | gauge | ring overflow alarm |
| `wal_bytes` | gauge | WAL alarm >128 MB |
| `checkpoint_duration_seconds{result}` | histogram | stall <250 ms |
| `writer_queue_depth` | gauge | backpressure trigger |
| `plugin_hook_duration_seconds{hook,result}` | histogram | 30 s deadline monitoring |
| `rss_bytes`, `rss_peak_bytes`, `mcp_*` | gauge | 300 MB budget |
| `blob_orphans`, `blob_missing_total` | gauge/counter | storage integrity |
| `llm_request_duration_seconds{provider,model}`, `llm_ttft_seconds`, `llm_stream_errors_total` | histogram/counter | provider health (S6) |
| `events_emitted_total{type}` | counter | contract drift evidence |

Structured logging: `tracing` + `tracing-subscriber` JSON to stderr, `RUST_LOG` filter,
level per-module (both repos lacked a subscriber init — we ship it).

## 3. Tunability

Single source: `refine.toml` (validated at boot, `deny_unknown_fields`) with env overrides
`REFINE_*` for the systemd unit. Declared knobs — nothing is a magic constant in code:

```toml
[server]   port, hostname, auth_mode ("off"|"basic"), auth_password_file
[store]    writer_cache_kb=16384, reader_cache_kb=4096, reader_pool=4,
           wal_autocheckpoint_pages=1000, journal_size_limit=67108864,
           checkpoint_idle_ms=5000, freelist_vacuum_threshold=10000
[events]   ring_max_per_session=200000, ring_retention_days=30, sse_ring_events=4096
[plugins]  enabled=true, hook_deadline_ms=30000, isolate_heap_mb=8, sidecar_rss_limit_mb=80
[mcp]      request_timeout_s=30, oauth_refresh_skew_s=120
[runtime]  worker_threads=8, max_blocking_threads=8, thread_stack_kb=1024,
           rayon_threads=8            # min(8, parallelism) default; env overridable
[blob]     chunk_bytes=1048576, zstd_level=3, gc_orphan_grace_hours=72
[metrics]  enabled=true, bind="127.0.0.1:9099"
```
Config reload: SIGHUP re-reads safe subset (metrics, log level, ring limits); structural keys
(store, port) require restart — declared, not guessed.

## 4. CPU: measure first, specialize second

**Justified per-arch work:**
- zstd and sha256 (`sha2` with `cpufeatures`) dispatch to AVX2/SHA-NI at **runtime** — zero
  code, verify with `refine bench --blob` before/after (expect nothing if already active).
- `available_parallelism()` for defaults (not hard-coded 22) with env override — the reliary8
  lesson (WSL2/ARM).
- `page_size` decided per-machine at DB creation (4096 x86 NVMe; 16384 arm64) — immutable
  under WAL, so it's a creation-time decision, not a tunable.
- Build: portable baseline by default (`x86-64-v2`); a `native` profile
  (`-C target-cpu=native`) exists for laptop-only builds; CI/PR artifacts stay portable.
  **Banned without a profile**: hand-written SIMD, `target-cpu=native` as default, huge
  pages (moot with `mmap_size=0`).

**Hot paths to profile in M2/M3 (criterion benches, before deciding anything):** SSE frame
encode, JSON part encode, blob zstd, FTS insert, tool-output chunking. Optimization lands
only with a before/after number on the bench gate.

## 5. DevOps

- **systemd unit** (`~/.config/systemd/user/refine.service`): `MemoryMax=300M`,
  `MemorySwapMax=0` (fail-fast over swap), `Restart=on-failure`, `RestartSec=5`,
  `Environment=MIMALLOC_PURGE_DELAY=500 RUST_LOG=refine=info`, hardening
  (`ProtectSystem=strict`, `ReadWritePaths` on data dir, `NoNewPrivileges`).
- **Release profile** (matches reliary8/stria): `lto="fat"`, `codegen-units=1`,
  `opt-level=3`, `panic="abort"`, `strip=true`; **binary size ceiling 10,485,760 B
  (10.0 MiB)** — established from the measured 2026-10-03 release build (9,279,528 B);
  raise only with a measured reason recorded here. Enforced by `scripts/nightly.sh`.
- **Per-commit gates (AGENTS §3, run by hand before every commit):** clippy `-D warnings`,
  fmt, full workspace tests, `scripts/check-guards.sh`, `scripts/check-matrix.sh`
  (+ area gate: storage `STORAGE.md §7`, memory `MEMORY.md §5`, wire differential replay).
- **Pre-commit hook (`.git/hooks/pre-commit`, installed 2026-10-03):** banned-string
  check (login name / absolute home paths) + docs-required (staged `crates/*.rs` must
  stage `TRACEABILITY.md`). Fast checks only — no network, no builds.
- **Nightly (wired: systemd user timers, 2026-10-03 — repo is local, so these are the
  "CI" until a remote exists):**
  - `refine-nightly.timer` 06:30 → `scripts/nightly.sh`: `cargo audit`, `cargo deny check`
    (`deny.toml`), SIGKILL blob/DB crash fuzz, provider stream fuzz (10 k seeded
    chunk-boundary splits vs whole-buffer parse), backup drill (live `VACUUM INTO` +
    integrity + row counts), binary size ceiling; `--with-mutants` = manual/weekly
    (never during benchmark sessions).
  - `refine-drift-watch.timer` 06:00 → `scripts/drift-watch.sh` (upstream release
    triage — adopt/ignore, never auto).
  - `refine-soak.timer` daily → fresh 24 h soak CSV at
    `~/.local/state/refine/soak/soak.csv`; `scripts/soak-gate.sh <csv>` machine-checks
    slope/health (replaces eyeballing). Restart manually after every deploy.
  All timers `Persistent=true` (missed runs catch up on boot).
- **Runbook:** `refine doctor` = boot checks + storage health + FTS integrity + blob GC dry-run
  + version/contract info; `refine import`, `refine bench {--http,--blob,--replay}`.

## 6. Patterns adopted from your repos (and anti-patterns rejected)

Adopted: dual PRAGMA profiles (serve vs import), pragmas once per connection, warm-conn +
RAII guard, `prepare_cached`, `BEGIN IMMEDIATE` + rollback-on-drop guard, drop secondary
indexes for bulk import, compute-in-Rust-then-one-SQLite-pass, `user_version` gate,
`WITHOUT ROWID`, conditional ANALYZE, `wal_checkpoint(TRUNCATE)` + tmp-build + atomic rename
(import finalize), `index_gen`-style freshness stamps on responses/caches, mimalloc, rayon
cap + env override, binary-size CI gate, env-tunable prefixes, trailing-edge debounce if a
watcher ever appears.

Rejected (documented anti-patterns): swallowed `execute_batch` errors, warn-only config
validation, `lock_timeout` pragma (no-op on stock builds — use `busy_timeout`), FTS5 trigram
copied blindly (reliary8 removed theirs after measuring), advisory-only perf benches.

Not in your repos (new here, per this spec): `/metrics`, tracing-subscriber JSON, readiness
split, graceful shutdown (drain SSE → close writer → `optimize` → exit), panic hook, soak
gates, criterion harness, systemd hardening.
