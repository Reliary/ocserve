# V2-PLAN.md — full `opencode2` backend (`feat/v2-engine`)

Status (2026-10-13): plan committed; P0 in progress. Reference pin: `@opencode/cli@2.0.28`
+ contract regenerated from `@opencode/protocol@2.0.28` sources (never the stale checked-in file).

Progress 2026-10-16: P0–P5 built and green (see TRACEABILITY K-V2-P0…K-V2-P5).
P6 perf note: union metadata routes 0.16–0.42 ms, admit 0.51 ms, inbox list
0.26 ms, content[] page 0.64 ms (release binary, 11-run trimmed means —
indicative, not a G2 gate). `/api/info` serves only when `OCSERVE_V2=1`
(dark JSON 404 otherwise).

This document is specification for the v2 build. Precedence: TESTING.md gates >
PLAN.md > this document for v2-domain decisions. Resolutions that change a
requirement land here as edits, never silently in code.

## 0. Locked decisions (user-approved, 2026-10-13)

- **D1 DB sharing**: one database, new additive tables. No renames, no in-place
  migration of live rows, no second DB file.
- **D2 `/api/info` gate**: dark until the acceptance bar passes. Serving it early
  is a guard failure (rule: `/api/info` route must not exist before P5 green).
- **D3 Reference pin**: `@opencode/cli@2.0.28` + regenerated contract. The
  `~/src/opencode-v2` worktree (`afc5d1e21`) is read-only reference, never modified.
