# Changelog

All notable changes to ocserve. The wire contract targets the frozen
openccode 1.18.31 release and the v2 surface the 2.0.28 client; anything that
differs is a named `D-*` row in `PLAN.md §17`.

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
