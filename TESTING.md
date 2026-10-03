# refine — Testing spec (ISTQB-aligned, anti-theater, adversarial)

Companion to `PLAN.md`. Terminology follows the ISTQB Foundation/Advanced syllabi. The two
mottos: (1) a test that passes with its guard removed is theatre; (2) every requirement has a
traceable test, every test has a traceable requirement.

## 1. Anti-theater rules (binding, enforced in CI)

1. **Negative control mandatory**: every bug-fix or guard gets a test that fails when the fix
   is reverted. Mutation testing (`cargo-mutants`) on `refine-store`, `refine-core`, and the
   SSE encoder proves it systematically — survivor threshold agreed at first run, then gated
   (no number claimed before it is measured).
2. **No untested claims**: any behavior asserted in PLAN/STORAGE/MEMORY/SRE must resolve to a
   test ID in the traceability matrix; docs are reviewed against the matrix, not the reverse.
3. **Oracle honesty**: differential replay passes = "matches upstream on recorded corpus",
   never "correct". Drift tests exist precisely where our design *deliberately* differs
   (slow-client disconnect, `{id}` quirk) — each divergence is an explicit, named exception
   test with justification, never a silently skipped assertion.
4. **Flake policy**: a test that fails intermittently is a defect (severity ≥ major), not a
   rerun. Quarantine with an issue and a deadline; no `sleep`-based synchronization.
5. **Coverage is a floor, not a goal**: branch gates on core crates; the *effectiveness*
   signal is mutation score + adversarial findings caught by tests, not % lines.
