# V2-PARITY-PLAN.md — full v2 feature parity on `feat/v2-engine`

Status: plan committed 2026-10-16; executing Phases A→F. Companion to
`V2-PLAN.md` (engine P0–P6, all green). Precedence: TESTING.md gates >
PLAN.md > this document for parity-domain decisions.

Locked decisions (user-approved 2026-10-16):
- **Rust-native serving path**: no new processes/runtimes/interpreters in the
  request path; sidecar frozen. Dev-time JS only where the artifact IS
  upstream JS (contract regen). Enforced by guard rule 34.
- **rpc/websearch/credential-backend**: honest tagged 404 + named divergence
  (never shape-only stubs — the compatibility-trap class).
- **Monthly pin, nightly watch**: drift-watch accumulates; adoption batched.
- **Long lane**: `feat/v2-engine` to parity exit, `--no-ff` to main.
- **Web pack**: extract from the binary's HTTP surface (proven pipeline).

## 0. Baseline (measured 2026-10-16)

- Differ: v1 188 ops / v2 142 ops → 13 identical / 1 superset / 30 renamed-ref
  / 2 stubs phased / **96 new-v2** / 139 v1-only / 3 removed / 0 conflict.
- Built (P3–P5): prompt admission+drain, inbox×3, form×6, info, log = 11 ops.
- Remaining: **85 ops, all Advanced** (basic connect/list/open/send/stream is
  bound in both contracts). Reference pin `@opencode/cli@2.0.28`; drift noted:
  `@opencode/cli@2.0.31`, `opencode-ai@1.19.3` exist (monthly pin holds).
- Dual-serve is safe: v2 TUI/app probe only `/api/info`; v1 TUI probes
  nothing; v1 app probes `/api/health`→`/global/health` (takes v1 path).
  No client probes both version signals.

## 1. Antagonized research base (verified, no memory-citations)

- **Fowler, Strangler Fig (2024)** (fetched): gradual replacement via seams;
  transitional architecture is expected cost. Our v2 block is the fig vine
  (additive routes behind `v2_enabled`); credential is the one subsystem-cut
  exception (no backend — route-by-route would lie).
- **Lercher et al., arXiv:2311.08175**: practitioners' core problems are
  change-impact analysis + consumer lock-in; automate impact analysis. =
  the contract-differ + bump pipeline.
