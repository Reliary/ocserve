# Gap-closure plan: absolute compatibility (v1 + v2), low maintenance, high performance

Status: **Tiers 1–3 complete** (2026-10-18). Tier 2 shipped as the
`command`/`skill` transform pair (`K-V2-TRANSFORM`, PR #13); the remaining
domains stay named divergences with their reasons recorded below. Scope
agreed 2026-10-17 (all tiers; foreground-only `task`; v2 acceptance pinned to
the vendored **2.0.28**; service-bound divergences kept).

## Findings this plan closes

- **v1: 188 implemented / 0 cited / 0 gaps**; v2: **142 / 0 / 0** (both spec-driven,
  guard rules 16/16b).
- **`task` was the one genuine functional hole — now closed** (`K-TASK`). The
  frozen `provider-tools.json` advertises `task` and the default system prompt
  tells the model to use it; the tool now recurses the single shared executor on
  a child session, with a `parentID` depth guard and derived permissions.
- **The one unbacked claim: no live v2 client ever booted against ocserve.**
  v2 is proven by `pair-v2-check` driving the real 2.0.28 CLI **as a server**;
  Phase A's own exit ("real v2 TUI reaches session list") was never met. The v1
  TUI (`tui-check.sh`) and v1 web UI *were* live-verified.
- All other differences are **named divergences**, never silent.

## Research anchors (verified this session)

| Work | ID | Use |
|---|---|---|
| Mosaic — multi-session agentic RL (forking + compaction as first-class) | arXiv 2610.22174 | frames `task` (child session + `parentID` lineage) and compaction |
| RCM — responsibility-centered delegation | arXiv 2610.21243 | child-permission derivation (child inherits parent deny + external_directory) |
| APIDiffer — spec-guided differential testing | OOPSLA'26, arXiv 2603.06029 | validates pair-check/pair-v2 as the compat oracle |
| SmartOracle — agentic triage of differential noise | arXiv 2601.15074 | triage posture (deterministic first) |

Non-goal recorded: SSR agent-discovery (arXiv 2610.21113) — our `task` takes an
explicit `subagent_type`, so candidate-context scaling is a non-problem.

## Tier 1 — close the real hole + the unbacked claims

### 1.1 `task` subagent tool (clears `D-TOOLS-TASK`)
- add `task` to `IMPLEMENTED` + the advertised set (frozen schema).
- launcher recurses the **shared** `run_prompt_with` on a child session (no second
  loop): child `parentID = ctx.sessionID`, title `"{description} (@{agent} subagent)"`.
- child permission = `derive_subagent_permission(parent_rules, subagent)` — parent
  `deny` + `external_directory` rules, plus `todowrite`/`task` denies unless the
  subagent permits (RCM framing; upstream `subagent-permissions.ts` parity).
- depth guard walking the `parentID` chain (`subagent_depth`, default 1).
- model = subagent's model else parent's; `ctx.ask({permission:"task",
  patterns:[subagent_type], always:["*"]})` before spawn.
- output block `<task id=… state=…><summary>…</summary><task_result>…</task_result></task>`.
- **foreground-only**; background → named divergence `D-TASK-BACKGROUND`.
- tests: spawn→answer e2e, depth refusal, permission derivation, unknown agent type,
  planted control (task advertised but unimplemented → capdiff red).

### 1.2 Live v2 client acceptance (arbitrates Phase A)
- pin `@opencode/cli-linux-x64@2.0.28`; boot ocserve `OCSERVE_V2=1`; run the v2 CLI
  against it (`--server http://127.0.0.1:PORT` where supported), capture the route
  hit-set, assert every route returns JSON (v2 twin of `scripts/tui-check.sh`).
- any HTML route = the crash class → fix or name it.

### 1.3 Live v2 web-UI browser run with `OCSERVE_UI_PACK=2.0.28` (mirror v1 acceptance).

## Tier 2 — v2 behaviour depth (mutating domains only) — COMPLETE, scoped

Shipped (`K-V2-TRANSFORM`): `command.transform` + `skill.transform`. Upstream's
`CommandEditor` exposes only `add`, and `State.create` re-runs transforms from a
pristine list, so the mechanism is a PULL on config reload (a push would serve a
stale list after a reconcile). Because `Payloads.command` feeds both `GET
/command` and the `<available_skills>` system-prompt block, the transform changes
what the model reads. Applied at boot as well as on reload. A throwing transform
is reported and skipped; an empty result is rejected.

Still deliberately unbridged, with the reason:

- `agent`/`model`/`provider` — registry-level. `build_agents` output feeds four
  consumers (the v1 route, `/api/agent`, per-agent system prompts, and
  `agent_rules`), and models/providers feed the `LlmRegistry`; editing only the
  served list would be shape-without-mutation (the A1 rule).
- `reference`/`worktree`/`websearch`, `integration` (OAuth), `mcp`, `vcs`
  actions, `aisdk`, `experimental`, `session.hook(lifecycle)`,
  `shell.create.before`, `tool.transform` — named divergences.
- disk-backed `ctx.storage` (`D-V2-STORAGE-MEM`) — needs a persistent sidecar
  data dir first; the host dir is throwaway today.
- `rpc.events.emit` (`D-V2-RPC-EVENTS`) — needs an RPC event SSE surface that
  does not exist.

## Tier 3 — hygiene — COMPLETE

- Fixed the stale `D-PLUGIN-INVENTORY` row (it was closed but still listed).
- Extended rule 31 to cross-check `D-*` CLOSED-vs-current across **all** docs;
  it immediately found a second stale claim (`/api/rpc/*` documented as a 404
  when it returns a byte-exact 400).
- Fixed three "executing" status banners and added the missing plan/report
  files to the AGENTS doc-map.

## Then — upgrade-machinery exercise — COMPLETE

- Exercised `bump.sh` against the real latest releases (v2 `2.0.32`, v1
  `1.19.3`) and verified the shipped binary against both (`K-BUMP`,
  `bench/BUMP-MACHINERY-TEST.md`). Pins stayed frozen: this tested the
  machinery, it did not adopt the upgrades.

## Guardrails (every tier)

Generated guards, never hand lists: hook-surface (rule 36), contract-differ (32),
coverage (16/16b), request-keys (20). Planted negative control per fix. Full gates:
fmt, clippy -D, workspace tests, guards, matrix, replay 26/0, pair-v2, capdiff,
spec-validate (both arms), permission-check, event-validate, field-contract,
sidefx; **G2 re-run on `.227`** for any perf-touching change. TRACEABILITY K-rows.