6. **Every shipped bug class gets a guard pair** (static check + behavioral test) so the
   class cannot return silently. `scripts/check-guards.sh` runs with the commit gates;
   negative controls for each rule planted and asserted at script introduction (2026-10):

   | Shipped bug | Static guard | Behavioral test |
   |---|---|---|
   | swallowed storage write (`let _ = writer.write` — finalize SQL failed silently for days) | `check-guards.sh` rule 1 | `finalize_writes_agent_and_model_columns` (row-affecting) |
   | literal `\\` backslash in a SQL string (syntax error, also swallowed) | `check-guards.sh` rule 2 | every WriteOp executes in tests |
   | whole-session `Value` materialization (OOM-killed the cgroup twice) | `check-guards.sh` rule 3 (`load_messages` banned in HTTP; named exception = `post_summarize` compaction full-read, marked `allow:load_messages` on the call line — upstream reads history the same way) | streamed responses carry **no Content-Length** (asserted in `page_headers_...`); big-session live gate K-MSG-FIELDS |
   | happy-path-only state cleanup (abort leaked `prompt_locks`; permission asks leaked on abort) | RAII `LockRelease` / `PendingGuard` — cleanup lives in `Drop`, never after an `.await` | `abort_releases_prompt_locks_and_tasks`, `dropping_pending_guard_clears_entries` |
   | provider hang → permanent busy + stuck lock queue | stall watchdog covers open AND read phases (`REFINE_PROVIDER_STALL_SECS`, default 120s) | `silent_provider_fails_within_stall_budget_and_releases_state` (1s budget, own binary) |
   | event ring overflows by BYTES (count-only bounds × MB-class part frames — the unbounded-growth class that OOMs upstream's per-subscriber queues) | custom EventBus: evict-oldest under `BUS_CAPACITY` **and** `ring_byte_budget()` (env `REFINE_EVENT_RING_MB`, default32MB); publishers never block; lagging receivers disconnect | core: `ring_is_byte_bounded_not_just_count_bounded` (+negative control: remove the eviction condition → red), `single_oversize_frame_admitted_alone`, `publisher_never_blocks_on_stalled_receivers`, `bus_delivers_and_bounds_by_count`; harness S5 storm records lag/evicted deltas + socket return-to-baseline |
   | part written without its search projection (FTS silently misses rows) | `check-guards.sh` rule 4: `msg_part` INSERT/UPDATE only in `refine-store/src` (helpers carry the companion) | search suite: blob-hit, backfill idempotence, PATCH-reindex, cascade-delete |
   | literal backslash at EOL in any SQL string (shipped ONCE in W1 — stray `\` reached SQLite) | `check-guards.sh` rule 2 widened to any `\\$` EOL in `crates/*/src` (was `sql:`-prefixed only — the widening itself has a planted-violation negative control) | backfill/search suites execute every SQL path |
   | plugin hook registered but never dispatched (P0 class: loads "ok", zero effect — the whole magic-context/codex-auth wire silent) | TRACEABILITY `K-HOOKS` parity table: every ported site cites its upstream line; sites with no dispatcher test fail the matrix | per-site integration test in `hooks.rs` (synthetic sidecar, mutation must reach wire/persist/execution) + negative control (plugins-disabled twin must differ) + `experimental_tool_routes` + real-plugin battery at P0 exit |

   **Named divergences (K-ADMIN):** GET /provider/auth derives methods from configured
   providers (`api-key` only — v1 derives from auth hooks incl. OAuth we cannot run);
   MCP auth start/callback/authenticate routes are ABSENT (no OAuth MCP servers
   configured; GET falls to SPA HTML like other unknown routes); dispose has no instance
   registry to tear down (event + reload only); `disconnected` is a refine-side status
   value (v1 corpus captured connected/failed only).

   **Named divergences (K-CONFIG):** refine serves ONE config (global==user file;
   `PATCH /config` and `PATCH /global/config` share a handler/target); live-swap covers
   the derived route payloads only — provider endpoint registries (LLM routing) rebuild
   on restart, not on PATCH (v1's instance disposal rebuilds everything).

   **Named divergences (K-SUMMARIZE):** refine's summarize does NOT create
   compaction-state/history filtering (upstream `filterCompacted` hides pre-compaction
   messages from later prompts — refine keeps full history and appends the summary as an
   ordinary assistant message, per the live probe's 4-message shape minus the
   `info.summary` marker); the agent system prompt stays attached (v1 compaction shapes
   its own system); no overflow/prune machinery (full history serialized, tool outputs
   truncated at 2000 chars like v1 serialize).

   **Named divergences (K-SYNC bridge):** legacy-delta sync is *additive-only* — upstream
   message/part **edits and deletes are not pulled** (rare in v1; a legacy edit after sync
   shows as the original in refine). Legacy **wins** title/time_updated for imported sessions
   (a rename done in refine is overwritten on the next tick). Both are accepted trade-offs for
   a development bridge that dies at final migration.

## 2. Test basis and artifacts

| Artifact | Content | Owner/review |
|---|---|---|
| Test strategy (this doc) | levels, techniques, tools, entry/exit | reviewed at each milestone |
| Traceability matrix | every F*/N* standard + plan kill criterion ↔ test IDs ↔ code location | checked by CI script (doc link checker + grep for orphan test IDs) |
| Test cases | ID, precondition, steps, expected result, oracle type (differential / golden / property / invariant), priority (risk-based) | code review |
| Defect reports | severity (blocker/major/minor/trivial), priority, root cause, regression test added | every fix |
| Test log + results | JUnit XML artifacts per run, retained 30 days | CI |

**Test basis sources**: frozen contract (PLAN §3), kill criteria (PLAN §6), standards
traceability (PLAN §1), upstream OpenAPI + recorded corpus, oc-remote source (74 calls, 40
event types), the two antagonism ledgers (every finding = at least one test).

## 3. Test levels (ISTQB) mapped to refine

| Level | What it covers here | Where it runs |
|---|---|---|
| **Component** | pure functions: chunker, zstd frame codec, event-ring bounds, config parsing (deny_unknown_fields), prompt assembly, tool-call stream assembly, canonical JSON hashing | `cargo test` per crate, per PR |
| **Integration** | handler↔store, store↔blob file system, plugin host↔hook dispatcher, MCP client↔mock server, SSE writer↔hyper body, import↔fixture DB | `cargo test --test *`, per PR |
| **System** | full binary vs upstream: record/replay differential, byte-golden SSE, TUI attach flows, oc-remote flows | CI with upstream 1.18.31 container, per PR + nightly |
| **Acceptance** | user-driven: daily-driver sessions on real projects for N days, plugin behavior sanity, import fidelity on the real 20 sessions | pre-cutover checklist (PLAN §14 M5) |
| Smoke | boot self-checks + health + one prompt roundtrip | every build, first thing CI runs |

Static testing (ISTQB) precedes dynamic: clippy `-D warnings`, review checklists (state-machine
reviews for session lifecycle; API contract review per handler), rustdoc examples compiled.

## 4. Test design techniques (applied, not decorative)

| Technique | Applied to | Example test case |
|---|---|---|
| Equivalence partitioning | config knobs (cache sizes, pool sizes, ring limits) valid/invalid classes | `cache_size=0` (invalid) → boot fatal with message |
| Boundary value analysis | ring capacity (4096th/4097th SSE event), chunk size (1 MiB±1), `substr` windows on 122 MB row, retention days 0/1/30 | 4097th event while client stalled → disconnect policy fires |
| Decision tables | auth mode × route class × credential present (off/basic × SSE/REST × header present/absent) | basic + SSE without header → 401 before stream opens |
| State transition testing | session lifecycle (idle→prompted→streaming→tool→compacting→aborted→idle; plus crash→claim→resume) — transitions from PLAN §11 v2 adoption | SIGKILL during tool call → restart → claim resumes exactly once |
| Use case testing | the 9 oc-remote flows + TUI attach flows (system level) | permission reply mid-stream from Android |
| Error guessing (from the ledgers) | every prior finding: WAL pinning, missing blob, torn blob/DB write, oversize frame, hook deadline, OAuth expiry | connection checkout crossing `.await` → CI lint test fails |
| Pairwise (pairwise) | config combinations (auth × metrics × plugins × cache) | combinatorial smoke, kept small by risk ranking |
| White-box: branch + MC/DC-style | blob/DB crash protocol decision logic, retry classification (overflow/rate/incomplete), SSE overflow policy | exhaustive branch on the commit ordering |

## 5. Non-functional testing (refine's real risk surface)

| Type | Method | Gate |
|---|---|---|
| Performance | criterion benches per hot path; p95/max latency suite; 10-min accelerated soak per PR, 24 h nightly | `PLAN §7` table, `MEMORY.md §5`, `STORAGE.md §7` |
| Reliability/fault | SIGKILL fuzz (blob protocol ×1000, claim/recovery), disk-full injection (WAL dir full → boot/readiness behavior), clock jumps, upstream-WAL-growth abort guard | all green nightly |
| Security | authz matrix on every route incl. SSE and `/metrics`; header/CORS byte-compat; secret handling (OAuth tokens mode 600, never in logs — grep-gate in CI); plugin isolate memory/deadline escape attempts | no unauthenticated route except freeze-documented ones |
| Compatibility | differential replay + byte-golden + provider stream fuzz (10 k chunk splits) + latest-upstream nightly drift | 0 diff except named divergence tests |
| Usability (operational) | `doctor` messages actionable (each failure mode has a forced-failure test producing the message) | smoke |

## 6. Adversarial testing (standing program)

- **Fuzzing** (`cargo-fuzz`, nightly, corpus seeded from recorded sessions): SSE frame parser,
  JSON event decoder, provider stream assembler, config TOML, `substr` window math, MCP JSON-RPC.
  Every crash → minimized corpus entry + regression test (fuzz finding = defect report).
- **Property tests** (`proptest`): chunker roundtrip (arbitrary bytes incl. sizes 0/1/2²⁰/
  2²⁴), zstd frame integrity, ring never exceeds bound, canonical-hash stability under key
  reorder, event sequence never skips after claimed resume, importer hash equality under
  truncation injection.
- **Mutation testing** (`cargo-mutants`, nightly): survivor report reviewed like a defect
  backlog; survivors in store/llm = must-fix before milestone exit.
- **Chaos scenarios** (scripted, nightly): kill during each phase of write path; corrupt a
  blob byte → doctor detects; fill disk → readiness 503, no corruption; plugin infinite loop →
  hook deadline, process alive; MCP server dies mid-call → typed error, session continues.
- **Two-person rule**: major features ship with at least one adversarial test authored from
  the *opponent's* view (how would a client/provider/plugin break us?) — written in the PR
  description section "Attacks considered".
- **LLM-dependent behaviors** (agent loop, compaction quality): no flaky assertions on model
  output; replayed-provider determinism for logic tests; real-model checks are nightly,
  interleaved baseline/gate in the same batch (2.7× variance), results recorded with
  provenance — never gate a PR on a live model.

## 7. Regression and change control

- Every defect fix adds: (a) failing test first, (b) negative control (mutation/revert), (c)
  matrix row. Reopened defect = process retrospective trigger.
- Contract change (upstream bump): run full replay before and after; new recordings are
  reviewed diffs, not silent overwrites (golden corpus versioned in git).
- Impact analysis for refactors: matrix points tests at requirement, so untraced code touching
  a traced requirement blocks merge.

## 8. Entry/exit criteria (per milestone, ISTQB-style)

| Phase | Entry | Exit |
|---|---|---|
| M0→M1 | fixtures captured, upstream container builds | upstream passes its own corpus (harness self-test); freeze artifacts complete |
| M1→M2 | smoke + SSE byte-goldens green | system-level list/attach tests green; auth decision table covered |
| M2→M3 | component/integration suites green | recorded transcript replay 0 diff; state-transition suite green; provider fuzz green |
| M3→M4 | storage/memory gates green | import gates green; p95/max gates green; mutation threshold met |
| M4→M5 | plugin/MCP gates green | 3 plugins + 3 MCP servers acceptance flows green; OAuth expiry test green |
| M5→cutover | nightly stable 1 week | **all** §7 KPIs green over 24 h soak; fault/security suites green; acceptance checklist signed; rollback drill done (opencode service restores in <1 min) |

No milestone exits on "mostly passing": exit = every gate in scope green or an explicitly
defect-filed waiver (blocker/major never waivable).

## 9. Test environment and data

- Local: fixture DB (20-session golden copy), recorded corpora, mock MCP servers, recorded
  provider streams. Real sessions never used as fixtures directly (import creates them).
- CI: pinned runner class (latency gates need stable hardware), upstream 1.18.31 container,
  cache warmed before timing runs.
- Test data hygiene: synthetic credentials only; recorded corpus scanned by the secret scanner
  (existing gitleaks pattern) before committing recordings.
