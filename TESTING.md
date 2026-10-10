# ocserve — Testing spec (ISTQB-aligned, anti-theater, adversarial)

Companion to `PLAN.md`. Terminology follows the ISTQB Foundation/Advanced syllabi. The two
mottos: (1) a test that passes with its guard removed is theatre; (2) every requirement has a
traceable test, every test has a traceable requirement.

## 1. Anti-theater rules (binding, enforced in CI)

1. **Negative control mandatory**: every bug-fix or guard gets a test that fails when the fix
   is reverted. Mutation testing (`cargo-mutants`) on `ocserve-store`, `ocserve-core`, and the
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
   | provider hang → permanent busy + stuck lock queue | stall watchdog covers open AND read phases (`OCSERVE_PROVIDER_STALL_SECS`, default 120s) | `silent_provider_fails_within_stall_budget_and_releases_state` (1s budget, own binary) |
   | event ring overflows by BYTES (count-only bounds × MB-class part frames — the unbounded-growth class that OOMs upstream's per-subscriber queues) | custom EventBus: evict-oldest under `BUS_CAPACITY` **and** `ring_byte_budget()` (env `OCSERVE_EVENT_RING_MB`, default32MB); publishers never block; lagging receivers disconnect | core: `ring_is_byte_bounded_not_just_count_bounded` (+negative control: remove the eviction condition → red), `single_oversize_frame_admitted_alone`, `publisher_never_blocks_on_stalled_receivers`, `bus_delivers_and_bounds_by_count`; harness S5 storm records lag/evicted deltas + socket return-to-baseline |
   | part written without its search projection (FTS silently misses rows) | `check-guards.sh` rule 4: `msg_part` INSERT/UPDATE only in `ocserve-store/src` (helpers carry the companion) | search suite: blob-hit, backfill idempotence, PATCH-reindex, cascade-delete |
   | literal backslash at EOL in any SQL string (shipped ONCE in W1 — stray `\` reached SQLite) | `check-guards.sh` rule 2 widened to any `\\$` EOL in `crates/*/src` (was `sql:`-prefixed only — the widening itself has a planted-violation negative control) | backfill/search suites execute every SQL path |
   | plugin hook registered but never dispatched (P0 class: loads "ok", zero effect — the whole magic-context/codex-auth wire silent) | TRACEABILITY `K-HOOKS` parity table: every ported site cites its upstream line; sites with no dispatcher test fail the matrix | per-site integration test in `hooks.rs` (synthetic sidecar, mutation must reach wire/persist/execution) + negative control (plugins-disabled twin must differ) + `experimental_tool_routes` + real-plugin battery at P0 exit |
   | nightly verification silently skipped (network/tool/PATH missing = no signal) | systemd user timers `Persistent=true` (missed runs catch up on boot) + `nightly.sh` fail-visible per step (missing binary/network = FAIL step, never a skip); pre-commit hook installed so AGENTS §3's promised checks actually exist | first live run6/6 (log cited in K-VERIFY); soak-gate negative fixtures (health/slope/short) all red; docs-required probe (staged .rs without TRACEABILITY) aborts |
    | upstream drift watch silently broken or nondeterministic (nightly report = noise; new upstream releases pass unnoticed) | `check-guards.sh` rule 5: `drift-watch.sh --selftest` (version compare, replay-output parse, set-difference classification, report writer — planted negative control: break `version_gt` → guards red) | every watch run replays the freeze control ×2 and **aborts on unequal failure sets** (drift vs noise cannot be claimed); first-boot warmup replay discarded on both arms (the determinism gate caught exactly this: `api_agent`/`api_command` failed run 1 only); **security-review hardening (all applied)**: M1 fail-closed replay (0-passed/transport), M2 flock mutual exclusion, M3 control+latest versions observed from `/global/health`, L1 npm-version regex, L2 private cron log, L3 FREEZE_RUNS≥2 enforced, L4 canned `.latest.out` detail-extraction (cell-position assert, planted negative control red→green), L5 `umask 077` — acceptance evidence: freeze-only determinism pass + full1.18.31-vs-1.18.34 run with both versions verified (drift=1 version-stamp, noise=6 config/data, anomaly=0) |
    | experiment/tooling mutated a non-ocserve host service (2026-10-05: a capture experiment wrote a drop-in under `systemd/user/opencode.service.d` + restarted the user's opencode — broken until manually fixed 6.5 h later) | `check-guards.sh` rule 6: `scripts/`+`bench/` (`.sh`/`.py`) may not reference `systemd/user/*service.d` paths or `systemctl --user edit/restart/stop/start/mask/kill`, except lines carrying `ocserve` (read-only `status`/`cat`/`show` intentionally unmatched); AGENTS §2 rule 11 is the agent-side half | planted negative controls at introduction: non-ocserve restart plant → guards rc=1 with the rule-6 FAIL line; ocserve restart plant → rc=0 (carve-out); restore → rc=0 |
    | test asserting exact deltas on a process-global counter from an in-crate unit test (the `reader_opens` accounting test raced sibling tests that open readers in parallel — observed fail→pass on identical code, 2026-10-05; flaky test = defect) | `check-guards.sh` rule 7: `assert*` on `reader_opens` banned in `crates/*/src` — such tests must live in `crates/*/tests/` where the binary owns the process and the crate is compiled without `cfg(test)` (production path) | the moved test `tests/reader_accounting.rs` passes standalone and in the full suite; mutation-kill parity proven: planting `READER_OPENS.fetch_add` removal → moved test red (`left: 0, right: 1`), restore → green; rule-7 planted control: src assert plant → guards rc=1, restore → rc=0 |
    | torn/partial normalized plugin output (crash mid-write → syntax-error load at boot; D1 writes `.normalized.mjs` beside entries and under `<data>/normalized/`) | `check-guards.sh` rule 8: `fs::write` with `.normalized` in its args banned in `crates/*/src` — final normalized paths only via `atomic_write` (pid-unique tmp + rename; hash file written AFTER the rename so any crash prefix leaves hash≠content → rebuild) | `wipe_then_normalize_rebuilds` + `warm_hash_skips_rebuild` (warm gate = hash match ∧ output exists ∧ size>0) + `normalize_error_falls_back_to_raw`; rule-8 planted control: direct-write plant → guards rc=1, restore → rc=0; kill-switch test proves rolldown never constructs when disabled (A3 panic=abort residual gate) |
    | child OOM bounces the whole service (2026-10-06 11:00:16: kernel correctly killed only the sidecar child per A3, then default `OOMPolicy=stop` marked the unit `oom-kill`-failed and restarted ocserve — the A3 "respawnable child" claim was half-true without this) | `check-guards.sh` rule 9: `OOMPolicy=continue` must be present in `deploy/ocserve.service` (the unit template) | synthetic control units with `MemoryMax=64M` + a 200 MB child hog (recipe in SRE §5): default-policy unit → `failed` with `Failed with result 'oom-kill'`, `continue` unit → `active`/MainPID alive after the kernel kill (journal 2026-10-06 11:20:04, 2 kernel OOM events); rule-9 planted control: line removed → guards rc=1, restore → rc=0 |
| cgroup OOM-killed ocserve 6× in one day (2026-10-05): warm unit ≈ main 487 MB + plugin host 145 + browser-harness 78 ≈ 680 MB vs `MemoryMax=750M` → ~118 MB headroom; catalog reload was the proven tight trigger (`models.json` write → kill +6 s — the reload parsed the 5.3 MB catalog TWICE); equal `oom_score` meant the kernel always killed MAIN (full service death) even when a child was allocating; soak `sidecar` column counted the plugin host only (bh/shim invisible) | (a) one catalog parse per reload (`Runtime.catalog` reused by `llm_registry`), (b) reconcile INFO-logged (changed files + duration + RSS delta) so kills are journal-attributable, (c) children spawn through an exec-wrapper that raises their `oom_score_adj` to 500 → memcg kills take a respawnable child (`ensure_alive`) — A3 completed 2026-10-06: kernel DID kill only bun on the sidecar-embedding OOM, but default `OOMPolicy=stop` then bounced the unit; `OOMPolicy=continue` + control units (stop→failed/oom-kill, continue→active) close the class (rule 9), (d) soak gained `cgroup`/`cgroup_peak`/`kids` columns, (e) journald retention floor staged (`deploy/journald-retention.conf` — root fs at 6.8% free < journald default SystemKeepFree 10% had pruned user logs to ~3 h, making the morning kills unverifiable) | A1: `registry_catalog_tests::llm_registry_uses_stored_catalog_not_a_second_disk_read` (fixture-vs-disk API — plant: restore the disk re-read → red); A3: `oom_wrapper_tests` in ocserve-mcp + ocserve-plugin (child reports adj=500 — plant: wrapper without the echo → red ×2); A2: live reconcile INFO line observed post-deploy (forced via `ocserve models refresh`); A4: soak one-tick row shows the three new columns; (A5) watch content-hash skip — identical-bytes mtime churn never reloads (`same_content_rewrite_is_skipped_content_change_fires`; plant: hash-skip disabled → red) after warm reloads proved lethal (kill #7, journal 17:01:33, +237MB via A2 log) → interim `MemoryMax 750M→1024M` (measured peak ~951MB); *B-phase*: lab A/B proved FRAGMENTATION not leak — `oom_reload.py` plain [220,68,1]/526MB vs arena1 [69,0,1]/308MB, bytehound 24.3M temporary vs718k live (live payload by design) → unit `Environment=MALLOC_ARENA_MAX=1`, cap back to 750M, production reload +90 cold/+0.2 warm, idle cgroup402 ≤460 (`OOM-RELOAD-REPORT.md`); Tier-1: trim_heap — full-catalog lab without trim 237→307MB vs with trim 89→94MB (negative control `notrim`; `trim_contract.rs` killswitch test) |
    | corrupt/unreadable models catalog failed boot (`load_for` did `read_json(cache)?` — upstream models-dev.ts deletes the file and refetches; a torn write would have taken ocserve down) | self-healing `read_catalog` (warn → best-effort remove → empty) used by `load_for` + `llm_registry`; fetch side is parse-validated before replace (D-MODELS-2) so ocserve never writes a corrupt catalog itself | `catalog_selfheal_tests::corrupt_models_catalog_self_heals_instead_of_failing` (corrupt removed / healthy kept / missing empty) with **planted control** (removal skipped → red, restore → green); models_dev non-JSON fetch rejection test |
| L1 recycle interrupting an in-flight plugin hook, recycling on a warm-up blip, or respawning into a recycle storm (the "middle ground" failing into either unbounded or thrash) | pure decision fn `should_recycle` (threshold ∧ debounce ∧ idle ∧ uptime all load-bearing) + guard rule 10 (A3 wrapper byte-identical in ocserve-mcp + ocserve-plugin — both spawn paths carry adj + kids move) | planted controls ×4 (kill-switch/debounce/idle-gate/uptime each removed → test red → restored green); live battery: forced low-threshold drop-in fires exactly once after uptime≥300s with metric + hooks recover via replay; rule-10 planted control: drift one copy → guards rc=1 |
| partition restructuring a terminal's/cargo's cgroup tree (tests & replay spawn `ocserve serve` in WHATEVER cgroup they live in) or silently doing nothing (the wiring-inversion class: unit tests passing while the call site passes `enabled()` into `disabled`) | scope gate `enabled_from` pure fn (bare → never; `=0` veto wins over systemd; `=1` forces) + probe-before-mutation (no mutation on any failed probe) + `outcome()` glue seam | glue test **exists because the live battery caught the inversion** (`result="disabled"` with INVOCATION_ID present — planted control: drop the `!` → glue test red, restore green); planted ×5 (kill-switch arm/systemd gate/self-move/+memory/kids ceiling); battery asserts `result="ok"` ∧ `subtree_control`=`memory` ∧ `kids/memory.max`=734003200 ∧ self in `main/` ∧ children in `kids/` |
| pair comparator vacuous / pass-set equality mistaken for equivalence (two servers can fail the SAME recorded route for different reasons; the recorded corpus came from a different environment — fixture-noise, drift control 19 pass/7 fail/5 defer) | design: pair mode gates on DIRECT live response diff, never recorded pass-sets; recorded freshness is info-only by construction; allowlist is the sole gate exception and each entry must cite a D-PAIR row (`check-guards.sh` rule 12) | `pair-check.sh --self-test`: clean run rc=0, then a key planted into one arm's fixture config must produce `PAIR-DIVERGE config` rc=1 (rc without route = vacuous → exit 3); rule-12 planted control: uncited entry → guards rc=1; first live run itself was the strongest control — 6 divergences surfaced and were triaged (config fixed, catalog asymmetry fixed, 3 named D-PAIR) instead of silently passing |
| fixture environment asymmetry between pair arms (one arm fetches the catalog, the other reads a cache; one arm has an internal provider registry, the other is config-derived) | identical fixture HOMEs built from one source, `OPENCODE_DISABLE_MODELS_FETCH=1` on BOTH, same cwd, same seed, version assert both arms (drift M3 lesson) | live pair output shows symmetric freshness (F 19/7 vs R 22/4 info lines) and zero unexplained asymmetries after the models.json copy; D-PAIR-2 documents the residual registry-vs-catalog provenance gap with its fix path |
| load harness silently targeting the LIVE services (k6 ramp against :4912/:4901 would load the user's daily-driver servers) | `check-guards.sh` rule 13: any load-test.sh line invoking k6 may not contain a live port (health-assert lines carry no k6 token → unaffected); never-live rule also written in README | rule-13 planted control (k6 line + live port → guards rc=1, restore rc=0); `--self-test` proves the threshold exit-code wiring both directions (clean rc=0 / planted breach rc≠0 / `--no-thresholds` neutralizes) without any server; live health asserted before+after every real run (infra exit if broken) |
| load results outgrowing their evidence (latency deltas claimed from n=1 on a shared desktop; empty-table benchmarks posing as read-load) | claim classes printed in every report (capacity/errors n=1 valid; latency deltas INDICATIVE until ROUNDS≥3) + fixture count-equality assertion (both arms same data or infra-exit) + real-data snapshot (never empty tables) + loadavg recorded per tick | report generator prints claim classes unconditionally; count-mismatch path exits 2 (tested by construction — builder/restore); quiet-host gate logs |
| uninstall deleting shared opencode state (the 2026-10-05 blast-radius class applied to the exit path) | `check-guards.sh` rule 11: install/uninstall scripts may not `rm` opencode/auth/models paths (only `*.normalized.mjs` lines allowed) | staged fake-HOME `uninstall.sh --selftest` canary battery (ocserve units/binary/derived gone; `opencode.json`/`auth.json`/`models.json`/`opencode.service` byte-identical; history kept by default, gone only under `--purge`; idempotent) — nightly-wired; rule-11 planted control: `rm ~/.config/opencode` plant → guards rc=1 |
| unbound v1 SDK route silently proxied as HTML (web UI settings crashed on `e.shells.reduce` after `/pty/shells` returned the app HTML — any unbound JSON route is the same class) | `check-guards.sh` rule 14: every URL in `bench/sdk-routes.txt` (extracted from the frozen v1 SDK) must be bound in the router (wildcard-segment aware) or carry an exact-URL PLAN §17 citation | planted control: bogus URL appended → guards rc=1 → removed → rc=0; behavioral: the batch's byte-shape tests (`tests/compat_batch.rs`, `tests/pty.rs`) assert the envelopes that previously never existed, and the live battery hit the exact crash path (`/pty/shells` → JSON array) |
| web-UI route method/param drift (a route bound at the wrong method, or a bundle route we never bound, silently degrades the app — `/global/config`, `/vcs/*`, `/api/health` were HTML fallbacks) | `check-guards.sh` rule 15: method-aware binding over `bench/webui-routes.txt` (extracted from the LIVE bundle), wildcard-segment aware, v2/exempt groups cited in PLAN §17 | planted control: a bogus `METHOD /path` appended → guards rc=1 → removed → rc=0; behavioral `webui_perf.rs::route_closure_shapes`; replay `PASS vcs`; pair 22/0 |
| a route bound at the wrong HTTP method (PATCH vs PUT on `/pty/{id}` — PATCH fell to the SPA with 200 HTML) | method_not_allowed_fallback routes unknown methods to the same handler as the catch-all (freeze parity: its catch-all is a route) | `tests/pty.rs::create_get_update_delete_lifecycle` asserts PATCH→200 SPA while PUT→JSON (probed against freeze) |
| internal payload key ≠ frozen wire key (the 2026-10-09 double-prompt class: `prompt.rs` read `messageId` while the v1 contract sends `messageID`, so client ids were silently dropped and oc-remote rendered both its optimistic row and the server row; siblings found in the same audit: command `model` string ignored by an object-only pointer read, shell's custom `command required` messages that never existed upstream, v2 payload errors rendered in the v1 envelope) | (a) typed decode for the whole prompt family (`ocserve-core/wire.rs` — `PromptRequest`/`CommandRequest`/`ShellRequest`/`V2PromptInput` with byte-exact Effect messages from `bench/openapi/field-probes.md`), (b) `wire_keys_match_frozen_spec` pins each struct's wire-key set to `bench/openapi/1.18.31.json` (upstream adds/renames → spec refresh → red), (c) `check-guards.sh` rule 20: every request key read off an axum `Json(...)` handler binding must exist in the frozen contract (allowlist: `offset` for the additive search route) — selftest plants `payload.get("messageId")` and requires the flag | `tests/field_contract.rs` (15: envelope bytes per route family — v1 `name/data/kind` vs v2 `_tag:InvalidRequestError`, decode-precedes-404 ordering, noReply returns the user message without any model call, lowercase `messageId` never stored, command model string reaches endpoint resolution by provider NAME); `golden.rs` prompt_async flipped to `messageID` + echo assert + lowercase negative pin; wire.rs probe corpus (13 tests, ~70 byte cases); nightly `field-contract-check.sh`: 42 decode vectors byte-compared live against freeze + identity echo on both arms (first run: 42/42 identical, 0 divergences) |
| live-only (`emit_live`) event payload ≠ frozen contract (the 2026-10-11 "web UI shows nothing until reload" class: `message.part.delta` carried `partID:""` + the USER `messageID` with no preceding start part, so the web UI's reducer — which keys deltas by `partID` against a published part — dropped every delta; the reply only appeared after reload because the full text part is persisted). Bus-only events never reach the persisted `event` log, so rule 19 (log validator) and the JSON-body validator are structurally blind to this class | (a) source: mint assistant MessageID + text/reasoning PartIDs BEFORE the provider turn, publish the assistant skeleton lazily (no ghost on failure), emit a start part before each part's first delta, deltas carry `^prt` id + assistant messageID, persisted final part reuses the streamed id (`bench/openapi/STREAM-DELTA.md`), (b) `check-guards.sh` rule 21 bans the literal empty `partID` assignment in `crates/*/src` (planted empty-partID self-check first) | `tests/stream_delta.rs`: stub-provider real turn asserts the full streaming contract (skel- no `time.completed`, start part before deltas, `^prt` delta ids, assistant messageID, persisted id reuse); negative control feeds the exact pre-fix frame to the shared checker and requires a flag (non-vacuous both directions); live real turn confirms skeleton + start part + `^prt` deltas |

   **Named divergences (K-ADMIN):** GET /provider/auth derives methods from configured
   providers (`api-key` only — v1 derives from auth hooks incl. OAuth we cannot run);
   MCP auth start/callback/authenticate routes are ABSENT (no OAuth MCP servers
   configured; GET falls to SPA HTML like other unknown routes); dispose has no instance
   registry to tear down (event + reload only); `disconnected` is a ocserve-side status
   value (v1 corpus captured connected/failed only).

   **Named divergences (K-CONFIG):** ocserve serves ONE config (global==user file;
   `PATCH /config` and `PATCH /global/config` share a handler/target). Live-swap covers
   derived route payloads AND the LLM registry (endpoints/keys/limits/default model —
   rebuilt on every reconcile; v1's instance disposal rebuilds everything on write).
   **D-CONFIG-1 (hot-reload):** upstream has NO file watcher — config is
   `Effect.cachedInvalidateWithTTL(Duration.infinity)` invalidated only on its own write
   (v1 config/config.ts:295,302,678), so an external edit needs a restart there; ocserve
   applies external edits within one poll interval (`OCSERVE_CONFIG_POLL_MS`, default
   2000, min 100; `OCSERVE_CONFIG_WATCH=0` disables) via fail-safe reconcile (a broken
   file keeps the old state serving and retries — boot remains fail-fast). MCP section
   changes connect/disconnect live. No shape change; replay unaffected.

   **D-TITLE-1 (auto-title):** upstream v1 never generates titles — `New session - <ISO>`
   defaults only, manual rename via PATCH; oc-remote likewise only renames from user
   dialogs. ocserve additionally renames a *still-default* session from its first user
   message after the first successful turn (partial `session.updated {id,title}`; SQL
   gate = empty or freeze-default prefix, so user-named sessions are never touched;
   command flows excluded via `RunOpts.auto_title=false`). Default-title creation itself
   is freeze-parity, not a divergence.

   **D-AGENT-MODEL (parked):** freeze resolves the compaction model as
   `agent.model ?? userMessage.model` (compaction.ts:359-361); upstream prompts
   also honor an agent's pinned `model`. ocserve never consults `agent.model` —
   harmless while no configured agent pins a model (all current ones don't),
   but a real divergence for pinned-agent configs. Parked with evidence; the
   compaction-model *selection* itself (session model) already matches.

   **D-PROMPT-BUDGET (history byte cap):** upstream streams the entire
   session history into every provider request with no bound; ocserve drops the
   OLDEST history first once `PROMPT_HISTORY_MAX_BYTES` (8 MB) is exceeded,
   always keeping the newest exchange, with `ocserve_history_truncated_total`
   + a warn log (measurable, not silent). Under-budget sessions are
   byte-identical. Rationale: unbounded history × concurrent prompts is the
   anon-memory hole at the stated scale target (thousands of sessions, tens
   concurrent); real overflow still routes through M6 compaction.

   **K-AUTONOMY failure class (silent stop):** background prompt failures were log-only
   (`prompt_async` logs + returns nothing downstream) — the run "just stopped". Every run
   error now finalizes state and emits durable `session.error` + a `[turn stopped]` text
   part; `autonomy.rs::round_cap_fails_loud…` is the behavioral proof with a planted
   negative control.

   **Named divergences (K-SUMMARIZE):** ocserve's summarize does NOT create
   compaction-state/history filtering (upstream `filterCompacted` hides pre-compaction
   messages from later prompts — ocserve keeps full history and appends the summary as an
   ordinary assistant message, per the live probe's 4-message shape minus the
   `info.summary` marker); the agent system prompt stays attached (v1 compaction shapes
   its own system); no overflow/prune machinery (full history serialized, tool outputs
   truncated at 2000 chars like v1 serialize).

   **Named divergences (K-SYNC bridge):** legacy-delta sync is *additive-only* — upstream
   message/part **edits and deletes are not pulled** (rare in v1; a legacy edit after sync
   shows as the original in ocserve). Legacy **wins** title/time_updated for imported sessions
   (a rename done in ocserve is overwritten on the next tick). Both are accepted trade-offs for
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

## 3. Test levels (ISTQB) mapped to ocserve

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

## 5. Non-functional testing (ocserve's real risk surface)

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
- **Mutation testing** (`cargo-mutants`, manual/weekly via `nightly.sh --with-mutants` —
  hours of CPU, never during benchmark sessions; survivor report reviewed like a defect
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