- **D4 Migration endpoint**: adopt upstream's one-shot `migration.v1.status`
  semantics (project, don't convert).
- **D5 Perf**: one event write per turn (projection, not a second event table).
- **D6 Guard rule 33**: allocation budgets on all v2-path queries are a hard gate.
- **D7 Search**: v2 search exposure parked as post-parity differentiator.

## 1. Evidence base (all verified read-only, 2026-10-13)

- **Engine**: admission → wake → drain → promote → step, server-owned. `POST prompt`
  only admits (`session.ts:146-179`); `Execution.Started` carries the write-ahead
  claim in-txn (`execution.ts:76-86`); deliver deletes inbox row + inserts message
  atomically (`projector.ts:609-640`); ordering key `enqueued_seq` = durable seq;
  `admittedSeq` is dead (stripped by hygiene test). Claims = restart markers, 10
  resume attempts max. Instructions = content-addressed `{key:hash}` deltas, epoch
  advance on compaction, reset on revert-commit. Reconnect = `log?after&follow`
  with single `log.synced` watermark. Event grammar has no `message.part.*`.
- **Storage**: SQLite, `session_v2` / `session_message` (no part table) /
  `session_inbox` (pending-only) / `instruction_*` / `event` + `event_sequence`.
  Compaction = append-marker; **revert = hard delete** (conflicts with our
  mark-only: named divergence `D-REVERT-NODELETE`). Upstream migration is
  one-shot + resumable + row-transforming, and deletes the event table.
- **Surface**: 140 declared endpoints / 136 vendored (4-op drift = proof regen
  matters). 31 `_tag` errors. ~45 session event types + ~40 domain types,
  durable/ephemeral split. PTY grammar confirmed (ticket scope, 64 KiB replay,
  `0x00+JSON` meta frame).
- **Reuse**: prompt executor seam is normalizer-only (`v2.rs:1161-1213` proves it).
  Free: splice, wire memos, FTS, snapshot engine, PTY, MCP hub, guards
  16/18/19/20/21/27/29/30, pair/capdiff/replay. Absent: inbox, claims,
  instructions, durable-seq, form. Two defects to fix en route: dual
  `emit_durable` chokes; stale `V2PromptInput::WIRE_KEYS`.
- **Perf asymmetries (verified)**: zero FTS in v2 core+server; `persist` defaults
  false (durable table empty unless opted in); history loads all rows since latest
  compaction with per-row `Schema.decode`; Effect/Drizzle per-request decode tax.
  Ours: trigram FTS 0.2 ms warm, always-on log, splice zero-parse, prepared cache,
  wire memos, 13.8–18.3% fewer provider bytes (capdiff).

## 2. Phases

### P0 — baseline (exit: branch + pinned + vendored + PLAN gate fixed)

- Branch `feat/v2-engine` off `main`.
- Vendor regenerated 2.0.28 contract as second embedded asset
  (`crates/ocserve-core/assets/openapi/2.0.28.json`) via `scripts/extract-spec.sh`
  (new target; regen in scratch from npm tarballs, never the worktree).
- Fix stale `PLAN.md:206-230` (v2 "pre-release/gated" is false; `latest=2.0.28`).
- Kill: vendored asset byte-equals fresh regen; `git status` clean in worktree.

### P1 — contract-differ (exit: machine gap table + rule 32)

- `scripts/contract-diff.py`: every op classified `identical / additive-superset /
  renamed-ref / ocserve-stub / new-v2 / v1-only / removed`; emits the committed
  table P2–P4 burn down. Includes `stale-asset` detector (regen ≠ vendored → red).
- Guard rule 32 (extends `check-guards.sh`): differ runs in CI; planted new-op
  control. Must reproduce hand counts (140 declared / 136 vendored / 31 differing
  shared / 7 differing write bodies) or it is wrong.
- Research anchor: Lercher et al. arXiv:2311.08175 (automate change-impact).

### P2 — schema v13 (exit: additive tables + one emission choke)

- New tables only: `session_inbox` (pending-only), `instruction_state/blob/entry`.
  No `event_v2` table (D5): `type@vN` suffix discipline + stable `evt_` ids at the
  single unified emission choke (merges the two `emit_durable` sites — load-bearing
  cleanup). `session_v2`-style renames forbidden; hot-path tables untouched.
- Fix stale `V2PromptInput::WIRE_KEYS` (live body is flat `{id?,text,...}`).
- Tests: inbox CRUD + idempotent reconcile + cancel/update guards; instruction
  epoch/reset; versioned-type round-trip. Negative controls per fix.
- Kill: G2 numbers unmoved (inbox/event writes measured in isolation).

### P3 — engine (exit: admit→wake→drain→promote→step live)

- `inbox` admit/deliver/promote/cancel/update (steer-first, compaction priority,
  never-cross-move); `run-coordinator` (idle→fiber, active→doorbell);
  `execution` (started/succeeded/failed/interrupted + claim in-txn);
  boot recovery (synthetic notice + 10-attempt budget + terminal-fail).
- `run_prompt` stays the single executor; v2 path = admit→wake→drain reusing the
  turn body. Runner calls the budgeted history builder (8 MB tail, splice), never
  a decoded-row loop. Instruction `observe/commit/apply/advanceEpoch/reset` wired;
  rendering feeds `estimate_request_bytes`; epoch memo designed, built on
  measurement only.
- Named divergences: `D-REVERT-NODELETE` (mark-only kept), compaction anchors kept.
- Kill switches: `OCSERVE_V2=1` gates runtime. Tests: E2E ordering, cancel/update,
  interrupt/resume, kill -9 mid-turn recovery, planted controls.

### P4 — wire (exit: union routes + supersets + dual events + real content[])

- New paths: `/api/info` (D2-gated), `/api/form*`, `/api/rpc`, `/api/debug`,
  `/api/experimental/{generate,session/log,instructions/*,migration/v1,
  persistent-pty/*,worktree,fs/write,integration-wellknown/oauth-status}`.
- Shared ops emit additive supersets; requests decode by key presence; all 31
  `_tag` errors mapped through the tagged machinery.
- Dual-emit at the single choke: v1 grammar on `/event`+`/global/event`, v2
  grammar on `/api/event`; `log?after&follow` + `log.synced` exact.
- Fill skeletal projections (`"content": []`, title-as-text) with real `content[]`.
- Kill: contract-differ rows for the phase green; pair vectors for the phase green.

### P5 — oracles (exit: proven against real 2.0.28 binary)

- `pair-check` + `capdiff` vs real `@opencode/cli@2.0.28` (npm-published, no
  source build): admission idempotency, steer/queue ordering, cancel/update,
  interrupt/resume, log replay + watermark, form lifecycle, instruction epoch,
  revert markers, fork prefix, `_tag` errors.
- `spec-validate.py` extended to the v2 contract (same `relax_null` calibration).
- capdiff **bytes axis**: v2 turn must not exceed v1 turn provider bytes.
- **Acceptance bar**: a v2 client can probe, browse, prompt, stream a reply, full
  round trip differential-green. Only then is `/api/info` served.

### P6 — perf/hardening/release (exit: shippable)

- G2 re-run (v1 unmoved); v2 admission in load mix behind flag; soak with
  kill -9 recovery; WAL ceiling holds; new-table queries under rule 33 budgets.
- Docs: README corrected, SECURITY asymmetries, TRACEABILITY rows, this doc
  status → shipped. Version bump only on green.

## 3. Antagonism ledger

| # | Attack | Resolution |
|---|---|---|
| A1 | No discriminator → one `/api/*` can't branch by client | Branch by request keys; superset responses both decoders tolerate |
| A2 | Dual-emit doubles ring pressure | Shared `Arc<str>` frames; deltas live-only; ring budget re-measured |
| A3 | Inbox vs 409-busy semantics | Busy gate stays for floods; inbox is ordering — both, tested together |
| A4 | Boot recovery vs soak restarts | Planted kill-mid-turn test: notice + resume + budget cap |
| A5 | Revert hard-delete vs mark-only | `D-REVERT-NODELETE`; differential-tested behavior |
| A6 | Differ passes vacuously | Must reproduce hand counts; planted new-op control |
| A7 | Weekly v2 drift re-opens everything | Nightly v2 drift column; bump = vendor + diff + gates |
| A8 | `/api/info` advertises a lie | Dark until acceptance; early serve = guard failure |
| A9 | Perf reads as "v2 tax on v1" | G2 re-run gates every phase; isolated measurement |
| C1 | Two event writes per turn | Projection, not second table (D5): one write, two grammars |
| C2 | Inbox table growth | Pending-only by construction; no prune loop needed |
| C3 | Naive history port | Runner calls budgeted builder; named integration point |
| C4 | Instruction render cost | Feeds byte budget; epoch memo gated on measurement |

## 4. Upstream advantages we keep (must not regress)

Splice zero-parse, prepared-statement cache, wire memos, trigram FTS (v2-exclusive
capability), always-on event log, `canonical_tool_args` + compact serialize on the
v2 turn (capdiff bytes axis), single Rust core (no GC on turn loop), byte-bounded
ring.

## 5. Guards added by this build

- Rule 32: contract-differ in CI + planted new-op control + stale-asset detector.
- Rule 33 (hard gate, D6): allocation-budget test per new-table v2 query +
  `SELECT *` ban extension; planted control per table.
- `/api/info`-dark rule: route must not exist before P5 green (planted control).
