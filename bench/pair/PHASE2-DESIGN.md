# Phase 2 (PRE-REGISTERED — not built): stub-driven prompt flows for pair-check

Status: DESIGN ONLY (2026-10-06). Phase 1 (`scripts/pair-check.sh` + the
`replay --pair` engine) is the live GET-route freeze↔refine differential.
This file pre-registers Phase 2 before any of it is built, per TESTING §2.

## Why

Phase 1's first run found three D-PAIR rows that are **state-shaped**, not
wire-shaped: `session_list` (freeze omits agent/model/summary until a first
prompt), `project`/`project_current` (project-state rows vs refine's
synthetic rows), and partly `provider` (registry provenance). All three
would likely converge if both arms could run an **identical real prompt**
against a **deterministic** backend — which the parity harness already has:
the stub provider (`Dockerfile.stub`: completions derive from the last user
message only, paced token streaming).

## Design

1. **Stub on the host**: run the parity stub container publishing a host
   port; fixture `opencode.json` copies for the pair get
   `baseURL: http://127.0.0.1:8194/v1` (both arms, identical files).
2. **Seed prompt**: after the existing identical `POST /session`, POST an
   identical short prompt (`POST /session/{id}/prompt` or the freeze route
   pair-check chooses) on both arms. Deterministic stub ⇒ same completion
   text, same tool-free flow, same persisted parts shape.
3. **New manifest entry kinds** (requires a record mode — `replay.rs` is
   compare-only today): `prompt_flow` entries asserting STRUCTURED outcomes
   across freeze and refine: final assistant text equality (stub-derived ⇒
   byte-comparable), part count/type sequence, finish reason, usage shape
   (pacing/latency explicitly NOT compared).
4. **Re-evaluate D-PAIR-3** (session_list) after the seed prompt: if rows
   now carry agent/model on both sides, REMOVE the allowlist entry — that
   removal is the phase's acceptance, not a new allowance.

## Kill criteria (pre-registered)

- **K2a**: the stub cannot serve both arms deterministically (stream/text
  mismatch >30% of prompt-flow routes with equal seeds) → abort phase;
  structured comparison is unattainable, document and stop.
- **K2b**: freeze persists prompt-derived state in files the fixture HOME
  cannot reproduce (project-state rows still divergent after identical
  prompts) → D-PAIR-1 stays allowlisted permanently; document as
  environmental and stop — no custom state emulators.
- **K2c**: freeze's prompt route requires capabilities refine hasn't
  ported (route 404 / schema mismatch) → scope Phase 2 to the routes
  both serve; the rest reverts to the recorded-corpus deferral ledger.

## Explicit non-goals

No multi-turn flows, no tool-loop comparison (agent-loop coverage stays
owned by the calculator/pipeline benchmarks), no resource assertions
(parity harness owns those), no scheduling change (pair-check stays
MANUAL — Phase 2 does not revisit that decision without a new call).
