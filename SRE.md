# ocserve — SRE / DevOps spec (fail-fast, metrics, tunability, CPU)

Companion to `PLAN.md`. Mindset: every failure is loud and actionable at boot; every KPI in
`PLAN.md §10` is a queryable metric; every knob is declared, typed, and validated; CPU work
is measured before it is specialized.

## Minimum CPU ISA

Release binaries are built with the **baseline x86-64 ISA** (no `.cargo/config.toml`).
An earlier plan (PERF-10X C1) added `-Ctarget-cpu=x86-64-v3` / `-march=x86-64-v3`, but the
A/B sandwich failed the keep-rule (post-flag throughput tied the pre-flag binary to 0.1%, so
the flag bought nothing measurable) and the config was **removed** (`PERF-10X.md` C1+C4).
No per-architecture flags are set today, so the release binary runs on any x86-64 host.
`RUSTFLAGS`/`CFLAGS` overrides remain available for a local, measured experiment; a change
is adopted only if it clears the A/B keep-rule (`PERF-10X.md` Phase IV).

## 1. Fail-fast

**Boot self-check (`ocserve serve` refuses to start unless all pass):**
- `sqlite_version() ≥ 3.51.3` (WAL-reset fix) — bundled rusqlite ≥0.40.2, never system lib
- FTS5 create + insert + match roundtrip on temp file (catches broken builds — we hit exactly
  such a build during planning: `content=''` constructor fails on box's 3.53.0)
- Data dir + WAL dir writable; free disk > 2× WAL limit; `auto_vacuum=INCREMENTAL` present
  before first table (asserted via `PRAGMA auto_vacuum` on the created DB)
- `ocserve.toml`: `deny_unknown_fields` — unknown key = **fatal** (reliary8 warns and proceeds;
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
| `ocserve_config_reload_total{result}` | counter | hot-reload health: `ok`/`mcp` per applied reload; sustained `error` = broken config file (fail-safe keeps old state serving) |
| `ocserve_prompt_rss_start_bytes` / `ocserve_prompt_rss_delta_bytes` | gauge | OOM phase attribution: RSS at prompt start and peak−delta during the run (Drop-emitted — every exit path incl. bails; last-prompt semantics; paired with the 15s serve sampler for continuous history) |
| `ocserve_db_opens_total` | counter | reader-open cost per prompt (delta over the run; KPI ≤3/prompt; approximate under parallel sessions) |
| `ocserve_sync_tick` | duration | legacy-sync tick cost (the idle-jump suspect until measured — 11 msgs total says likely tiny) |
| `ocserve_db_bytes` | gauge | DB file growth (MEMORY §7.6 KPI; policy trigger at ≥5 GB / ≥1k sessions) |
| `ocserve_history_truncated_total{dropped}` | counter | prompt-history budget trips (8 MB, tail-weighted) — silent context loss made measurable |
| `ocserve_prompt_history_load_total{mode}` | counter | bounded prompt load: `full` / `window_budget` / `window_compaction`. A sustained `full` on a large session means the window could not be proven equivalent and the pre-change cost is back (the corpus probe is the byte-identity gate) |
| `ocserve_provider_prefix_changed_total{part}` | counter | KV/prefix cache invalidation attribution: `part="system"` (system prompt changed) or `part="tools"` (tool schema array changed) between two requests on the same session. A rising count on a stable config is a cache-busting regression worth a look |
| `ocserve_external_commit_total{source}` | counter | commits made by ANOTHER process into the data dir (the importer), observed as `PRAGMA data_version` changes on one long-lived reader; each observation bumps the write epoch, invalidating list/page/search caches. A persistent zero while importing means the server is not seeing the new rows |
| `ocserve_external_commit_poll_errors_total{source}` | counter | the watcher could not read `PRAGMA data_version`; caches silently stop invalidating while this rises |
| soak CSV `oc_rss` column | sample | native opencode RSS = the future-usage ceiling model (MEMORY §7.6) |
| `ocserve_prompt_rounds_total{bucket,finish}` | counter | uncensored rounds-per-turn distribution: `bucket` = 0-9/10-19/20-39/40-79/80-159/160+, `finish` = `done`/`error`/`capped` — the K-AUTONOMY telemetry the old flat cap made unmeasurable (right-censored at 25) |
| `ocserve_plugin_normalize_total{result}` | counter | plugin normalizer health (D1): sustained `error` = builds failing → raw fallback; `warm` on every load after first boot; `disabled` only when kill-switched |
| `rss_bytes`, `rss_peak_bytes`, `mcp_*` | gauge | 300 MB budget |
| `blob_orphans`, `blob_missing_total` | gauge/counter | storage integrity |
| `llm_request_duration_seconds{provider,model}`, `llm_ttft_seconds`, `llm_stream_errors_total` | histogram/counter | provider health (S6) |
| `events_emitted_total{type}` | counter | contract drift evidence |

Structured logging: `tracing` + `tracing-subscriber` JSON to stderr, `RUST_LOG` filter,
level per-module (both repos lacked a subscriber init — we ship it).

## 3. Tunability

Single source: `ocserve.toml` (validated at boot, `deny_unknown_fields`) with env overrides
`OCSERVE_*` for the systemd unit. Declared knobs — nothing is a magic constant in code:

```toml
[server]   port, hostname, auth_mode ("off"|"basic"), auth_password_file
[store]    writer_cache_kb=16384, reader_cache_kb=4096, reader_pool=4,
           wal_autocheckpoint_pages=1000, journal_size_limit=67108864,
           checkpoint_idle_ms=5000, freelist_vacuum_threshold=10000
[events]   ring_max_per_session=200000, ring_retention_days=30, sse_ring_events=4096
[plugins]  enabled (env OCSERVE_PLUGIN_RUNTIME=node|bun, OCSERVE_PLUGIN_HEAP_MB,
           OCSERVE_PLUGIN_NORMALIZE, OCSERVE_PLUGIN_TOOLS; hook budgets via
           OCSERVE_HOOK_TIMEOUT_MS; no [plugins] TOML section exists)
[mcp]      request_timeout_s=30, oauth_refresh_skew_s=120
[runtime]  worker_threads=8, max_blocking_threads=8, thread_stack_kb=1024,
           rayon_threads=8            # min(8, parallelism) default; env overridable
[blob]     chunk_bytes=1048576, zstd_level=3, gc_orphan_grace_hours=72
[metrics]  enabled=true, bind="127.0.0.1:9099"
```
### Upgrade and rollback

`scripts/install.sh --release --tag vX.Y.Z` fetches that tag's checksummed
artifact and installs it at the one stable path the unit executes,
`~/.local/bin/ocserve`, keeping the previous binary as `ocserve.prev` and
writing a receipt (`~/.local/share/ocserve/install-receipt`, with the SHA256)
so the running file can be tied to an artifact. Rollback is the same command
with the previous tag; 0.5.0 onward keep schema v15, so no downgrade step
exists. A failed probe leaves the new binary in place but exits non-zero, so
the operator knows to restore `.prev`.

Before 0.6.5 the unit executed `target/release/ocserve` while
`~/.local/bin/ocserve` held the verified download: two different hashes and no
way to tell which one was live. `install.sh` now places every install at the
stable path and renders `ExecStart` from it.

Config reload: SIGHUP re-reads safe subset (metrics, log level, ring limits); structural keys
(store, port) require restart — declared, not guessed. External config-file edits
(opencode.json / ocserve overlay / auth.json / auth-overlay / models cache / state model)
hot-reload via poller: `OCSERVE_CONFIG_WATCH=0` disables, `OCSERVE_CONFIG_POLL_MS`
(default 2000, min 100 — clamped so 0 can never busy-loop); fail-safe at runtime
(broken file keeps old state, retries; boot stays fail-fast).

Runaway/autonomy knobs — read **once at process start** (restart to change, like
`OCSERVE_PROVIDER_STALL_SECS`): `OCSERVE_PROMPT_MAX_ROUNDS` (default 0 = unlimited;
hard cap only — failure is surfaced, never silent) and `OCSERVE_PROMPT_MAX_COST_USD`
(default 0 = off; hard USD ceiling per prompt). Declared here, not guessed: both
have kill-criteria tests (`autonomy.rs`) and the bound lives on dollars/visibility,
not on a magic round count.

History-load knob — `OCSERVE_PROMPT_HISTORY_UNBOUNDED=1` (default unset) forces the
pre-change whole-session prompt load; read once at boot. It changes only how much is
READ: the `filter_compacted` selection and the 8 MB trim still apply, so the provider
request is identical. Unset (default) loads a proven `seq` suffix via
`ocserve_store::prompt_window` + `load_prompt_history`, falling back to the full load
whenever equivalence cannot be shown. Proof and negative controls:
`crates/ocserve-store/tests/prompt_window.rs`, `prompt.rs::bounded_prompt_load_tests`,
and the real-corpus probe `bounded_prompt_load_matches_real_corpus` (ignored by
default; run with `OCSERVE_CORPUS_DB` + `OCSERVE_CORPUS_SESSION`). Guard rule 57 keeps
every other prompt-path read on the bounded loader.

Plugin normalization knob — `OCSERVE_PLUGIN_NORMALIZE` (default **on**): content-hash-cached
rolldown normalization of plugin entries at load (D1 — decision record
`bench/normalizer-spike/D1-PLAN.md`). `=0` skips **before rolldown is constructed** (the
panic=abort residual gate); loads fall back to the raw entry — never worse than today's
behavior. Telemetry: `ocserve_plugin_normalize_total{result}` (`warm` hash hit / `built` +
A2 log line with destination, ms, RSS before→after / `disabled` kill-switch / `error`
WARN + raw fallback / `passthrough` double-normalize guard).

Provider knob — `OCSERVE_ZEN_KEYLESS` (default **on**): keyless opencode zen
endpoint (`Bearer public` + the proven discriminator wire — composite UA,
native session id, ≥2 known tool names; evidence ledger
`bench/zen-probe/FINDINGS.md` P5/P7/P8 + B1–B7). `OCSERVE_ZEN_KEYLESS=0`
restores the pre-port behavior (no keyless endpoint → deepseek fallback
default). Gate flips increment `ocserve_zen_freetier_total`
(`result="gate"` for a 403 tier rejection, `result="usage_limit"` for a spent
free allowance) and are checked nightly by the live trio
(`crates/ocserve-core/tests/zen_live.rs`: positive / text-only / negative — the
negative proves the wall still exists). A spent allowance surfaces upstream's
fixed upsell ("Free usage exceeded, subscribe to Go") as a typed
`APIError{statusCode:429,isRetryable:false}`, not a raw provider body: it is
terminal, so retrying only produces silent failures.

opencode.ai requests are sent over BoringSSL (hyper-boring), not rustls: their
free-tier gate fingerprints the TLS ClientHello, so a byte-identical request
passes from Bun's stack and returns 429 from rustls/OpenSSL/curl (5-stack
replay, 2026-10-25). Only that host uses the lane; all other providers keep
rustls. Named divergence `D-ZEN-TLS` in the PLAN §18 ledger.

Retired models are not served. The catalog marks models `status:"deprecated"`
(48 entries, 37 of them free models under `opencode` on 2026-10-25); freeze
deletes those, plus `alpha` ones unless `OPENCODE_ENABLE_EXPERIMENTAL_MODELS`
is set (`provider.ts:1744-1745`). ocserve applies the same rule in
`build_providers` and when choosing the effective default, so the picker and
the default can never disagree.

Model catalog (K-MODELS) — same-as-upstream refresh, no operator action
needed: honors `OPENCODE_MODELS_URL` / `OPENCODE_MODELS_PATH` /
`OPENCODE_DISABLE_MODELS_FETCH` (upstream truthy: `"1"`/`"true"`); source
default `https://models.opencode.ai/api.json`; fresh TTL 5 min; the serve
loop refreshes immediately-if-stale then every 60 min; `ocserve models
refresh` forces (upstream `opencode models refresh` parity). Shared cache
`~/.cache/opencode/models.json` (their opencode writes the same file —
coordination via the same lease under `~/.local/state/opencode/locks/`,
heartbeat 20 s / stale 60 s); UA mirrors
`opencode/{channel||latest}/{version||1.18.31}/{client||cli}` (env
overridable). Corrupt catalog self-heals (remove + refetch) — it never
fails boot. Visibility: `ocserve_models_refresh_total{result}` + nightly
`live_fetch_contains_big_pickle`.

## 4. CPU: measure first, specialize second

**Justified per-arch work:**
- zstd and sha256 (`sha2` with `cpufeatures`) dispatch to AVX2/SHA-NI at **runtime** —
  zero code, active on any capable host; verify with `ocserve doctor` (blob GC dry-run)
  before/after (expect nothing if already active).
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


### 5.x Blocking filesystem I/O (M3, PERF-10X Phase II)

`GET /find/file` and `GET /file` walk the filesystem. Both bodies now run via
`tokio::task::spawn_blocking`; the walk itself is unchanged (same bounds:
20,000 directories visited, stop at 4×limit results, 5,000 entries returned).
Reason: they were the last inline blocking syscalls on an async worker — the
same convoy class as the pre-F1 SQLite work, which parked a worker behind a
slow store call.

The load harness could not see this: its `LIST_PATH` is an empty `/tmp`
directory, so a k6 run never walked anything. Proof is structural instead —
`find_files_walk_runs_off_the_worker` asserts the walk executed on a thread
other than the async worker (thread identity recorded by a test-only seam).
**Negative control**: restoring the inline call makes that test red.

## 5. DevOps

- **The service is an OPTIONAL overlay, preferred, never implicit.**
  Foreground `ocserve serve` is the contract (tests, replay-check, the parity
  harness all run it directly and gain zero requirements). Nothing in build,
  tests, or `ocserve doctor` ever installs or enables anything; the ONLY code
  path that touches systemd is an explicit `scripts/install.sh` run, and
  `doctor` reports the overlay as one informational line either way.
- **Resource-control stack (2026-10-06, "middle ground": neither unbounded
  nor a knife-edge cap).** Every layer degrades to the one below on failure;
  the cap is the BACKSTOP, not the control:
  1. **L0 bounded-by-construction (always on):** 32 MB event ring, 8 MB
     tail-weighted history budget applied to a provably-equivalent `seq`
     suffix (the kill switch disables only the suffix read), streamed
     `/message`, `malloc_trim` cadence, `MALLOC_ARENA_MAX=1`, single catalog
     parse per reload.
  2. **L1 graceful sidecar recycle** (15 s sampler): RSS ≥
     `OCSERVE_SIDECAR_RECYCLE_MB` (default 280 — above the ~100 MB steady
     state and **below the 320 MB plugin leaf cap**, so recycle can actually
     win; `0` = kill switch) × 2 consecutive samples ∧ zero
     in-flight plugin RPCs (try_lock busy ⇒ skip) ∧ uptime ≥ 300 s → kill
     the sidecar (state is disk-backed; `ensure_alive` respawns + replays
     next trigger, warm-hash normalize no-op). Never interrupts a hook,
     never storms; precise RSS via the sidecar's own `child_pid` (the old
     "first node/bun child" scan could hit the browser host after any
     respawn). Metrics: `ocserve_sidecar_recycle_total{reason="rss"}`.
  3. **L2 cgroup partition** (`partition.rs`): `mkdir main kids → move self
     into main/ → +memory +pids +cpu → kids/memory.max=700M`, then four class
     leaves (`kids/{plugin,mcp,tools,pty}`) each with `memory.oom.group=1`,
     `pids.max=1024` (kernel no-internal-process rule) and its own
     high/max + `cpu.weight` (plugin 288/320, mcp 160/192, tools 448/640,
     pty 64/96; main 256/512/768 at weight 500 vs kids 100, tools 50).
     A partially applied budget reports `result="degraded"`, never `ok`. Every child spawn
     goes through `ocserve_store::cgroup` (guard rule 45), whose wrapper
     moves the child into its leaf and sets `oom_score_adj=500` before `exec`.
     **Placement is the control; the cap is the guardrail** (2026-10-21: a
     bash tool ran `nohup cargo build &`; the tool returned, the shell exited,
     and the build stayed in `main/` at adj=200 until the kernel killed the
     SERVER at 321 MB while the build held ~600 MB. Had it been in
     `kids/tools`, total would have been 223+600=823 MB — under the cap, no
     OOM at all. The cap number was never the bug). Leaves are separate
     because `kids/` holds independent services: `oom.group=1` on the
     aggregate would kill the browser host because a build spiked.
     `main/memory.low=256M` plus the unit's `MemoryLow=256M` makes main the
     reclaim-protected side (the unit setting is required: cgroup-v2 bounds a
     child's protection by every ancestor's `memory.low`, and all ancestors
     were 0 until 2026-10-23). Per-class caps now bound each service instead of
     only the aggregate, and `cpu.weight` makes a build yield to the API.
     Parent `MemoryHigh=960M` throttles before the 1024M backstop. Boot logs
     the budget and WARNs when it is tight; the comparison is deliberately
     about unreclaimable memory (page cache is charged to the parent but
     reclaimed under pressure: measured 924 MB of 1024 MB used with 364 MB of
     it file cache, no OOM).
     **Probe-first:** root-exists / owned-by-us / `memory` in controllers /
     subtree-writable probed before any mutation; any failure → metric
     `ocserve_cgroup_partition{result="unavailable"}` + today's flat shared
     cap. **Scope gate:** runs only under systemd (`INVOCATION_ID`) or
     forced `OCSERVE_CGROUP_PARTITION=1`; `=0` vetoes — bare/test/harness
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
  6. **Drain deadline (2026-10-21 wedge).** `bash` starts a reader thread per
     stream and used to `join()` them after the direct child exited. A command
     like `cd x && nohup server &` backgrounds the **whole AND-list as a
     subshell**; that subshell inherits fd 1/2 (our pipes) and waits on a
     server that never exits, so the pipes never reach EOF. The join blocked a
     blocking-pool thread **forever** and the session lock was never released
     (one wedged session, two of sixteen threads; sixteen incidents would stall
     every `spawn_blocking` caller). The tool timeout never applied because the
     direct child had exited normally.
     Fix: each call owns a `DrainGate`. The moment the direct child is reaped
     the gate closes after a 250 ms grace; Unix readers `poll(2)` with a 10 ms
     timeout so they can observe the gate, then stop reading. **Nothing is
     killed** — `cd x && nohup dev-server &` is a legitimate way to leave work
     running, and it is already contained by `kids/tools`. Windows keeps the
     blocking read (no pollable anonymous pipes) and documents the lingering
     reader as a build-only cost. `sift` uses the same gate. Guard rule 46
     requires a `close_in` before any reader `join`.
  7. **Lock-age visibility.** `ocserve_prompt_lock_age_seconds` plus a
     one-shot WARN past 600 s (the tool's own hard cap is 600 s). A wedged
     blocking task cannot be cancelled, so the honest remedy is to say so and
     let the operator restart the unit, which reaps the whole cgroup.

  5. **Containment: per-class caps, weights, one backstop.** Placement alone
     was never containment: until 2026-10-23 every leaf inherited
     `memory.max=max` (one build could starve the plugin host out of the shared
     700M pot) and the `cpu` controller was not enabled at all (a `cargo build`
     competed with the API on equal terms). The partition now writes:

     | node | memory.low | memory.high | memory.max | cpu.weight |
     |---|---|---|---|---|
     | `main` (server) | 256M | `max` (disabled) | 768M | 500 |
     | `kids/` aggregate | - | 640M | 700M | 100 |
     | `kids/plugin` | - | 288M | 320M | 100 |
     | `kids/mcp` | - | 160M | 192M | 100 |
     | `kids/tools` | - | 448M | 640M | 50 |
     | `kids/pty` | - | 64M | 96M | 100 |

     Leaf caps are ceilings, not reservations, so they sum above the parent;
     the kernel explicitly allows over-committed limits. `high < max` means a
     class is throttled and reclaimed before it is killed.

     **`main` deliberately has NO `memory.high`** (2026-10-24): it holds exactly
     one process, and `high` throttles by putting the allocating task into
     direct reclaim. With no swap and almost all anon pages that reclaim cannot
     make progress, so the server spent 325,225 high events at 601 MiB against
     a 512 MiB high and answered nothing for over an hour while the unit stayed
     `active`. `high` is for elastic classes; the parent `MemoryHigh` still
     throttles the service as a whole and `max` remains the backstop. Weights are
     work-conserving: a build uses every idle core and yields when the API
     needs one. `oom.group=1` stays on leaves only.

     **Backstop:** the unit carries `MemoryHigh=960M` and `MemoryMax=1024M`
     (plus `MemorySwapMax=0`). The numbers are a documented backstop, not a
     tuning surface: the controls are the per-class caps and weights. A
     larger ceiling costs nothing at idle (cgroups charge on touch); leak
     detection is the recycle gauge and soak slope, never cap proximity.

     The dance needs a **delegated** cgroup tree; it works because the user
     manager is delegated, and the unit itself does not set `Delegate=yes`.
     `OOMScoreAdjust=200` is pinned so the 300-point gap to children is a
     property of our config. `Restart=on-failure`, `RestartSec=5`,
     `RUST_LOG=ocserve=info`, hardening (`ProtectSystem=strict`, optional
     `ReadWritePaths`, `NoNewPrivileges`).

  6. **Heavy work is not the service's policy.** A build inside the agent is
     an **unbounded external workload**; the service contains it but does not
     schedule it. Run heavy builds in a scope the machine, not the service,
     governs:

     ```sh
     systemd-run --user --scope --collect \
       -p MemoryHigh=4G -p MemoryMax=6G -p CPUQuota=800% -p Nice=10 \
       --working-directory="$PWD" \
       cargo build --release
     ```

     The scope is outside `ocserve.service`, so it does not count against the
     service's cap and the service cannot kill it; the tradeoff is that the
     service's tool timeout and cleanup do not govern it either. For a standing
     policy, put those three properties in
     `~/.config/systemd/user/ocserve-build.slice` and use
     `systemd-run --user --slice=ocserve-build.slice`. The anti-pattern that
     caused the 2026-10-23 kill: `nohup cargo build &` inside the agent stays
     in `kids/tools`, inherits the class cap, and is killed as a unit.
     **We deliberately do not set `CARGO_BUILD_JOBS`, `RAYON_NUM_THREADS` or a
     CPU quota for tool children**: that is workload policy, invisible to the
     user, and a divergence from upstream's shell tool.

  7. **Unmanaged install.** `scripts/install.sh --no-resource-limits` renders
     the unit without `MemoryHigh`/`MemoryMax`/`MemorySwapMax` and sets
     `OCSERVE_CGROUP_PARTITION=0` and `OCSERVE_SIDECAR_RECYCLE_MB=0`; the
     operator owns memory. **Placement is gone in this mode**: the server and
     every child share the unit cgroup, which now has no memory ceiling, and a
     tool child can consume the whole machine's free memory; only
     `oom_score_adj=500` on children remains to bias the global OOM killer
     toward them, and the plugin host is no longer recycled. Timeouts, OOM
     policy and the rest of the service are unchanged. This is for operators
     with their own slice policy; the default install is the managed one.

- **Install (preferred, opt-in by invocation):** `scripts/install.sh` —
  default = full (build-if-needed, binary used IN PLACE, render
  `deploy/ocserve.service` (single source of truth), `daemon-reload`,
  enable/restart, health-probe, print overrides + kill switches + uninstall
  hint); `--dry-run` (diff, touch nothing), `--bin-only`; non-systemd host
  auto-falls to bin-only. Writes `install-receipt` (BIN=) so uninstall can
  attribute the binary. Never sudo, never non-ocserve units (rule 6).
- **Install from CI (no toolchain):** `scripts/install.sh --release [--tag vX.Y.Z]`
  fetches the tagged artifact with `scripts/fetch-release.sh`, verifies it
  against `SHA256SUMS`, and places it at `~/.local/bin/ocserve` before
  rendering the unit. Placement is deliberately outside `$TMPDIR`: the unit
  runs an absolute path, so a temp-file binary would vanish at the next reboot
  and take the service with it. The fetcher's `--selftest` (run by guard rule
  39) proves a corrupted tarball is rejected.
