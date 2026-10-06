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
| `refine_config_reload_total{result}` | counter | hot-reload health: `ok`/`mcp` per applied reload; sustained `error` = broken config file (fail-safe keeps old state serving) |
| `refine_prompt_rss_start_bytes` / `refine_prompt_rss_delta_bytes` | gauge | OOM phase attribution: RSS at prompt start and peak−delta during the run (Drop-emitted — every exit path incl. bails; last-prompt semantics; paired with the 15s serve sampler for continuous history) |
| `refine_db_opens_total` | counter | reader-open cost per prompt (delta over the run; KPI ≤3/prompt; approximate under parallel sessions) |
| `refine_sync_tick` | duration | legacy-sync tick cost (the idle-jump suspect until measured — 11 msgs total says likely tiny) |
| `refine_db_bytes` | gauge | DB file growth (MEMORY §7.6 KPI; policy trigger at ≥5 GB / ≥1k sessions) |
| `refine_history_truncated_total{dropped}` | counter | prompt-history budget trips (8 MB, tail-weighted) — silent context loss made measurable |
| soak CSV `oc_rss` column | sample | native opencode RSS = the future-usage ceiling model (MEMORY §7.6) |
| `refine_prompt_rounds_total{bucket,finish}` | counter | uncensored rounds-per-turn distribution: `bucket` = 0-9/10-19/20-39/40-79/80-159/160+, `finish` = `done`/`error`/`capped` — the K-AUTONOMY telemetry the old flat cap made unmeasurable (right-censored at 25) |
| `refine_plugin_normalize_total{result}` | counter | plugin normalizer health (D1): sustained `error` = builds failing → raw fallback; `warm` on every load after first boot; `disabled` only when kill-switched |
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
(store, port) require restart — declared, not guessed. External config-file edits
(opencode.json / refine overlay / auth.json / auth-overlay / models cache / state model)
hot-reload via poller: `REFINE_CONFIG_WATCH=0` disables, `REFINE_CONFIG_POLL_MS`
(default 2000, min 100 — clamped so 0 can never busy-loop); fail-safe at runtime
(broken file keeps old state, retries; boot stays fail-fast).

Runaway/autonomy knobs — read **once at process start** (restart to change, like
`REFINE_PROVIDER_STALL_SECS`): `REFINE_PROMPT_MAX_ROUNDS` (default 0 = unlimited;
hard cap only — failure is surfaced, never silent) and `REFINE_PROMPT_MAX_COST_USD`
(default 0 = off; hard USD ceiling per prompt). Declared here, not guessed: both
have kill-criteria tests (`autonomy.rs`) and the bound lives on dollars/visibility,
not on a magic round count.

Plugin normalization knob — `REFINE_PLUGIN_NORMALIZE` (default **on**): content-hash-cached
rolldown normalization of plugin entries at load (D1 — decision record
`bench/normalizer-spike/D1-PLAN.md`). `=0` skips **before rolldown is constructed** (the
panic=abort residual gate); loads fall back to the raw entry — never worse than today's
behavior. Telemetry: `refine_plugin_normalize_total{result}` (`warm` hash hit / `built` +
A2 log line with destination, ms, RSS before→after / `disabled` kill-switch / `error`
WARN + raw fallback / `passthrough` double-normalize guard).

Provider knob — `REFINE_ZEN_KEYLESS` (default **on**): keyless opencode zen
endpoint (`Bearer public` + the proven discriminator wire — composite UA,
native session id, ≥2 known tool names; evidence ledger
`bench/zen-probe/FINDINGS.md` P5/P7/P8 + B1–B7). `REFINE_ZEN_KEYLESS=0`
restores the pre-port behavior (no keyless endpoint → deepseek fallback
default). Gate flips increment `refine_zen_freetier_total` and are checked
nightly by the live trio (`crates/refine-llm/tests/zen_live.rs`: positive /
text-only / negative — the negative proves the wall still exists).

Model catalog (K-MODELS) — same-as-upstream refresh, no operator action
needed: honors `OPENCODE_MODELS_URL` / `OPENCODE_MODELS_PATH` /
`OPENCODE_DISABLE_MODELS_FETCH` (upstream truthy: `"1"`/`"true"`); source
default `https://models.opencode.ai/api.json`; fresh TTL 5 min; the serve
loop refreshes immediately-if-stale then every 60 min; `refine models
refresh` forces (upstream `opencode models refresh` parity). Shared cache
`~/.cache/opencode/models.json` (their opencode writes the same file —
coordination via the same lease under `~/.local/state/opencode/locks/`,
heartbeat 20 s / stale 60 s); UA mirrors
`opencode/{channel||latest}/{version||1.18.31}/{client||cli}` (env
overridable). Corrupt catalog self-heals (remove + refetch) — it never
fails boot. Visibility: `refine_models_refresh_total{result}` + nightly
`live_fetch_contains_big_pickle`.

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

- **The service is an OPTIONAL overlay, preferred, never implicit.**
  Foreground `refine serve` is the contract (tests, replay-check, the parity
  harness all run it directly and gain zero requirements). Nothing in build,
  tests, or `refine doctor` ever installs or enables anything; the ONLY code
  path that touches systemd is an explicit `scripts/install.sh` run, and
  `doctor` reports the overlay as one informational line either way.