- **Roseau, arXiv:2507.17369 (ICSME'25)** (abstract fetched): source-based API
  models vs versions, F1=0.99, <2s on hundred-kLOC. Adopted: key on stable
  identity (METHOD+path — our `operationId` stability measures zero) and keep
  the comparison per-commit fast.
- **Overeem et al., JSS 178 (2021), arXiv:2104.01146** (abstract fetched):
  five event-evolution tactics — versioned events, weak schema, upcasting,
  in-place, copy-and-transform. Ours: `type@vN`, `onExcessProperty:ignore`,
  `load_session_events`/`project_page_v2` re-projection, D-V2-UNION. Warning
  heeded: rebuilding projections is a first-class cost (epoch memos stand).
- **Consumer-driven contracts** (Pact practice, no paper — stated as such):
  pair-v2 vectors must be *captured* from real TUI boot order, not invented.
  Until the capture harness exists, pair/connect + integration stay highest
  skew risk.

## 2. Killed claims (do not reintroduce)

- "11.6× CPU-per-req": fabricated; cite only splice A/B, session_wire 8→1µs,
  Phase-V CPU/req −14.4%, 20.3× closed rps (queueing-inflated, never gated).
- "Bun GC on the turn loop": architecture differs; zero GC measurement. Claim
  structure only until a GC-trace experiment.
- "SQLite tuning they lack": we disable mmap; FTS-optimize left boot. True
  remainder: journal_size_limit, wal_autocheckpoint, auto_vacuum+page-size,
  prepared-cache 32, optimize-after-DDL with retry_busy.
- "Unbounded SSE queues": stale for v2 (theirs is 4096-count-bounded with
  overflow error). Surviving asymmetry: shared byte-bounded ring (one 32MB
  copy) vs per-subscriber count-only queues (4096 × frame × subscribers).
- "13.8–18.3% fewer provider bytes" is v1-freeze-scoped; v2 turns with
  instruction rendering unmeasured (capdiff bytes axis pending).
- Union serving: "does not do", never "would never do".

## 3. Phases

### Phase A — TUI boot closure (exit: real v2 TUI reaches session list)
/api/project (+PATCH), `/api/mcp` + resource catalog, `/api/vcs*` aliases,
`/api/session/active` check, `file.list` vs `/api/fs/*` resolution. Shell
registry verdict 2026-10-16: FAIL-OPEN by code reading (per-resource
`locationResource` syncs with independent keys; bootstrap collects errors
via `runAll` + toast and continues — `bootstrap.ts:122-175`,
`data.ts:146-200,1301-1330`). Shell registry deferred to Phase D; the
real-TUI-boot kill still arbitrates. Each route: bind → pair-v2 vector →
spec-validate. Technique: strangler route-by-route.

### Phase B — session write/read adapters (exit: vectors green)
`diff` (envelope), `fork` (`before` brand adapter), `background` (honest
204 + D-BACKGROUND-NOOP), `wait` (`/experimental/` alias), `mcp`/`project`/
`vcs` (Phase A). SCOPE DECISION 2026-10-16: `command|shell|move|synthetic|
view|environment|generate` are NOT aliases — their bodies differ from v1
(admit-typed-inbox vs sync-turn), so they move to Phase D as typed inbox
execution under branch-by-abstraction. An alias that hides a semantic
difference is the compatibility-trap class.

### Phase C — S-family, ~25 ops (exit: spec-validate v2 stays 0)
worktree, debug, metadata, credential stubs (cut, not fake), plugin list,
fs/write, migration-status, session remove/update/diff, model/default. Pure
Rust projections, wire-cached where hot (rule 23 pattern extends).

### Phase D — M-family, ~35 ops (exit: per-family pair vectors + G2 re-run)
Shell registry (byte cap + 512-evict), instruction entries, import/export/
**columnar stats** (prepared SUM/COUNT — the projectional synergy; never
JSON-shredding), generate (`RunOpts` preset), integration-connect (FormGate
template), MCP experimental, pairing, persistent-pty. State-machine tests +
negative controls + rule-33 budgets each.

### Phase E — web pack (exit: v2 shell renders)
Extract pinned bundle from binary HTTP surface (`extract-app.sh` +2 lines),
embed second pack, `OCSERVE_UI_PACK` flag, per-tag skew guard.

### Phase F — verdicts (exit: D-rows machine-checked)
rpc, websearch, credential-backend: tagged 404 + named divergence with build
triggers. Revisit only on observed client demand.

### Phase G — gap closure (exit: 142/142 bound, zero named gaps; landed 2026-10-17)
All five Phase-F verdicts closed against live 2.0.28 probes (K-V2-GAPS):
experimental generate via `RunOpts.transient` (zero session-side effects);
credential store (single-active-per-integration, 600 DB mode); session env
PUT + subprocess injection (provider-clean by construction); RPC byte-exact
400 (no enabling state exists); websearch list + keyed Tavily/Exa direct
(D-WEBSEARCH-PARTIAL for the rest). Techniques: live-oracle-first (every
behavior probed against the real binary before building), branch-by-
abstraction (one executor, one tools fn, additive tables), exact-error
parity where no success state is constructible.

## 4. Maintenance machine

`bump.sh <line> <version>`: acquire → re-vendor (6 artifacts) → contract-diff
(STUBS need phases; conflicts block) → regen vectors → oracle battery (pair,
pair-v2, replay, field-contract, permission, capdiff both directions) →
spec-validate both arms (freeze-0-fairness) → event/tui checks →
guards/matrix/package/publish → TRACEABILITY K-row + dark-gate re-proof.
Automatable (planted controls): version sweep, stale-asset red, listings,
scans, effects, skew, containment, doc paths/rules, opId-rename map,
scenario validation, corpus-diff summary, pair-v2 auth harness. Judgment
(human names, machine enforces): D-rows, union-vs-reject, allowlists,
calibrations, member-count growth, inflation tolerance, 20% probe kill,
share/OAuth non-goals, `/api/info` release call.

## 5. Detection (oracles per layer)

Contract: differ (32) + coverage (16) + request-keys (20, dual-spec). Shape:
spec-validate v1+v2 (freeze-fairness first) + event-validate (19). Behavior:
pair (12) + pair-v2 + field-contract + permission-replay (18) + capdiff
bytes axis (27) + sidefx (29). Perf: CPU/req table, budgets (33), WAL
ceiling (26), thresholds. Freshness: drift-watch v1 + v2 column (new).
Purity: rule 34 (no non-Rust runtime in the request path).

## 6. Kill criteria

K1 hand-count reproduction · K2 conflict-blocks-superset · K3
freeze-nonzero-stops · K4 20%-probe-escalation · K5 inflation-blocks ·
K6 G2-regression-blocks · **K7 serving-path purity (rule 34)**.

## 7. Synergies pressed (confirmed-only + columnar stats)

Trigram FTS over parts (exclusive) · always-on log (reconnect-by-default;
measure per-turn cost in G2) · splice/memos/prepared-cache on their decode
hot path (no ratios) · single-copy ring (per-subscriber MB math) · union
serving (one port, cited) · **columnar stats aggregation** (new: their
stats endpoint shreds JSON per request; ours is indexed SUM/COUNT).

## 8. Named divergences added in parity phases

New write paths sub-ms (inbox_list 0.25ms, stats 0.50ms for 3 SUM/COUNTs —
the columnar-synergy claim measured, shell_list 0.24ms, admit-404 0.23ms);
v1 session_list 0.69ms (no regression from v2 additions); verdict routes
≤0.5ms. Full k6/GATED on .227 stays the release gate (new write paths are
admit-only; the turn executor is unchanged). Binary 33,013,072 B ≤ 34M.

- D-BACKGROUND-NOOP: background is an honest recorded 204 (no
  foreground/background tool distinction in the inline loop).
- D-VIEW-NOWATERMARK: view records + 204s (no read watermark tracked).
- D-PLUGIN-INVENTORY: plugin list honestly empty (no inventory RPC yet).
- D-ENV: CLOSED — `PUT /api/session/{id}/environment` bound (replace
  semantics); `session_environment` table; read once per turn, injected
  into bash subprocess env; never sent to the provider (e2e asserts tool
  result carries the value + first provider body lacks it).
  `PUT /api/session/{sessionID}/environment` — same verdict, exact path.
- D-RPC: CLOSED as exact-error parity — no constructible state enables RPC
  (fake + real builtin ids all → `rpc.unavailable`, verified live);
  byte-exact 400 + type field; missing/invalid body → exact Payload error.
  Routes to the sidecar the day a plugin registers RPC methods.
  `POST /api/rpc/{rpcID}/{method}` — same handler (exact path), same verdict.
- D-WEBSEARCH: CLOSED — provider list exact (verified bytes); validation
  byte-exact (required/not-found); active credential key → direct backend
  (Tavily/Exa); no key → `{results: []}` (observed keyless behavior);
  backend failure → exact ServiceUnavailable envelope. Firecrawl/Parallel/
  TinyFish → same failure envelope (D-WEBSEARCH-PARTIAL — the envelope
  upstream emits when ITS backend fails, never fabricated results).
  `GET /api/websearch/provider` — same verdict (exact list).
- D-CREDENTIAL: CLOSED — `credential` table (upstream shape); CRUD with
  single-active-per-integration (create default + activate, verified live);
  unknown activate = 204 no-op; label defaults "default"; value union
  validated with exact Payload wording; values echo on GET (upstream
  parity); DB file 600 (pragma::open_writer restricts every open).
- D-GENERATE-EPHEMERAL: CLOSED — `RunOpts.transient` threads the shared
  executor (no session bail, no persist, no session-scoped emissions,
  no compaction anchor, unsolicited tool_calls dropped); experimental
  generate returns `{data:{text}}` with zero session/message/event rows
  (verified live: upstream creates nothing either). Also fixed the same
  latent empty-text bug in session generate (info has no text field —
  extract from parts).

## 9. Phase D exit: G2-targeted perf (local single-VU, release binary)

New write paths sub-ms (inbox_list 0.25ms, stats 0.50ms for 3 SUM/COUNTs —
the columnar-synergy claim measured, shell_list 0.24ms, admit-404 0.23ms);
v1 session_list 0.69ms (no regression from v2 additions); verdict routes
≤0.5ms. Full k6/GATED on .227 stays the release gate (new write paths are
admit-only; the turn executor is unchanged). Binary 33,013,072 B ≤ 34M.