- **Release is a tag, not a local build.** `release.yml` fires on `v*` and
  publishes linux x86_64 + aarch64 (glibc), linux x86_64 (static musl), and
  macOS/Windows x86_64 + aarch64 tarballs + `SHA256SUMS` to the GitHub Release;
  `publish.yml` also fires on the same tag and publishes all 17 crates in
  dependency order, with `ocserve` last. A cancelled matrix leg makes the
  attach step skip, so the assets are only there once every target succeeds
  (re-run the failed leg; it is usually a reclaimed runner, not a build
  failure).
- **Uninstall (application-clean; history sacred):** `scripts/uninstall.sh`
  — default removes ocserve\* units/timers/drop-ins (glob), the
  receipt/ExecStart binary (dev `target/` builds KEPT with a note), and
  ocserve-derived `*.normalized.mjs{,.hash}` artifacts (basename-gated);
  **session history KEPT** with sizes + purge hint. `--purge` = stats →
  confirm (`--yes` for scripts) → optional `VACUUM INTO` backup
  (`--no-backup` to skip) → data/state removal (path from unit
  `Environment=`, refused unless under `$HOME` AND containing `ocserve`;
  custom non-conforming dirs are reported, never deleted). Shared opencode
  state (config, `auth.json`, `opencode.db`, `models.json`, plugin
  packages) is NEVER touched — guard rule 11 + staged `--selftest` canary
  battery (ocserve artifacts gone, canaries byte-identical, receipt binary
  removed, dev build kept, idempotent, history honored; nightly-wired).
  Journal vacuum deliberately NOT done: the journal is shared with
  opencode and unit logs age via the retention floor.
