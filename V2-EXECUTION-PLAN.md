# Execution plan: pristine + impressive (all decisions locked)

## Locked decisions

| # | Decision |
|---|---|
| D1 | v2 plugins work — full `ctx` subset, one build (no phasing) |
| D2 | Password auth implemented |
| D3 | Credential echo keeps upstream parity |
| D4 | G2 re-run, arrival independently gated |
| D5 | Bump held (2.0.32 stays informational) |
| D6 | Password stdout: flag/env-gated, **default file-only** |
| D7 | Shim normalizes **both** promise and Effect registration dialects |

Antagonism findings incorporated: A1 (twin dialects + module-dir shim + mechanical fixture extraction), A2 (rule-34 scope: same-sidecar-protocol only, amend wording), A3 (stdout tradeoff → D6), A4 (separate arrival gate — prerequisite), A5 (plumb v2 mix via LOAD_ROUTES with separate thresholds), A6 (retain run artifacts; one-hop trust), A7 (pin list-echo assertion + pair-v2 credential vectors), A8 (drift watches `packages/plugin/src`; no-silent-rebasing kill rule).

## Phase 0 — integrity (first; gates must be truthful before certifying)

1. Rule-27 plant: match `^def provider_view(` at checker (:453) + plant detector (:496). Prove red→green in-run.
2. Inline plants for 16b (temp-spec minus one op → GAP + nonzero, trap-restore) + 35 (`__rule35_plant.rs` bare trim+join → flagged, removed).
3. Header honesty (:1-5): enumerate exactly which rules self-prove vs rely on suite tests.
4. Ceilings: CI (`ci.yml:116`) + SRE (`SRE.md:258`) to 34M with nightly's provenance chain.
5. Hygiene: trace rule 34; fix `D-PERM-V2-*` (cited in scripts, absent from the PLAN §18 ledger); restore pair-v2 exec bit; event-validate non-vacuous (seeded window or minimum-row gate); update TRACEABILITY `:177-178`.
6. Kill-criteria amendment: add *no silent gate re-basing*.
7. Gate separation: closed thresholds for closed runs only; arrival gets its own pre-registered gate (achieved% floor, p95 bound, `dropped_iterations=0`, err bound, freeze-RSS co-tenant guard). Fix contradicting comments.

Acceptance: every plant proven red→green in-run; ceilings agree; docs match reality.

## Phase 1 — password auth (D2, D6)

8. CLI + config: auth inputs on `Serve` (`main.rs:20-35`); `resolve_auth` flag > env > default (`:87-111`); thread through `serve()` (`:130-148,:231`); new `ocserve.toml` loader (`deny_unknown_fields`, `[server]` keys per SRE §74-78, fail-fast malformed).
9. Production wiring: `main.rs:345` `None` → `Some` under password mode; `auth_password_file` (600, never logged); stdout gate flag, default off (rule 8 stands).
10. Gate hardening (`lib.rs:5266-5322`): probe upstream `WWW-Authenticate` bytes first (mandatory — uncaptured), then challenge + constant-time compare + no-log verification.
11. Fallback ordering (`:5985` vs `:5998`): live-prove unknown paths + 405s challenge under `basic`; move fallback inside auth or check in `ui.rs:315`.
12. Tests: extend `auth_metrics.rs` (unknown paths, 405s, challenge header, SSE denial, CORS×auth, edge credentials, multi-pair) + CLI-level test (no hardcoded `None`, password-file flow end to end).

Acceptance: every route incl. fallback/405 challenges under `basic`; loopback default unchanged; no secret in file logs/metrics.

## Phase 2 — testing gates

13. Mutation: baseline store+core, add SSE/`ocserve-llm` to nightly scope, gate `missed < N`, reconcile TRACEABILITY rows.
14. Coverage floor: `llvm-cov`/tarpaulin over core crates with floor + ratchet (count ratchet stays as backstop).
15. Fuzz: wire short `cargo-fuzz` into nightly (or correct TESTING.md:200); add the 5 missing targets or justify each.
16. Real-plugin battery (committed, CI): vendored minimal plugins per authoring form + per dispatched hook, asserting load/dispatch/mutation. Fixes silent tuple-drop (`main.rs:252-261`) as part of options-tuple support.

## Phase 3 — v2 plugin shim (D1, D7)

17. Module `compat_v2/` (per-domain adapters, per-adapter mapping diffs): accept both promise and Effect dialects, normalize to one dispatch path; expose session/tool/permission/shell/event/rpc/storage/options backed by existing `hook_mutate` sites.
18. Translation table: arity 2→1; params/headers/system/messages → `session.context` facets; tool before/after (throw-to-reject, status union); `session.compacting` with synthesized `kind=compaction`; event push→subscribe bridge; `ctx.rpc.register` + storage + providerID scoping (new); `SessionRequestKind` synthesis (compaction real; title/generate synthesized or diverged explicitly).
19. RPC dispatch: `/api/rpc/:rpcID/:method` → `registrations.get(rpcID)?.at(-1)`, schema-validate in/out, exact error tags; unregistered → today's byte-exact 400. 30s deadline + fail-open; arrival mix gains an rpc vector (rule-34 scope note: same sidecar protocol extended, no new runtimes — amend rule wording).
20. Fixture honesty: generate v2 test fixture from vendored v2 type definitions mechanically + assert extraction in CI.
21. Verdicts: `text.complete`, `command.execute.before`, `config` (invoke instead of hard-`ok`, or document), `auth`/`tool-map` — bridge, emulate, or named divergence each. `plugin_list` wired to real inventory.
22. Differential: `/api/plugin` + `/api/rpc` vectors in pair-v2 against 2.0.28; credential list/create/activate vectors (pins D3 echo parity).

Acceptance: real v1 plugins load/dispatch through battery; v2 fixture matches upstream shapes; RPC exact-errors preserved where unregistered.

## Phase 4 — G2 fresh (D4)

23. Prerequisites: arrival gate separated (Phase 0.7), rule-33 extended to `credential`/`session_environment` + single-VU re-measure, v2-mix plumbed via `LOAD_ROUTES` with **separate** thresholds.
24. Run on `.227`: interleaved rounds, `LOAD_TARGETS=50,75,150`, closed GATED + `LOAD_ARRIVAL=1 LOAD_RPS=4480` 60s under the pre-registered arrival gate; retain full artifacts (summaries + samples + meta + report).
25. Derive + commit thresholds from the final binary before `GATED=1`; record PASS/FAIL per pre-registered discipline (arrival FAIL stays FAIL; frontier shipped honestly if ceiling falls short).

## Phase 5 — parity refresh + closeout

26. Green nightly on a clean disk; drift triage (2.0.32/1.19.3 → documented hold per D5); `task` stays queued; drift watch extended to `packages/plugin/src`.
27. TRACEABILITY rows per phase + planted controls green in-run; kill list enforced.

## Sequencing

Phase 0 → (Phase 1 + Phase 2 in parallel) → Phase 3 → Phase 4 (final binary only) → Phase 5. Per-phase TRACEABILITY row; per-guard planted proof in-run.

## Residual risks

- `.227` availability + co-tenant noise; freeze-RSS guard mitigates.
- Upstream challenge bytes + title/generate synthesis need live probes mid-build (mandatory steps).
- Effect-dialect normalization is the least-proven estimate — phased internals with full external behavior acceptable if needed.
