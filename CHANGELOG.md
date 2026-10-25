# Changelog

All notable changes to ocserve. The wire contract targets the frozen
openccode 1.18.31 release and the v2 surface the 2.0.28 client; anything that
differs is a named `D-*` row in `PLAN.md §17`.

## Unreleased (after 0.6.7)

### Fixed
- **A spent opencode free allowance failed silently.** Three consecutive
  attempts on 2026-10-25 died with the raw provider body (429
  `FreeUsageLimitError`) and only a log line; the client showed nothing
  actionable and the user retried blindly. It is now classified as terminal
  and surfaced with upstream's exact upsell ("Free usage exceeded, subscribe
  to Go") as a typed `APIError{statusCode:429,isRetryable:false}`, plus
  `ocserve_zen_freetier_total{result="usage_limit"}`. The keyless/free tier is
  not free at scale: the allowance is shared and finite.
- **Retired models were still listed.** The served model map included every
  catalog entry, so `opencode` showed 46 models, 37 of them `deprecated`
  (upstream shows 9). Freeze deletes `status:"deprecated"` and, unless
  `OPENCODE_ENABLE_EXPERIMENTAL_MODELS` is set, `status:"alpha"`
  (`provider.ts:1744-1745`); ocserve now does the same in `build_providers`
  and when picking the effective default, so the picker and the default agree.

### Added
- `ocserve import --session <id>` (repeatable) and `--force` are now reachable
  from the CLI, matching the documented behaviour: adopt named sessions into a
  data dir that already has history (the search trigger is kept, transactions
  stay short, so a running server is safe), refuse a session that is already
  present unless `--force`, and require `--force` to name at least one
  session so a destructive import is never implicit. Session task lists are
  adopted too.
- `ocserve import --bulk` gives a populated data dir the fast index path while
  the server is stopped. Before this, stopping the server changed nothing: the
  strategy was chosen from the target's contents (`existing == 0`), so an
  existing data dir always took the per-row trigger path. `--bulk` drops the
  trigger, streams the parts and rebuilds the index once; on the measured
  5,722-part session that was 26-32 s versus 40-47 s incremental (warm), and
  the warning states that a server must not be writing.
- A live import above 10,000 source parts is refused before writing anything
  unless `--allow-slow-live` or `--bulk`: per-row indexing was measured at
  ~140 parts/s on a small populated target, and a 21k-message / 80k-part
  adoption exceeded 15 minutes with no progress output.
- `--force` snapshots the target with `VACUUM INTO` before deleting anything,
  and applies in every mode (a bulk re-import used to bypass the gate and let
  `INSERT OR REPLACE` cascade the old session away).
- The bulk index rebuild and trigger recreation are one transaction, and the
  parity stamp records `count(part_search)`; the boot gate now compares the
  index to the rows, so a row committed during the old gap is repaired instead
  of being recorded as healthy.
- The external-commit watcher survives a backup restore (per-path state keyed
  by device+inode, reopen on replacement), runs on the blocking pool, and
  counts poll failures.
- The bounded prompt load is conservative whenever ANY compaction row exists:
  an unfinished imported summary reorders history under the full filter, so a
  suffix is only used when the session has no compaction rows or the boundary
  resolves exactly.
- Source parts are size-checked before the target is created, and source paths
  are URI-encoded (`#`/`?`/`%` no longer open a different file).
- Nullable upstream columns no longer abort the import. `session.path` is
  nullable in the opencode schema; reading it as a `String` failed the whole
  import with `Invalid column type Null` on a real row.
- `ocserve doctor --env` and `ocserve env-reference` expose every `OCSERVE_*`
  variable with its default and accepted values. `docs/ENVIRONMENT.md` is
  generated from that contract, and guard rule 58 fails the build if a runtime
  read is undocumented or the committed reference is stale.
- The server notices commits made by another process into its data dir
  (`PRAGMA data_version` on one long-lived reader, folded into the write epoch
  by the 15 s sampler), so an importer's rows cannot leave the list, page or
  search caches stale.