- **Release profile** (matches reliary8/stria): `lto="fat"`, `codegen-units=1`,
  `opt-level=3`, `panic="abort"`, `strip=true`; **binary size ceiling 62,000,000 B (62 MB)**
  — re-baselined 2026-10-18 from the measured K-SECREV build (61,479,024 B: the
  complete web-UI closure for both packs, incl. 36 MB of lazily-loaded
  office-preview WASM; prior: 34,000,000 B from the Phase E second pack).
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
  - `ocserve-nightly.timer` 06:30 → `scripts/nightly.sh`: `cargo audit`, `cargo deny check`
    (`deny.toml`), SIGKILL blob/DB crash fuzz, provider stream fuzz (10 k seeded
    chunk-boundary splits vs whole-buffer parse), backup drill (live `VACUUM INTO` +
    integrity + row counts), binary size ceiling; `--with-mutants` = manual/weekly
    (never during benchmark sessions).
  - `ocserve-drift-watch.timer` 06:00 → `scripts/drift-watch.sh` (upstream release
    triage — adopt/ignore, never auto).
  - `ocserve-soak.timer` daily → fresh 24 h soak CSV at
    `~/.local/state/ocserve/soak/soak.csv`; `scripts/soak-gate.sh <csv>` machine-checks
    slope/health (replaces eyeballing). Restart manually after every deploy.
  All timers `Persistent=true` (missed runs catch up on boot).