- **Resource-control stack (2026-10-06, "middle ground": neither unbounded
  nor a knife-edge cap).** Every layer degrades to the one below on failure;
  the cap is the BACKSTOP, not the control:
  1. **L0 bounded-by-construction (always on):** 32 MB event ring, 8 MB
     tail-weighted history budget, streamed `/message`, `malloc_trim`
     cadence, `MALLOC_ARENA_MAX=1`, single catalog parse per reload.
  2. **L1 graceful sidecar recycle** (15 s sampler): RSS ≥
     `REFINE_SIDECAR_RECYCLE_MB` (default 450 — above the measured 365 MB
     warm-up peak; `0` = kill switch) × 2 consecutive samples ∧ zero
     in-flight plugin RPCs (try_lock busy ⇒ skip) ∧ uptime ≥ 300 s → kill
     the sidecar (state is disk-backed; `ensure_alive` respawns + replays
     next trigger, warm-hash normalize no-op). Never interrupts a hook,
     never storms; precise RSS via the sidecar's own `child_pid` (the old
     "first node/bun child" scan could hit the browser host after any
     respawn). Metrics: `refine_sidecar_recycle_total{reason="rss"}`.
  3. **L2 cgroup partition** (`partition.rs`): `mkdir main kids → move self
     into main/ → +memory → kids/memory.max=700M` (kernel no-internal-
     process rule); the A3 wrapper moves every child into `kids/` via
     per-Command `REFINE_KIDS_CGROUP` env. Children get a chosen ceiling
     (covers the measured 614 MB embedding burst); main's reserve under
     `MemoryMax` becomes structural instead of an `oom_score_adj` lottery.
     **Probe-first:** root-exists / owned-by-us / `memory` in controllers /
     subtree-writable probed before any mutation; any failure → metric
     `refine_cgroup_partition{result="unavailable"}` + today's flat shared
     cap. **Scope gate:** runs only under systemd (`INVOCATION_ID`) or
     forced `REFINE_CGROUP_PARTITION=1`; `=0` vetoes — bare/test/harness
     runs never restructure a terminal's or cargo's cgroup tree.
  4. **Kill policy:** children `oom_score_adj=500` + `OOMPolicy=continue`
     (**2026-10-06 incident**: kernel OOM correctly killed only the sidecar
     child (A3 working) but default `OOMPolicy=stop` then bounced the WHOLE
     unit — `Failed with result 'oom-kill'` → restart, journal 11:00:16.
     Proven with synthetic control units under `MemoryMax=64M`: default →
     `failed/oom-kill`; `continue` → `active`, MainPID alive after the child
     died. `continue` = log + survive, so `ensure_alive` respawns the
     sidecar; a MAIN-process OOM still restarts via normal exit handling.
     guard rule 9).
  5. **Backstop:** `MemoryMax=1024M` — kids 700 + main reserve ~324 (≈ 2×
     the measured 180 MB envelope); provenance comment chain in the unit
     template (history: 480 → 600 → 750 → 1024, each step measured). A
     larger ceiling costs **nothing at idle** (cgroups charge on touch) —
     leak detection is the recycle gauge + soak slope, never cap proximity.
     Also: `Delegate=yes` (systemd stops managing the unit subtree so the
     dance can run), `MemorySwapMax=0` (fail-fast over swap — the reason
     no `MemoryHigh` soft throttle ships yet: without swap it stalls
     instead of shrinking; PSI gauge `refine_mem_pressure_avg10` is the
     measure-first gate for revisiting), `Restart=on-failure`,
     `RestartSec=5`, `RUST_LOG=refine=info` (MIMALLOC_* removed 2026-10-04
     — dead without a linked allocator), hardening (`ProtectSystem=strict`,
     `-path` optional ReadWritePaths for the opencode/plugin dirs so fresh
     machines boot, `NoNewPrivileges`).
- **Install (preferred, opt-in by invocation):** `scripts/install.sh` —
  default = full (build-if-needed, binary used IN PLACE, render
  `deploy/refine.service` (single source of truth), `daemon-reload`,
  enable/restart, health-probe, print overrides + kill switches + uninstall
  hint); `--dry-run` (diff, touch nothing), `--bin-only`; non-systemd host
  auto-falls to bin-only. Writes `install-receipt` (BIN=) so uninstall can
  attribute the binary. Never sudo, never non-refine units (rule 6).
- **Uninstall (application-clean; history sacred):** `scripts/uninstall.sh`
  — default removes refine\* units/timers/drop-ins (glob), the
  receipt/ExecStart binary (dev `target/` builds KEPT with a note), and
  refine-derived `*.normalized.mjs{,.hash}` artifacts (basename-gated);
  **session history KEPT** with sizes + purge hint. `--purge` = stats →
  confirm (`--yes` for scripts) → optional `VACUUM INTO` backup
  (`--no-backup` to skip) → data/state removal (path from unit
  `Environment=`, refused unless under `$HOME` AND containing `refine`;
  custom non-conforming dirs are reported, never deleted). Shared opencode
  state (config, `auth.json`, `opencode.db`, `models.json`, plugin
  packages) is NEVER touched — guard rule 11 + staged `--selftest` canary
  battery (refine artifacts gone, canaries byte-identical, receipt binary
  removed, dev build kept, idempotent, history honored; nightly-wired).
  Journal vacuum deliberately NOT done: the journal is shared with
  opencode and unit logs age via the retention floor.
- **Release profile** (matches reliary8/stria): `lto="fat"`, `codegen-units=1`,
  `opt-level=3`, `panic="abort"`, `strip=true`; **binary size ceiling 20,971,520 B (20 MiB)**
  — re-baselined 2026-10-06 from the measured D1 build (20,359,600 B with
  rolldown linked; original: 10,485,760 B from 9,279,528 B measured 2026-10-03).
  Raise ONLY with a new measurement + provenance comment in `scripts/nightly.sh`
  (the tripwire re-sets from evidence, never moves to fit). Enforced there.
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