### Changed
- **Prompt history is read as a proven suffix instead of the whole session.** A
  56k-message session stores ~85 MB of parts while the 8 MB history budget keeps
  only the tail. `prompt_window` proposes a `seq` suffix mirroring
  `filter_compacted`'s stop condition; `load_prompt_history` accepts it only when
  the exact byte trim proves it, or the compaction boundary sits at its start.
  Every other shape falls back to the full load. Interleaved, warm-up discarded:
  deep session 4.6-5.2 s -> 1.45-1.59 s, compacted session 117 ms -> 22 ms, a
  dense mid-size session 2.89 s -> 2.56 s; emitted provider messages are
  byte-identical in every case. Kill switch
  `OCSERVE_PROMPT_HISTORY_UNBOUNDED=1`; metric
  `ocserve_prompt_history_load_total{mode}`; guard rule 57.
- **Prefix-cache changes are observable.**
  `ocserve_provider_prefix_changed_total{part="system"|"tools"}` counts
  per-session changes of the system prompt and tool-schema array between
  requests, so a silent KV-prefix invalidation is visible instead of merely
  expensive.
- The per-tool-call permission path no longer re-parses the 26 KB provider tool
  schema JSON to answer "is this a builtin" (`ocserve_tools::is_builtin`).

### Fixed (second review pass)
- **The server hung under `memory.high`.** `main/memory.high=512M` throttled the
  single server process into direct reclaim it could never satisfy (no swap,
  almost all anon): on 2026-10-24 it sat at 601 MiB with 325,225 high events,
  four threads parked in `mem_cgroup_handle_over_high`, and served nothing for
  over an hour while the unit stayed `active`. `main` no longer gets
  `memory.high`; the parent `MemoryHigh=960M` still throttles, `max` is the
  backstop. Regression test pins the absence.
- `--version` now reports the build commit (`ocserve 0.6.7 (e1a54dd)`), and the
  install receipt records `COMMIT=`, so a local build is distinguishable from a
  released artifact.
- Per-agent `tools:{bash:false}` in config now becomes permission rules, as
  upstream normalizes it; it was silently ignored.
- The forwarder retries forever (`StartLimitIntervalSec=0`) so a boot with
  Tailscale down cannot leave it permanently failed.
- `install.sh` no longer re-enables a forwarder the operator deliberately
  disabled; it only starts an already-enabled one.
- Plugin-tool execution allow-list is intersected with the retained wire set,
  so a name dropped by a deny rule cannot execute.
- `check-matrix.sh` has an explicit docs-only opt-out; the default stays
  fail-closed.

### Fixed (first review pass)


### Fixed
- **Tailnet access died on every deploy.** The forwarder unit used
  `PartOf=ocserve.service`, which propagates stop but not start, and
  `install.sh` restarts ocserve on every install. It is now independent
  (`Wants`+`After`, `Restart=always`), install restarts it when present, and
  `doctor` prints `TAILNET IS DOWN` instead of a neutral line. Guard 55.
- **A policy-denied plugin tool was still executable.** The advertised set is
  also the execution allow-list, and the name was inserted before the policy
  check, so a plan-mode-denied tool could be called by name and was gated as a
  wildcard write rather than `edit`. Insert now happens only on the advertised
  path.
- **Plugin tools could duplicate an MCP tool name.** The reserved set now
  covers every advertised name (builtin + MCP), not just builtins; a duplicate
  is a provider 400 and the plugin branch wins dispatch over MCP.
- `check-matrix.sh` failed open when the workspace did not build (it printed a
  WARN and exited 0); it now fails, since CI runs it after a successful build.
- `check-v2-hooks.py` no longer passes on an empty surface, an empty or
  commented-out hook const, or a stale fixture on both sides.
- Guard 54 is semantic (reserved source + policy-before-insert) with a planted
  control for each half; guard 53c rejects a `panic!` stub.

## 0.6.7