- **Runbook:** `ocserve doctor` = boot checks + storage health + FTS integrity + blob GC dry-run
  + version/contract info; `ocserve import` (legacy snapshot), `ocserve replay --target|--pair`,
  and the harnesses `scripts/{replay-check,pair-check,load-test,capdiff-check}.sh` (`ocserve bench` was
  documented here but NEVER existed — clap rejects it; corrected 2026-10-06).
- **GitHub CI (`.github/workflows/`, family convention — pinned action SHAs with version
  comments):**
  - Per-PR: `ci.yml` (fmt, clippy `-D warnings`, workspace tests, `check-guards.sh`,
    `check-matrix.sh`, `cargo package --list` for every crate, publish-order guard, test-count
    ratchet floor, binary-size ceiling, aarch64 cross-check, cargo-deny/audit, artifact upload);
    `replay.yml` (route replay against the committed corpus — offline, no freeze needed);
    `codeql-analysis.yml`; `pr-secret-scan.yml` (gitleaks over the PR range + `.gitleaks.toml`);
    `dependency-review.yml`.
  - **Freeze differentials are LOCAL, not hosted:** the GitHub-hosted runner cannot drive the
    vendored opencode 1.18.31 (a message POST returns HTTP 500 there — runner-specific;
    byte-identical binary is green on the dev box). `capdiff` (provider boundary) and
    `pair-check` (live server-to-server) therefore run in the local `scripts/nightly.sh`, and
    `scripts/acquire-freeze.sh` stages the freeze binary (npm `opencode-ai@1.18.31`) for a
    from-scratch box. Promote to PR CI when a self-hosted runner exists.
  - Scheduled: `hardening.yml` (deny hard gate + unsafe-policy assertion + weekly outdated),
    `fuzz.yml` (new `fuzz/` crate: `sse_parser`, `splice_parser`, `wire_decode`), `bench.yml`.
  - Release: `publish.yml` (tag or manual dry-run; ordered crates.io publish, covers every
    workspace member in dependency order — `scripts/check-publish-order.py`), `release.yml`
    (linux/macos x86_64+aarch64 + windows, per-target sha256 + combined SHA256SUMS).
  - `scorecard.yml` + `dependabot.yml`. Heavy Docker benches (`parity`, `load-test`, `soak`)
    run locally/manually — no hosted runner yet (decided 2026-10-12).
  - The nightly `scripts/nightly.sh` remains the local superset (it includes the live-DB
    steps CI cannot run: splice/list parity over the live db, backup drill, soak gate).
  - **Provider-boundary review (speed/efficiency):** `scripts/capdiff-check.sh --full` diffs
    every provider request the two arms send (sequence) + the SSE event-type set + a cost
    axis (bytes/round-trips; ocserve inflation = red), and prints ocserve `/metrics` deltas
    (`ocserve_db_opens_total`, `ocserve_prompt_rounds_total`, `ocserve_llm_request_duration_seconds_count`,
    `ocserve_rss_bytes`). Freeze serves its SPA at `/metrics` (no metrics) → `n/a`, never 0
    (`D-PARITY-METRICS`). Determinism: run `--full` **and** `--full --reverse` — captures must be
    byte-identical. Speed *latency* claims live in `bench/parity` + `bench/load`; capdiff's cost
    axis is a regression guard, not a latency claim. Result artifact: the JSON report on stdout.

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
