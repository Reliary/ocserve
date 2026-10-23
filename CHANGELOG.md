# Changelog

All notable changes to ocserve. The wire contract targets the frozen
openccode 1.18.31 release and the v2 surface the 2.0.28 client; anything that
differs is a named `D-*` row in `PLAN.md §17`.

## 0.6.0

Both the safety work and the feature work are in this release: hook
serialization and unbounded hooks were fixed first, then the capability
gaps. They were developed and tested together, so they ship as one rollback
unit.

### Safety
- Per-hook timeouts derived from measured maxima on a live install
  (`v2.session.context` 15.3 s, `tool.execute.after` 30.0 s): 60 s for
  transforms/context/compaction, 180 s for tool hooks, 300 s for plugin tools.
  `OCSERVE_HOOK_TIMEOUT_MS` overrides; `0` restores upstream parity (unbounded).
  Named divergence `D-HOOK-TIMEOUT` (upstream has no hook timeout at all).
- **Hook dispatch no longer serializes sessions.** The sidecar is reached
  through a cloneable handle; the server mutex is released before awaiting, so
  two sessions' hooks interleave. The event pump uses the same path.
- `ocserve_plugin_hook_timeout_total`, `ocserve_plugin_internal_error_total`,
  `ocserve_plugin_hook_noop_total`, per-hook duration maxima.
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
  bridged/unbridged transforms, and the last internal error.
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