### Fixed
- v2 plugins that register `experimental.ws.handshake`/`send`/`receive` no
  longer fail to load. Upstream declares those hooks, but the shim's accepted
  list omitted them, so registration threw `unknown session hook` and the
  plugin's whole `setup` failed (losing every hook we do dispatch). All 16
  declared hooks are now accepted; the six with a dispatch site still run, the
  rest stay named divergences.
- `ctx.app.version` reports the running server version instead of a hardcoded
  `0.3.1` left over from Phase 3.
- The v2 capability matrix said `session.prompt` was diverged (it has been
  dispatched since 0.6.0) and listed five read domains as throwing proxies (they
  are proxied). Corrected; rule 36 now also requires every extracted hook to be
  *registrable*, with a planted removal as the control.

## 0.6.6

Everything in 0.6.5 plus the install provenance, advisory and CI fixes that
landed after that tag. No schema, wire, or storage change.

### Fixed
- `install.sh` installs to the one path the unit executes
  (`~/.local/bin/ocserve`, with `.prev` and a SHA256 receipt) instead of
  pointing `ExecStart` at the build output.
- tokio 1.54.1 (RUSTSEC-2026-0386, `AsyncFd` I/O-safety unsoundness).
- `--render-to` is a pure render: no build, no install.

## 0.6.5

The 0.6.4 tag never produced a release: both Windows jobs failed on a missing
non-unix `Shutdown` stub. 0.6.5 is 0.6.4 plus that fix. Content otherwise
identical; no schema, wire, or storage change.

## 0.6.4

Fixes from independent SRE and security reviews of 0.6.3. No schema, wire, or
storage change; rollback is a binary swap.

### Fixed
- Graceful shutdown closes the event bus, so an open SSE client can no longer
  hold the server until systemd SIGKILLs it (`drain_budget.rs`).
- SIGTERM is registered before all startup work and the drain deadline starts
  at the signal (`OCSERVE_DRAIN_SECS`, default 20 s).
- Partial cgroup budgets report `degraded`, never `ok`.
- L1 recycle default 280 MB so it sits below the 320 MB plugin cap.
- `MemoryLow=256M` on the unit makes `main/memory.low` effective.
- Spawns without a leaf clear `OCSERVE_KIDS_CGROUP`.
- `install.sh --render-to`; guard 53b asserts rendered units in both modes.
- Restored the non-unix `Shutdown` stub: the 0.6.4 tag failed both Windows
  release jobs with `cannot find function install_shutdown`, because the host
  build and every Linux test exercised only the unix variant. Guard 53c now
  requires both cfgs and a planted removal proves it bites.

## 0.6.3

### Fixed
- SIGTERM is registered as soon as the server starts (before boot checks,
  partition, config load, MCP probe and plugin load), and the drain deadline
  is measured from the signal, not from startup: `OCSERVE_DRAIN_SECS` (default
  20 s, 0 = wait forever). The 0.6.2 fix only covered the bind-to-serve
  window; a stop during startup, or a drain with an open connection, still hit
  systemd's 90 s timeout and SIGKILL. (The first version of this fix wrapped
  the signal wait itself, which shut the server down 20 s after every start;
  `drain_budget.rs` pins the correct behaviour.)
- `MemoryLow=256M` on the unit so `main/memory.low` is actually effective:
  kernel cgroup-v2 limits protections by the ancestors' `memory.low`, and
  every ancestor was 0.
- A partially applied partition (missing leaf, failed cap write, non-delegable
  kids subtree) now reports `degraded` instead of `ok`; it still never fails
  boot.
- `doctor --budget` reports per-node `memory.events` and pressure, and
  distinguishes "installed but stopped" from "not installed".
- Graceful shutdown closes the event bus, ending SSE streams. A connected
  event-stream client used to hold `axum::serve` open, so every restart waited
  out the drain budget (and 90 s SIGKILL before the budget existed). Verified
  with a live SSE client: shutdown now completes in under 2 s.
