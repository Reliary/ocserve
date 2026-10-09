# Spec-driven API coverage — plan

Goal: full v1+v2 wire coverage (TUI, web UI, oc-remote) with minimal ongoing
maintenance, by deriving the coverage contract from the frozen OpenAPI spec
instead of hand-maintained route lists.

## Ground truth

- Frozen 1.18.31 ships a complete OpenAPI 3.1.0 document: `GET /doc`, 162 paths,
  188 operations, 472 schemas, covering v1 paths + `/api/*` v2 + `/sync/*` +
  `/tui/*` in one tagged document.
- Tag-extracted spec == live `GET /doc` (byte-equal, verified 2026-10-08).
- ocserve today: 94 `.route(` entries; the v2 `/api/*` group is unbound and
  falls through the UI catch-all to SPA HTML (same silent-failure class as the
  `/pty/shells` crash).

## Phases

### P0 — Contract + self-description
- Vendor `bench/openapi/1.18.31.json` (tag-extracted, byte-equal to live /doc).
- `GET /doc` serves it (freeze parity; also the runtime oracle for P4).
- `scripts/extract-spec.sh <tag>` regenerates on version bump.

### P1 — One generated coverage guard
- Generator cross-references spec ops against the router →
  `bench/openapi/coverage.md` (implemented / gap / cited-out).
- One rule replaces rules 14+15+bundle extractor: every spec op is bound or
  carries an exact-URL PLAN §17 citation. Derived → cannot drift.

### P2 — Catch-all
- No divergence. The SDK sends `Accept: */*`; freeze serves HTML blindly.
  Parity preserved.

### P3 — Implementation tiers
- A (real crashes): `GET /session/{id}/diff`, `GET /api/fs/find`.
  - `session_diff` HTML string → `SidebarFiles` `.flatMap` TypeError.
- B (TUI data layer, ~20): `/api/{agent,model,provider,provider/{id},command,
  skill,reference,location,integration,health}`, `/api/session*`,
  `/api/permission/{saved,request}`, `/api/question/request`, `/api/fs/{list,read}`.
  - Shared prompt executor: v2 `/api/session/{id}/prompt` normalizes
    `{prompt:{text,files,agents},delivery,resume}` → v1 payload → calls the same
    `build_prompt_context` + `ocserve_core::prompt::run_prompt`. No loop
    duplication.
- C (experimental stubs): workspace/console/control-plane/projectCopy set,
  matching freeze error envelopes.
- D (cited outs): `/sync/*`, worktree, credential, integration connect/attempt,
  revert stage/clear/commit, `/global/upgrade`, `/project/git/init`, `/vcs/apply`.

### P4 — Calibrated spec validation
- `jsonschema` dev-dependency (MSRV 1.85 ≤ 1.98).
- Validate every 200 JSON response against its declared schema.
- Calibration: freeze self-violates its own spec on 3 routes (`/vcs`, `/command`,
  `/agent` — null where schema says string) → explicit nullability relaxation +
  documented allowlist. Gate must be green on freeze.

### P5 — Drift triage
- Extend `drift-watch`: on bump, re-extract spec → op + schema diff → report
  only added/removed ops + renamed fields. (1.18.31→1.18.34 = 0 drift.)

### P6 — Version-skew fix + acceptance
- Default web UI = freeze's embedded bundle (815 KB gz, version-matched);
  CF-latest becomes opt-in.
- Acceptance: real `opencode attach` asserting JSON content-type on v2 routes;
  pair-check; replay 26/0; guards; deploy + soak.

## Honest limits
- Tier B requires per-endpoint handlers (v1↔v2 data models do not overlap).
- Tier C are shape stubs; v2 `context`/`history` are projections over the stored
  event log, thinner than upstream's event-sourcing.
- P4 catches type/shape drift, not semantic drift — pair-check remains the
  semantic oracle.
