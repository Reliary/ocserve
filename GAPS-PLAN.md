# Gap-closure plan: absolute compatibility (v1 + v2), low maintenance, high performance

Status: **executing**. Scope agreed 2026-10-17 (all tiers; foreground-only `task`;
v2 acceptance pinned to the vendored **2.0.28**; service-bound divergences kept).

## Findings this plan closes

- **v1: 188 implemented / 0 cited / 0 gaps**; v2: **142 / 0 / 0** (both spec-driven,
  guard rules 16/16b).
- **The one genuine functional hole: `task`.** The frozen `provider-tools.json`
  advertises `task`; `ocserve-tools::IMPLEMENTED` excludes it; the default system
  prompt tells the model to use Task → a dead tool step. `D-TOOLS-TASK`.
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

## Tier 2 — v2 behaviour depth (mutating domains only)

- bridge transform domains `agent`/`model`/`provider`/`command`/`skill`/`reference`/
  `worktree`/`websearch` over the derived payloads, applied at reload, with `reload()`.
- disk-backed `ctx.storage` (drops `D-V2-STORAGE-MEM`); `rpc.events.emit` (drops
  `D-V2-RPC-EVENTS`).
- keep named divergences: `integration` (OAuth), `mcp`, `vcs` actions, `aisdk`,
  `experimental`, `session.hook(lifecycle)`, `shell.create.before`, `tool.transform`.

## Tier 3 — hygiene

- fix stale `D-PLUGIN-INVENTORY` row (closed, still listed).
- extend rule 31: cross-check `D-*` CLOSED-vs-current across **all** docs.

## Then — upgrade-machinery exercise

- run `bump.sh v2 <npm-latest>` (2.0.32 at time of writing) against the current
  shipped binary: the mechanical pipeline's first real use.

## Guardrails (every tier)

Generated guards, never hand lists: hook-surface (rule 36), contract-differ (32),
coverage (16/16b), request-keys (20). Planted negative control per fix. Full gates:
fmt, clippy -D, workspace tests, guards, matrix, replay 26/0, pair-v2, capdiff,
spec-validate (both arms), permission-check, event-validate, field-contract,
sidefx; **G2 re-run on `.227`** for any perf-touching change. TRACEABILITY K-rows.