- L1 sidecar recycle default lowered 450 → 280 MB: 450 sat above the plugin
  leaf's 320 MB cap, so the kernel always won and the graceful path was dead
  code. Ordered invariant pinned by tests.
- Spawns with no leaf now clear `OCSERVE_KIDS_CGROUP` instead of inheriting an
  outer one; `OCSERVE_KIDS_MEMORY_MAX_MB` is range-checked; `doctor --budget`
  rejects a `..` cgroup path; `install.sh --render-to` lets the guard assert
  the rendered unit in both modes (the old rule grepped source text).
- `soak-gate.sh` now rejects samples above the parent or kids memory max (a
  500 MB kids allocation used to pass); `soak.sh` no longer references an
  undefined `$BASE` for a metric that does not exist.

## 0.6.2

### Reliability
- SIGTERM is registered **before** the socket binds. Previously the signal
  stream was created when `axum::serve` first polled it, so a signal in the
  bind-to-serve window killed the process without draining (seen once as
  `ExitStatus(15)`). `shutdown_race.rs` pins it with a deterministic window.

### Resource containment
- **Per-class memory caps and CPU weights.** Every child leaf now has its own
  `memory.high`/`memory.max` (plugin 288/320M, mcp 160/192M, tools 448/640M,
  pty 64/96M) and the `cpu` controller is enabled: the server weighs 500
  against the child classes' 50-100, so a build uses idle cores and yields
  when the API needs one. Previously all leaves inherited `max`, one shared
  700M pot, and no CPU priority.
- The aggregate `kids/` throttles at 640M before its 700M cap; `main` has a
  512M soft ceiling and a 768M backstop; the unit carries `MemoryHigh=960M`
  under the existing `MemoryMax=1024M`.
- `ocserve doctor --budget` prints the resolved caps, per-leaf current/peak,
  `memory.events` and CPU throttling.
- `scripts/install.sh --no-resource-limits` installs the unit without memory
  caps and disables the child partition and sidecar recycle for operators who
  own memory policy.
- The service deliberately does **not** set build job counts or CPU quotas;
  heavy builds belong in a `systemd-run --user --scope`, documented in README
  and SRE section 5.6.



Security and reliability review of 0.6.x (two independent audits). Fixes:

### Critical
- **A dead reader left a live-but-deaf host.** One host line over 8 MiB stopped
  the response reader while the child stayed alive, so every later hook burned
  its full budget and failed until restart. The reader now poisons the
  generation and the owner replaces the host; requests on a dead generation
  fail immediately instead of waiting.
- **The hook budget did not cover the stdin write.** A host that stopped
  draining blocked every caller on the stdin mutex with no timer. The budget
  now covers the write and the wait.

### High
- **A recycled or crashed sidecar silently disabled every plugin.** The 0.6.0
  handle refactor removed `ensure_alive()` from the traffic path and
  `recycle_child()` also cleared the loaded-plugin map, so `registers()` said
  "no plugins" forever. Recovery now runs on the hot path, and a recycle keeps
  the load knowledge.
- **A plugin tool returning a non-object `metadata` aborted the process.**
  serde_json's `IndexMut` panics on non-objects and release builds abort. The
  value is coerced at both boundaries.
- **The boot `config` hook result was overwritten** by a following
  `reconcile()` that re-read the file, so a plugin that tightened permissions
  was reverted milliseconds later. Applied once, and the follow-up only runs
  when the apply failed.
- **Plugin tools bypassed the `plan` agent's edit ban.** The gate sees a tool
  name, never its effects. A plugin tool now needs explicit `allow`, or an
  agent with bash-level authority; `ctx.ask` throws instead of silently
  approving.
- **Plugin tool names could collide** with builtins or each other, which is a
  provider 400 for every request. Refused and counted.
- **The `config` hook was serialized** behind the old mutex at three more sites;
  all now use the handle path.

### Medium
- `plugin_origins` (an absolute home path) no longer reaches the served config.
- Plugin tool output is bounded in the host, before it crosses the pipe.
- Pending RPC entries are removed by a `Drop` guard, so an aborted turn cannot
  wedge the recycle gate.
