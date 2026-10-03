# DIFFERENTIATION.md — refine vs upstream opencode

Standing contract: **what we build differently, why the evidence says so, and what we
refuse to build.** Precedence: TESTING gates > PLAN > this doc > code. A claim in this
doc without a citation (arXiv ID, file:line, bench name) is aspirational and must not be
repeated in README/user-facing surfaces.

## 1. The thesis

**The Scaffold Effect (arXiv:2607.22585)**: same models across Goose/OpenCode/OpenHands —
harness choice moves **tokens-per-solved-task up to 40×** while pass rates move only
0–8pp. Failure fingerprints are harness-level, model-independent; **OpenCode's published
fingerprint is `idle-loop / TIME`** (no-action turns = wait tax).

Corollary: we differentiate on **cost, latency, oversight quality, and measurement
credibility — never on model accuracy**. That is the axis our parity numbers already win
(boot 4.66s→0.59s, anon peak 724MB→4.4MB, reads p95 8–15ms→0–2ms; bench `bench/parity`).

Supporting: *Inside the Scaffold* (2604.03515 — navigation dominates agent activity,
failure trajectories 12–82% longer); *On Randomness in Agentic Evals* (2602.07150 — 60k
trajectories, single-run pass@1 varies 2.2–6.0pp, sd>1.5pp even at temp 0);
*Deterministic Replay for AI Agent Systems* (2607.16200 — replay ≠ observability).

## 2. Candidates and status

| # | Candidate | Evidence | Status |
|---|---|---|---|
| D1 | Loop intelligence (repeat/oscillation/spiral/error-storm) | upstream `DOOM_LOOP_THRESHOLD=3` → `permission.ask` (processor.ts:29,356-383); IALs 2607.01641; degradation up to 30% of hard-task steps (2604.13759); detectors without a second LLM work (2608.02464) | **build (P1b)** — parity class = same permission name `doom_loop`; extensions distinguished by additive `metadata.class` only |
| D2 | Verification grounding (file-delta/test-status as progress oracle) | progress mirage 2607.25152 (56% of self-claimed wins ≤0 real delta); Aria 2607.06341 | parked — evidence-gated |
| D3 | MCP trust layer (static description scan, TOFU pinning, drift alerts, observe-only runtime flags) | OWASP MCP03; 2601.17549; 2510.16558 (67,057 servers, DSN'26); Microsoft disclosure 2026-06-30 | **build (P2)** |
| D4 | Replay-as-CI | 2602.07150 + our own interleaved-bench rule | **build (P1a)** |
| D5 | Server-side memory | Mem0/A-MEM/MemoryAgentBench; precision critique 2605.11325 | **excluded** — magic-context plugin owns injection on the live config; revisit only if drift watch flags a v1 drop |
| D6 | Index-backed tools | MCP parity — works on upstream too | **excluded** — user's reliary8 MCP is the indexing solution; refine's MCP hub reaches it on both sides |
| D7 | Trace-mining → guard candidates | Self-Harness 2606.09498; we already do this manually (TESTING §1.6) | process, not code |

## 3. Non-goals (graveyard — do not re-propose)

1. **Tool forcing / tool-list surgery** — measured +69–113% cost (user bench).
2. **Read augmentation / eager preloading** — 170–600% input overhead, zero accuracy gain; Goose's eager tree is the same mistake (2607.22585 scores it).
3. **Message/history rewriting** — corrupts provider format (SYNTHESIS What-To-Kill #3; relay bench Phase 5: 3× more tool calls).
4. **Request-path compression of assistant text** — KV-cache bust every request; deleted in our own `PLAN_agent_integration.md` Phase C.
5. **Second-LLM judge/monitor** — single-LLM constraint; 10–15% overhead (2604.13759) and judges don't fix grounding (2607.25152).
6. **Format coercion / schema gating** — SYNTHESIS What-To-Kill #1/#2; 49-token tool JSON isn't worth cache-invalidating system prompts.
7. **Chasing v2 plugins/routes** — §8 bridge plan; drift watch flags the trigger.
8. **Vector-DB memory v1** — FTS-first (trigram), precision over dumping (2605.11325).
9. **reasoning/history IR compression in refine** — moot: `to_provider_messages` re-sends only `type=="text"` (prompt.rs:152-158); stored reasoning never reaches the provider.

## 4. Local primitives rolled in

**sift (Tier-1, ~2.2K, grammar-free output compression) — YES, at the provider boundary only.**

Placement law (from our own `PLAN_agent_integration.md`, cache audit 2026-10):
- Tool output is **generated once** → compressing it before it first enters the provider
  conversation is cache-SAFE (subsequent turns resend the identical compressed bytes).
- Compressing already-sent assistant text on the request path busts KV every request — banned (§3.4).

Roll-in rules (P1c):
- **Persist raw, compress at provider build** (`to_provider_messages` tool-result site) —
  client/oc-remote always sees upstream-faithful raw output.
- bash/shell/command tool outputs only; **never** read/grep (model must see file bytes it
  will edit); MCP tool outputs = parked flag.
- Enabled via env `REFINE_SIFT` (`auto` = `reliary sift --stdin` with
  `~/.local/bin` prepended, or an explicit binary path); default **OFF** until the
  Arc49-style gate passes (interleaved, score median non-regression, WC ≥15%).
- Bench precondition (parked, bench repo): llm-replay records per condition and must
  fail loud on a messages-hash miss — a silent miss hits the live provider and
  corrupts the comparison.
- Hard guards: output ≤ raw AND output non-empty when raw non-empty (observed:
  `compress_unified` can return `""` on degenerate input — empty guard is load-bearing),
  subprocess timeout → raw, deterministic memoized per output hash.
- Model-facing `[compressed N→M bytes]` marker; divergence row in PLAN §17.

Rejected local primitives (evidence): reasoning-compress (§3.9), conversation/context
rewrites (SYNTHESIS #3), quali (Python seam, SYNTHESIS Seam 2, no loop bench),
stria/relay guard+risk+dead via MCP (D6 exclusion), harness contract/ellipsis formats
(format coercion, SYNTHESIS #1), B-cell memory (D5 exclusion), gate.js turn-counting
(refine owns the loop).

## 5. Phasing (approved order)

`P0` this doc → `P1a` `scripts/replay-check.sh` (boot → wire-corpus replay → exit code;
self-test proves both exits) → `P1b` loop intelligence → `P1c` sift-at-boundary
(default-off; flip only on Arc49 gate) → `P2` MCP trust layer.

Standing gates: every code commit = fmt + clippy -D + full tests + guards + matrix;
negative control per new guard; no perf/accuracy claim without an interleaved bench
(2.7× variance rule); new config keys typed in `refine.toml` (unknown-key = fatal).