- `compacting` now matches the 60 s budget (the substring check missed it).
- `soak-gate.sh` no longer accepts a failed probe (`000000` coerced to 0).
- New metrics: plugin tool calls/failures/duration, sidecar respawns,
  config-apply failures, event-delivery failures, sidecar up/down.
- Load failures are `ERROR` with a counter instead of a single boot warning.

## 0.6.1

### Fixed
- Plugin tools advertised an **invalid JSON Schema** when the plugin's Zod was
  v3 (resolved first) or when the schema was generated from the normalized
  entry, where the plugin's `node_modules` is not resolvable. Every request in
  such a session got a provider 400. The conversion now resolves `zod/v4`
  first, uses the plugin's real entry, mirrors upstream's normalizer, and
  never emits a raw Zod object.

## 0.6.0

Both the safety work and the feature work are in this release: hook
serialization and unbounded hooks were fixed first, then the capability
gaps. They were developed and tested together, so they ship as one rollback
unit.

### Safety
- Per-hook timeouts derived from measured maxima on a live install
  (`v2.session.context` 15.3 s, `tool.execute.after` 30.0 s): 60 s for
  transforms/context/compaction, 180 s for tool hooks, 300 s for plugin tools.
  `OCSERVE_HOOK_TIMEOUT_MS` overrides every hook budget; `0` restores upstream
  parity (unbounded).
  Named divergence `D-HOOK-TIMEOUT` (upstream has no hook timeout at all).
- **Hook dispatch no longer serializes sessions.** The sidecar is reached
  through a cloneable handle; the server mutex is released before awaiting, so
  two sessions' hooks interleave. The event pump uses the same path.
- `ocserve_plugin_hook_timeout_total`, `ocserve_plugin_internal_error_total`,
  per-hook duration maxima.
- Plugin-internal hook failures (a plugin catching its own exception and
  returning the input unchanged) are counted per plugin and logged.

### Added
- **v1 plugin tools.** `hooks.tool` registrations are advertised to the model.
  The JSON Schema is generated in the plugin's own runtime (`z.toJSONSchema`,
  falling back to the legacy plain-object shape), so there is no Rust-side Zod
  converter. Execution runs in the sidecar with the reference's
  `PluginToolContext`; output is bounded to 1 MiB and failures become error
  tool results. Kill switch: `OCSERVE_PLUGIN_TOOLS=0`.
- **v2 `session.prompt` hook** fires before admission and its returned prompt
  is what the inbox row carries, matching upstream `SessionPrompt.prepare`.
- **v2 read-only context domains** (`model`, `provider`, `agent`, `integration`,
  `reference`) are proxied to the routes ocserve already serves instead of
  throwing a false `D-V2-DOMAIN`.
- **`ocserve doctor --plugins`**: per-plugin dialect, registered hooks,
  bridged/unbridged transforms.
- Demand ledger (`bench/plugins/demand.json`) + guard 50: every hook the
  ecosystem declares is classified as bound or named-diverged.

### Changed
- Hooks nobody registered are skipped without a sidecar round-trip.
- The plugin `config` hook's returned object is applied through the same
  reconcile path as a hot reload. Previously it was called and discarded.

## 0.4.1
- v1 hook-contract capture (`scripts/capture-hooks.sh`), replay/order/effect
  gates, and the fixes they found: `messages.transform` payload, length-changing
  compaction output, `tool.execute.after` keys, `tool.definition`, `shell.env`,
  and the `ctx.client` `{path,query,body}` convention.

## 0.4.0
- v2 plugin dialects (promise + Effect), command/skill transforms, RPC dispatch.

## 0.3.1
- Repository/asset packaging fix; `cargo install` works again.

## 0.3.0
- v2 plugin surface, snapshot engine, revert semantics.

## 0.2.0
- Full CI suite, aarch64 release, Linux/macOS PTY.

## 0.1.0
- First public release.
