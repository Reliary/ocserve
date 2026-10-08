# ocserve — Rust drop-in server for opencode v1

Status: **plan, pre-implementation, adversarially stress-tested twice** (2026-10-01).
Every claim is sourced from local trees/service/DB measurements, direct empirical tests, or a
verified reference (§12). Nothing is aspirational.

**Companion specs (part of this plan):**
- `STORAGE.md` — SQLite tuned within an inch of its life (pragmas, FTS5, blob protocol, storage CI gates)
- `MEMORY.md` — reconciled 300 MB budget, allocator/runtime discipline, memory CI gates
- `SRE.md` — fail-fast, metrics, tunability, CPU/arch policy, DevOps (CI/CD, runbook)
- `TESTING.md` — ISTQB-aligned test strategy: levels, techniques, traceability matrix,
  anti-theater rules, adversarial program (fuzz/property/mutation/chaos), entry/exit criteria

Related: `~/src/opencode-v1-v2-diff.md` (v1 vs v2 source comparison).

---

## 1. Goal, scope, standards

**ocserve** is a single native Rust binary replacing the local user service
`opencode serve --port 4901` (Homebrew opencode 1.18.31) for one power user, keeping these
clients working **unchanged**:

- opencode TUI in attach mode (`opencode attach <url>`)
- **oc-remote** Android client (Ktor, frozen at commit `bb43e7b`, 74 HTTP calls, SSE)

Non-goals: v2 protocol migration, desktop/web/share/console, GitHub/PR, multi-user hosting,
data migration beyond the 20-session import (§9).

### Standards traceability (functional)

| # | Standard | Where enforced |
|---|---|---|
| F1 | Drop-in: both frozen clients work with zero modifications | §3 freeze; §6 harness kill criteria; milestones |
| F2 | opencode plugin compat (3 live plugins) + MCP compat (3 servers) | §10, quickjs gate with sidecar fallback |
| F3 | Import 20 most recent sessions; original DB never written | §9; `STORAGE.md §5` |
| F4 | Pure Rust request path (no Node/Bun/TS in it) | §2 architecture; sidecar only as declared plugin fallback |
| F5 | Low maintenance vs upstream drift (no per-change code churn) | §8 contract pipeline |
| F6 | v1 wire compatibility only (v2 is reference, not target) | §3; §11 gap table |

### Standards traceability (non-functional)

| # | Standard | Where enforced |
|---|---|---|
| N1 | RSS < 300 MB steady / 24 h; zero swap growth; import peak bounded | `MEMORY.md` budget + CI gates |
| N2 | SQLite tuned day one; no query > 1 s; p95 session list < 50 ms | `STORAGE.md` pragmas + §7 KPIs + CI gates |
| N3 | Fail-fast at boot and at runtime; every KPI is a metric; all magic numbers declared | `SRE.md §1-3` |
| N4 | Vicious CPU optimization per architecture — measured first, specialized second | `SRE.md §4` |
| N5 | Deterministic compatibility: byte-golden SSE/HTTP replay, 0 diff | §6 harness |
| N6 | Auth + permissions identical to recorded freeze; no unauthenticated diagnostics | §6 C3, `SRE.md §1` |
| N7 | Full ISTQB-aligned testing: every requirement traced to a test; no testing theater; standing adversarial program (fuzz/property/mutation/chaos) with negative-control rule | `TESTING.md` (matrix + §1 anti-theater rules + §6 adversarial) |

## 2. Architecture

```
                    ┌──────────────────────── ocserve (single static binary) ───────────────────────┐
opencode TUI  ──HTTP──▶ axum router ──▶ api/v1 handlers ──▶ SessionService ──▶ AgentRunner ──▶ ProviderClient
oc-remote     ──HTTP──▶   │  auth       │  REST + SSE      │  EventBus      │  tools, perms   │  (reqwest, SSE parse)
                         │ (freeze C3)  │                  │  Store         │                 │
                         └─────────────┴──────────┬───────┴────────┬────────┴─────────────────┘
                                                  │                │
                                     SQLite (WAL, main-DB FTS5)   plugin host (rquickjs + esbuild shims)
                                     + external chunked blob store  MCP client (stdio + StreamableHTTP)
```

Workspace crates (rust ≥1.86, edition 2024; release profile per `SRE.md §5`):

| Crate | Responsibility |
|---|---|
| `ocserve-core` | session domain, inbox admission, agent loop, compaction, event bus, permissions |
| `ocserve-http` | axum router, auth, SSE writer (byte-golden frames), error envelope |
| `ocserve-store` | single-writer store, role-scoped pools, blob store, FTS5, importer |
| `ocserve-llm` | providers (openai-compatible first), auth.json, stream assembly + fuzz harness |
| `ocserve-tools` | bash, read, write, edit, glob, grep, webfetch, task, question, todowrite |
| `ocserve-mcp` | MCP stdio + StreamableHTTP + legacy SSE client, OAuth token store |
| `ocserve-plugin` | rquickjs host + shims; Bun sidecar fallback protocol |
| `ocserve-importer` | streaming 20-session read-only importer |
| `ocserve` | `serve`, `import`, `doctor`, `bench` |

## 3. Contract freeze (before any endpoint code)

| Artifact | Freeze | Verification |
|---|---|---|
| Upstream server | opencode **1.18.31** (Homebrew, the version serving today) | `/global/health` → `{"healthy":true,"version":"1.18.31"}` (probed live) |
| TUI | same Homebrew 1.18.31 | attach against ocserve in CI |
| oc-remote | commit `bb43e7b` (v1.9.0 tree) | instrumented build in CI |
| Wire surface | 162 paths in v1 tree `packages/sdk/openapi.json` ∩ 74 oc-remote calls; TUI-only `/tui/*` hit-set **captured** behind proxy at M1 (not assumed) | mechanical diff report |
| SSE wire | raw bytes + headers: **no `id:`, no `retry:`**, `server.connected` then 10 s JSON `server.heartbeat` (first tick dropped), exact header set (`Cache-Control: no-cache, no-transform`, `X-Accel-Buffering: no`, `X-Content-Type-Options: nosniff`), `server.instance.disposed` on teardown | byte-golden capture |
| Auth mode | **recorded from live instance**: `/config` answered 200 with no `Authorization` → freeze auth mode = `off`; ocserve supports `auth=off\|basic`, default off, replay tests both | capture across all 74 calls + SSE handshake |
| Plugins | hook usage of 3 live plugins **plus** full upstream hook-name table (implemented / skipped+logged / doctor-fatal) — plugins update independently of opencode releases | static scan of installed bundles vs dispatcher table |

Anything outside the freeze is out of MVP scope; upstream releases are adopted deliberately (§8).

## 4. HTTP/SSE API surface

**P0 (MVP):** health, config, agent, command, provider, models, project(+current), path, app;
session CRUD + status/search/children/todo/messages/prompt/command/abort/summarize;
permission + question replies; `GET /event` and `GET /global/event`; file/find/file+symbol.

**P1 (TUI attach + extras):** lsp, formatter, mcp, skill, diff, revert/unrevert/share,
nine `/tui/*` control endpoints (promoted to P0 if M1 capture shows attach needs them).

**P2 (deferred):** acp, `/sync/*`, `/experimental/workspace*`, `/api/*`, `/control-plane`,
pty, workspace adapters.

Error envelope + status codes byte-match upstream (oc-remote's Ktor deserializers are strict).

**SSE semantics (contract, not implementation detail):**
- Frames exactly as freeze §3 — no ids, no retry hints, heartbeat cadence as above.
- Per-subscriber bounded ring (`sse_ring_events`, default 4096). **Overflow policy:
  disconnect the subscriber with clean teardown; client recovers via REST snapshot on
  reconnect** (upstream buffers unboundedly — we must not; the divergence is deliberate and
  covered by a harness test: stall a client 30 s during a 5 MB burst, reconnect, diff UI
  state via REST).
- Backpressure: never block the event bus on one subscriber; slow clients never grow RSS.
- Abort/interrupt semantics replay-tested mid-stream.

## 5. Storage (summary — full spec in STORAGE.md)

Measured pathologies that must become impossible: 33 GB DB, 21 GB freelist, 888 MB WAL,
122 MB single `part.data`, 2.2 M-row unbounded `event`, 8–21 s scans, 2 min aggregations.

- **Indexed tables = fixed-size metadata only**; payloads live in an external content-addressed
  blob store (≤1 MB zstd chunks, fsync+rename-then-commit protocol, boot orphan GC).
- **FTS5 external-content lives in the main DB** over a slim `search_doc` projection, synced in
  the writer transaction. *(Empirically corrected: attached-file FTS5 sync is impossible —
  content-name qualification breaks and triggers can't write across ATTACH; contentless FTS5
  fails to even construct on the box's SQLite 3.53.0 — see STORAGE.md §1 test log.)*
- **Role-scoped caches, aggregate ≤32 MB** (writer 16, 4 readers × 4) — the original
  256 MB-per-connection figure was physically incompatible with the 300 MB budget.
- Version gate: rusqlite ≥0.40.2 `bundled` + boot assert `sqlite_version() ≥ 3.51.3`
  (WAL-reset corruption fix).
- Single writer, batched ≤50 ms txns, **no connection checkout across `.await`** (WAL-starvation
  rule), opportunistic PASSIVE checkpoints, idle-only TRUNCATE, `wal_bytes` alarm at 128 MB.
- Bounded event ring (200 k/session, 30 d) in SQLite — never RAM.

## 6. Compatibility harness (milestone 0 — built before endpoints)

Differential testing (McKeeman 1998; Roseau ICSME 2025):

1. **Record** upstream 1.18.31: all REST pairs via logging proxy, **raw SSE frames + headers
   byte-golden**, auth presence per call, TUI attach endpoint hit-set, logcat/screenshot flows.
2. **Replay** against ocserve: REST schema/status equality; SSE byte equality where deterministic,
   normalized sequence equality where timestamps intervene.
3. **Golden corpus**: fixture copy of the live DB containing exactly the 20 imported sessions.
4. **Kill criteria (all green before MVP ships):**
   - every P0 route: schema + status identical across corpus
   - SSE: byte-golden first frames; 40 event types sequence-identical; no `id:`/`retry:` ever;
     10 s heartbeat cadence (first tick dropped); abort mid-stream identical
   - 5 MB synthetic tool output: converges via deltas + REST, zero dropped frames
   - slow-client stall 30 s → disconnect → REST-reconcile → UI state diff = 0
   - TUI attach flows 100%; oc-remote nine flows 100%
   - provider stream fuzz: 10 k seeded chunk-boundary splits of recorded streams → assembled
     output byte-matches upstream
5. **Ongoing**: every PR replays corpus against ocserve vs upstream container; nightly replays
   against **latest** upstream release (triage report — adopt/ignore, never auto-merge) —
   **implemented by `scripts/drift-watch.sh`** (freeze control ×2 + latest npm arm, same
   config mounts, warmup-replayed first, drift/noise/anomaly set classification; nondeter-
   ministic control aborts the run so drift is never claimed from noise);
   kill criterion for the whole strategy: if >20% of divergences found are unrecordable by
   the harness, shift to contract-by-probe.

## 7. KPIs and verification

| Metric | Target | Method |
|---|---|---|
| RSS steady (24 h, 10 sessions) | <300 MB, slope <1 MB/h after h1 | soak gate (`MEMORY.md §5`) + systemd `MemoryMax=300M`, `MemorySwapMax=0` |
| Import RSS (post-idle) | ≤ steady budget; `rss_peak_bytes` reported | import fixture incl. 122 MB row |
| Session list p95 | <50 ms (fail >60 ms) | ≥1000 samples, pinned runner class |
| Any query | **max** <1 s (max tracked, not p99) | `query_duration_seconds` max gauge + CI |
| Full scans | none on tables >10 k rows | EXPLAIN audit gate |
| Checkpoint stall | <250 ms; `wal_bytes` <128 MB | soak metrics |
| Cold start | <500 ms | hyperfine |
| SSE replay diff | 0 bytes | nightly byte-golden |
| Swap growth | 0 | soak gate |
| Binary size | ≤ established ceiling | size.yml (reliary8 pattern) |

Token-layer optimizations (verified literature): byte-stable system+tool prefix for provider
prompt caches; attention-sink-stable session head across compaction; critical context at
prompt edges; tool-output compression before compaction; durable event log + checkpoints for
recovery. **No savings percentages are claimed until the benchmark harness measures them.**

## 8. Low-maintenance pipeline

1. Contract extracted mechanically from frozen binary (record, don't hand-write).
2. Generated types (progenitor) — regeneration never touches handler bodies.
3. Differential CI per PR (upstream container); nightly drift watch on latest upstream.
4. Upgrade runbook: bump freeze → regen → compile lists touchpoints → re-record → replay.
   Target <1 day/upstream minor.
5. Adapters over forks: provider quirks in data, event mapping in one table, hook dispatcher
   with full name table (unimplemented hooks = logged no-op + doctor warning, not crash).
6. MCP OAuth: token store on disk (mode 600), refresh-before-expiry with skew, single-flight
   refresh; mock-server test with 6-minute tokens under 20-minute session.

**Adoption posture on major upstream lines (recorded 2026-10-03, deep dive evidence below):**

- **v1 freeze remains the contract.** `PLAN.md §3` wire/API/plugin surfaces are pinned to
  1.18.31; upstream v1 patch/minor releases (1.18.32–34) are watched via the nightly byte-
  golden container replay — **implemented by `scripts/drift-watch.sh`** (dual-arm: freeze
  control image replayed ×2 for determinism, npm `latest` arm, drift = set difference of
  failure sets; selftest wired as `check-guards` rule 5; reports under `bench/drift/`,
  gitignored); any change to a live freeze artifact (§3 tables) is its own
  `test(corpus): …` commit. This keeps the maintenance load at the "30–60 min per minor"
  level already designed for.
- **v2 is gated, not chased.** The v2 line (v2.0.*) is recorded as *pre-release* in our
  workspace: intentionally breaks plugins, server API contracts, and TUI config (upstream's
  own docs: "V1 plugin implementations do not run in V2"). Benefits v2 actually gives us —
  domain-scoped hook registrations with per-provider scoping (`ModelHookOptions.providerID`),
  native `http.request`/`http.response`/`experimental.ws.*` hooks, session request kinds
  (`primary|compaction|title|generate`) including a native `compaction` hook slot, per-session
  `title`/`generate` hooks — are tracked as the **future plugin-parity surface**, not an
  emergency upgrade. Ocserve's oc-remote primary client is v1-contract; the v2 server API is a
  deliberate divergence we will not mirror early.
- **`compat-pluginsv2` adapter is the bridge strategy.** When a plugin we depend on ships
  v2-only, ocserve implements upstream's own documented bridge shape: accept the v2
  registration surface (`Plugin.define({id, setup(ctx)})` + `ctx.session.hook(...)` +
  `ctx.tool.hook(...)` + `ctx.event.subscribe()` + `ctx.storage`/`options`) and map each
  domain hook to our v1-style dispatch (`hook_mutate` sites). Adapter lives in one file
  (`crates/ocserve-plugin/src/compat_v2.rs` conceptually) so v2 drift is triaged as a mapping
  diff, not scattered edits. Maintenance cost = the shim + one nightly replay column; the
  rest of the pipeline (freeze, matrix, guards) is unchanged.
- **Explicit non-goals at the v2 boundary** (recorded so they don't creep): mirroring v2's
  breaking route tree before a tagged stable release; adopting the Effect-native plugin host
  architecture (our Node/Bun sidecar stays a v1-semantic pragmatic clone); writing v2 plugins
  ourselves for the plugins we run (magic-context/codex-auth/context-mode must ship the v2
  form; our adapter accepts it).
- **Trigger for v2 adoption work**: a plugin in `{magic-context, codex-auth, context-mode}`
  declares a minimum `@opencode/plugin` version with no v1 entrypoint preserved *and* a v2
  stable tag exists on npm (not just the `v2` branch pre-release tags). Until then, the
  nightly watch on `latest` (v1) + version-tagged replay on `v2` stays read-only triage.

(Deep-dive evidence for the posture: `~/src/opencode-v1-v2-diff.md` at v1.18.34/v2.0.21;
v2 migration guide at `opencode.ai/v2/docs/build/plugins/migrate-v1`; v2 hook surfaces from
`packages/plugin/src/effect/session.ts` and provider plugins under `packages/core/src/plugin/
provider/`.)

## 9. Import (20 sessions, original untouched)

- Source opened read-only (`mode=ro`, `query_only`), never read-write; no `NO_MUTEX`
  (multi-thread unsafe flag dropped); verify original untouched via mtime + `data_version`.
- **Chunked read transactions** released between batches (one long read txn would pin
  upstream's 888 MB WAL and stall the live server); abort guard if source WAL grows >500 MB.
- 122 MB row: `substr` 4 MB windows pumped straight into the zstd encoder — never parsed
  whole (stream deserializers materialize a single large value); single-allocation cap 8 MB;
  verification by incremental canonical hash, not `Value==Value`.
- External `tool-output` refs: per-ref manifest (copied/missing) surfaced in `doctor`; missing
  files warn, never fail import silently.
- Finalize: tmp build → `wal_checkpoint(TRUNCATE)` → atomic rename (stria pattern).
- Acceptance: peak post-idle RSS within budget, hash equality, blob hash check via `doctor`.

## 10. Plugins + MCP (compatibility gates)

Verified hook usage: context-mode (`tool.execute.before/after`, `experimental.session.compacting`),
magic-context (`chat.message`, `tool.execute.after`, `experimental.chat.messages.transform`,
`experimental.chat.system.transform`, `experimental.text.complete`), reliary8 (`tool.execute.after`).
Auth plugins: out (user decision). Dispatcher nevertheless carries the full upstream name table (§3).

1. ~~Install time: esbuild bundle per plugin~~ — **superseded by gate result below**.
2. ~~rquickjs isolate per plugin~~ — **quickjs rejected at the gate on measured evidence**
   (import audit of the frozen artifacts: `bun:sqlite`, `child_process`, `node:fs/crypto/url`,
   better-sqlite3/bun-branch detection, TTY UI modules → a dozen shims plus a *synchronous*
   SQLite bridge = deadlock-prone). Rejected before any rquickjs code was written.
3. State under `~/.local/share/ocserve/plugin/{name}/`.
4. **Gate result (M4b, 2026-10)**: Node v25 sidecar (bun is not installed on this host;
   Node is, with `node:sqlite`) — NDJSON JSON-RPC over stdio, host files materialized next
   to the data dir, plugin chatter forced to stderr so framing cannot corrupt, RPC deadline
   30 s, declared *outside* ocserve's 300 MB budget with child RSS scraped into metrics
   (≤80 MB target). Extraction chain ported from `readV1Plugin`+`getLegacyPlugins`
   (PluginModule.server → default object.server → default fn → named fn exports); hook
   semantics = v1 sequential `fn(input, output)` mutation; `bun:sqlite` satisfied by a
   node:sqlite shim on both the ESM import path (module.register resolver) and the CJS
   require path (`Module._resolveFilename`), with a truthy `globalThis.Bun` marker so
   plugins take their bun branches. Falsifiable acceptance: synthetic-plugin roundtrip
   tests + live: 4/5 configured plugins load (gemini-auth ships a raw `.ts` entry —
   Bun-only, user-declared unused, fails with a clear error), and
   `trigger tool.execute.after: 3 hook(s)` proven in the server log on a real write prompt.
   Remaining gate items: recorded-transcript behavioral parity + restart-state preservation
   (M5 soak).
   preserves state; else Bun sidecar fallback (JSON-RPC over unix socket, ≤80 MB, declared
   outside the 300 MB budget but scraped into metrics).
5. MCP: stdio + StreamableHTTP + legacy SSE; tools/resources/prompts; change notifications;
   OAuth per spec; upstream namespacing `{server}_{tool}`; all 3 servers survive restart.

## 11. v1→v2 gaps and ocserve's position

| v2 improvement | Client-visible? | Decision |
|---|---|---|
| Durable inbox (steer/queue admission) | no | adopt |
| Write-ahead execution claims + restart recovery | no | adopt |
| Instruction deltas (hashes, epochs) | no | adopt |
| Tool-call durability before side effects | no | adopt |
| Bounded event log w/ sequence | no | adopt + bound (upstream's grew to 2.2 M rows) |
| Provider retry classification (overflow/rate/incomplete) | no | adopt |
| Location/instance multi-project | no (v1 = one project/process) | partial: match v1 |
| Tool renames (`bash`→`shell`, `task`→`subagent`, …), CodeMode | **yes** | **reject** — keep v1 names |
| Rich `session.next.*` beyond frozen decode set | **yes** | emit only freeze set; richer set documented as future opt-in |

Beyond both versions: no whole-row JSON, no unbounded events, no scan queries, role-scoped
cache ≤32 MB, `auto_vacuum` from birth, mechanical contract, SRE instrumentation.

## 12. Verified references

| Work | Venue/Year | ID |
|---|---|---|
| SQLite: Past, Present, and Future | PVLDB 15(12) 2022 | DOI 10.14778/3554821.3554842 |
| Are You Sure You Want to Use MMAP…? | CIDR 2022 | cidrdb.org/cidr2022/papers/p13-crotty.pdf |
| LSM-Tree | Acta Informatica 1996 | DOI 10.1007/s002360050048 |
| WiscKey | FAST 2016 | usenix.org/conference/fast16/…/lu |
| Differential Testing for Software | DTJ 1998 | dblp.org/rec/journals/dtj/McKeeman98 |
| Zstandard | RFC 8878 | rfc-editor.org/rfc/rfc8878 |
| SQLite docs (auto_vacuum/WAL/optimize/FTS5) | 2026-08 | sqlite.org/pragma.html, wal.html |
| Refactoring impact on client-used APIs | IST 2017 | arXiv:1709.09474 |
| Roseau (API breaking changes) | ICSME 2025 | arXiv:2507.17369 |
| Incremental View Maintenance | PODS Gems 2024 | arXiv:2404.17679 |
| MemGPT | 2023 | arXiv:2310.08560 |
| StreamingLLM (attention sinks) | ICLR 2024 | arXiv:2309.17453 |
| vLLM/PagedAttention | SOSP 2023 | arXiv:2309.06180 |
| SGLang/RadixAttention | 2023 | arXiv:2312.07104 |
| Prompt Cache | MLSys 2024 | arXiv:2311.04934 |
| LLMLingua | EMNLP 2023 | arXiv:2310.05736 |
| Lost in the Middle | TACL 2023 | arXiv:2307.03172 |
| ReAct | ICLR 2023 | arXiv:2210.03629 |
| Context Engineering survey | 2025 | arXiv:2507.13334 |
| Durable Functions + Netherite | 2021 | arXiv:2103.00033 |

All re-verified by fetching sources this session; LSM-Tree DOI corrected during verification.

## 13. Antagonism ledger (two passes; all findings dispositioned)

**Pass 1 (2 fatal, 9 serious, 3 minor)** → resolutions: contract freeze (F1), quickjs+sidecar
gate (F2), byte-golden harness (S1/S2), streaming importer (S3), snapshot fidelity test (S4),
mechanical contract (S5), provider record/replay (S6), MCP scope (S7), auth (S8), store split
(S9), plugin paths (M1), `{id}` quirk retired (M2), streaming invariants (M3).

**Pass 2 (attack + empirical testing, 2026-10-01):**

| Finding | Severity | Disposition |
|---|---|---|
| A1 attached-file external-content FTS5 broken | fatal | **Confirmed by test** → FTS5 in main DB (§5, STORAGE.md §1) |
| A2 cross-file WAL non-atomic | fatal | Moot after A1 fix (single file) |
| A3 256 MB cache × pool ≻ 300 MB budget | fatal | Role-scoped caches, aggregate ≤32 MB (`MEMORY.md §1`) |
| A4 `temp_store=MEMORY` unbounded | serious | `temp_store=FILE` + SQLITE_TMPDIR (STORAGE.md §2) |
| A5 connection-across-await starves WAL | serious | Hard rule + CI lint + `wal_bytes` alarm |
| A6 backup omits blobs/FTS | serious | `VACUUM INTO` + generation manifest + restore drill |
| A7 rusqlite WAL-reset version gate | serious | ≥0.40.2 bundled + boot assert ≥3.51.3 |
| A8/A9 pragma preamble/freelist | minor | Shared `open()` routine; idle `incremental_vacuum` |
| B1 ring must be SQLite not RAM | fatal | Bounded ring in SQLite, declared in §5 |
| B2 import peak measured pre-purge | serious | Post-idle measurement (MEMORY.md §5) |
| B3 sidecar outside budget | minor | Declared boundary + metrics scrape |
| C1 SSE bytes/headers/timing | serious | Byte-golden capture (§3, §6) |
| C2 slow-client unbounded upstream vs our bound | serious | Disconnect + REST reconcile policy, harness-tested (§4) |
| C3 auth mode freeze artifact | serious | Recorded: live is passwordless → `auth=off` default, both modes replayed |
| C4 TUI hit-set assumed | minor | Captured behind proxy at M1, promoted if needed |
| D1 hook surface drift (plugins update independently) | serious | Full name table + static scan + doctor |
| D2 MCP OAuth | serious | Token store + skew refresh + single-flight + mock test |
| D3 TUI upgrades anyway | serious | Nightly latest-upstream replay, triage report |
| D4 provider stream edge cases | minor | 10 k seeded chunk-split fuzz (§6) |
| E1 import pins upstream WAL | serious | Chunked read txns + 500 MB abort guard |
| E2 122 MB row materialization | serious | `substr` window pump, 8 MB alloc cap, hash verify |
| E3/E4 missing refs / NO_MUTEX | minor | Manifest warn; flag dropped |
| F1 fail-fast inventory | serious | `SRE.md §1` |
| F2 metrics undefined | serious | `SRE.md §2` (every KPI = series) |
| F3 tunability | minor | `SRE.md §3` (typed config, unknown = fatal) |
| F4 per-arch CPU | minor | `SRE.md §4` (measure-first; native profile opt-in) |
| F5 CI perf gates | serious | `SRE.md §5` + §7 |

**Cheapest decisive experiments (M0 pre-work, ~1 day):** rusqlite bundled version assert;
pool+cache under `MemoryMax=300M` with 100 k-row fixture; SIGKILL fuzz of blob/DB protocol;
byte-golden capture of live `/global/event`; contentless-FTS retest under bundled build
(box's 3.53.0 fails it — `STORAGE.md §1`).

## 14. Milestones (acceptance-gated; entry/exit criteria per `TESTING.md §8`)

| # | Milestone | Acceptance |
|---|---|---|
| 0 | Freeze capture + contract + harness + decisive experiments | upstream passes corpus against itself; experiments green; traceability matrix skeleton exists |
| 1 | Skeleton: health/config/sessions + byte-golden SSE; `/metrics`, `doctor` boot checks | oc-remote renders list; TUI attach opens; fail-fast tests pass; SSE auth decision table covered |
| 2 | Agent loop: prompt → stream → tools → permissions; provider fuzz harness | transcript replay matches; provider fuzz green; state-transition suite green |
| 3 | Store + importer + FTS + all storage gates | import gates green; p95 <50 ms; full-scan detector clean; mutation threshold met |
| 4 | Plugins (quickjs) + MCP + OAuth | 3+3 pass; restart preserves state; sidecar gate decided; OAuth expiry test green |
| 5 | Soak + hardening | every §7 row green over 24 h; systemd hardening live; fault + security suites green; rollback drill done |
| 6 | Auto-compaction (`COMPACTION.md` — design reviewed before code, §10 gate) | COMPACTION §9 suite green: stub overflow e2e, cap-3, P1/P7 equivalence, P3 atomicity, no-inflation, classifier +±, serialize golden, hook exactly-once with negative twins; K-SUMMARIZE re-verified byte `true`; K-COMPACTION green |

## 15. Open questions (originally "decide before M1" — statuses updated 2026-10-03)

1. Plugin fallback boundary (B3): **ANSWERED** — sidecar RSS measured 121–151 MB under a
   160 MB cap (MEMORY provenance; heap default 128 via `OCSERVE_PLUGIN_HEAP_MB`).
2. Snapshot/revert for imported sessions: **OUT** — snapshot/git-revert engine is out of
   MVP (§2/§17); re-affirmed with evidence 2026-10-03 (session diff depends on it;
   oc-remote's REST diff caller is dead code). Reopen only as its own project.
3. Network: **ANSWERED** — socat tailnet forwarder shipped (`ocserve-tailscale-forward.service`).
4. `native` CPU profile: **STILL OPEN** — portable default ships; decide if a native-tuned
   laptop build is worth the artifact matrix.
5. Service cutover: **STILL OPEN, pending readiness report** — recommended shape unchanged
   (`ocserve.service` takes 4901, Homebrew opencode stays installed as instant rollback);
   gate on the migration-readiness checklist (T5) before scheduling.

### 15.5 Migration readiness snapshot (2026-10-03 — gates BEFORE cutover)

| Item | Status | Evidence |
|---|---|---|
| Legacy delta-sync lag | **green** | `ocserve_sync_last_age` = 33 s (≤1×60 s tick), 198 msgs adopted, error counter never emitted |
| Route table (post-disposition pass) | **green** | replay vs live **26/0** (5 honest defers printed); §17 rows all evidence-linked; K-OCREMOTE green |
| Plugin stack | **green** | P0 battery + post-fix live run (sidecar RSS 185 MB under CPU storm vs 121 MB calm baseline — watch, under 160 MB boundary) |
|24 h soak gate | **pending** | fresh sampler started 2026-10-03 14:51 (systemd timer, `~/.local/state/ocserve/soak/soak.csv`); gate = `scripts/soak-gate.sh` after 24 h — earliest cutover decision +24 h |
| Memory vs 750 M cap | **ok, tight high-water** | anon RSS 367 MB; cgroup current 632 MB / peak 740 MiB of 750 MiB — peak = page cache during the mutants+tests storm (reclaimable), zero swap |
| Backup drill | **green** | nightly6/6 (VACUUM INTO integrity=ok, msg counts equal) |
| oc-remote holes | **green** | none left unclassified; dead diff caller documented; SSE stub honest |
| Armory (timers/watch) | **green** | soak/drift/nightly timers enabled (Persistent); pre-commit hook live; first mutants run grinding (529) |
| Legacy untouched | **green** | :4901 untouched, forwarder active, ocserve = only writer of its own DB |

**Cutover preconditions:** soak-gate PASS (+24 h) → then §15.5 re-run → your go on §15.5 cutover shape.

## 16. Immediate stopgap (independent)

Live 33 GB DB reclaim while ocserve is built: `VACUUM` (~21 GB back), rotate 577 MB logs, WAL
checkpoint, raise `cache_size`, `swappiness` 150 → 10. Optional; not part of ocserve.

## 16.5. Client behavior discovered in the field (2026-10-02, oc-remote)

**Cross-server SSE fusion**: `OpenCodeConnectionService` keeps an SSE connection to
**every saved server simultaneously** ("Maintains persistent SSE connections to one or
more servers"), all feeding **one `EventReducer` whose message/part maps are keyed by
`sessionId` only** (`EventReducer.kt:81,144` — `serverId` tracks session *ownership*,
never content; `ChatViewModel.kt:602-606` combines those flows without serverId
filtering). Consequence: a session that exists on two servers (imported copy + live
source) displays live events from **both** SSE streams in either server's view — the
user watched a legacy session stream live while the bound REST connection (ocserve)
returned the frozen import snapshot. REST requests follow the bound connection; live
content can come from the other. Implication for the delta bridge: REST freshness (K-SYNC)
makes the fusion consistent instead of contradictory; until then, sends from a view write
to that view's server and the two DBs diverge underneath a unified display.

## 16.7. `POST /session/search` contract (W1 — fork client codes against this)

```
POST /session/search       Content-Type: application/json
{ "query": string          // required, trimmed, 1..=512 chars
  "sessionID"?: string     // optional scope
  "limit"?: int            // default 50, clamped 1..=200
  "offset"?: int }         // default 0
→ 200 { "hits": [ { "sessionID", "messageID", "partID",
                    "role", "time", "snippet" } ], "truncated": bool }
→ 400 {"name":"BadRequest","data":{"message":"query must be ..."}}  (empty/oversized)
```
Semantics: case-insensitive **substring** (trigram ≥3 chars, LIKE ≤2 — identical
results either way); matches part payload JSON text; blobbed parts included;
`truncated=true` when more hits remain past limit+offset; snippet = ±80 chars
around the first match with ellipses; hits ordered by message time DESC.

## 17. oc-remote route coverage (exhaustive client audit)

Source: static extraction of all58 route templates + call graph from
`~/src/oc-remote` (`OpenCodeApi.kt` + callers), diffed against ocserve's router.
Status codes: **live** = implemented + tested this session; **probe-by-design** =
the client probes for MiMoCode extensions; vanilla opencode also 404s, so our
404 *is* freeze behavior; **out** = explicitly out of MVP scope with reason;
**batch N** = planned follow-up batches (not yet implemented).

| Route (oc-remote) | Method | Status |
|---|---|---|
| `/global/health` `/global/event` `/path` `/project` `/project/current` `/project/{id}/directories` `/agent` `/command` `/config` `/config/providers` `/provider` `/session` `/session/{id}` `/session/{id}/message` `/session/{id}/prompt_async` `/session/status` `/experimental/session` `/experimental/workspace` `/mcp` `/permission` `/permission/{id}/reply` | GET/POST/… | **live** (M0–M5 core) |
| `/session/{id}/children` `/session/{id}/todo` `/session/{id}/abort`, `DELETE /session/{id}`, `PATCH /session/{id}` | GET/POST/DELETE/PATCH | **live** (Batch 1) |
| `DELETE/PATCH /session/{id}/message/{mid}`, `DELETE/PATCH …/part/{pid}` | DELETE/PATCH | **live** (Batch 1) |
| `/file`, `/file/content`, `/find/file`, `/find` | GET | **live** (Batch 2: scope-guarded fs reads, bounded walk, single-spawn grep) |
| `/question`, `/question/{id}/reply|reject` | GET/POST | **live** — full question tool flow: QuestionGate rendezvous, asked/replied/rejected events, v1 output formatting, oc-remote empty-body reject; live tailnet E2E (`GOT=Blue`) |
| `/session/{id}/task`, `/session/{id}/actors`, `/bash-interactive`, `/workflows` | GET | **probe-by-design** — no v1 route exists upstream either (verified in v1 httpapi groups); client tolerates the absence |
| `/session/{id}/command` `/session/{id}/shell` | POST | **live** — command = v1 template expansion ($N/$ARGUMENTS/append rules) + session.error SSE on unknown; shell = direct bash exec (no model), synthetic-user + assistant/bash messages, cost 0 |
| `/session/{id}/summarize` | POST | **live (W3)** — probe bytes: 200 `true`; probe shape: empty user marker (parts=[]) with last-user agent + summarize model, assistant summary follows; port = one transient text-only turn (ocserve-core::compact = v1 buildPrompt+serialize verbatim), sync, try_lock→409; divergences: no compaction-state/history filter, no info.summary marker (TESTING §1.6) |
| `PATCH /config` `PATCH /global/config` | PATCH | **live (W4)** — echo payload, shared-file default (overlay via env), reloader swap; see K-CONFIG |
| `POST /mcp/{name}/connect\|disconnect` `DELETE /mcp/{name}/auth` `PUT/DELETE /auth/{providerID}` `GET /provider/auth` `POST /global/dispose` | mixed | **live (W5)** — see K-ADMIN; auth writes ocserve overlay; GET mcp auth/callback routes intentionally absent (no OAuth servers → SPA HTML, documented) |
| `GET /event` | GET | **live (2026-10-03)** — alias of `/global/event` (upstream serves `/event`,`/global/event`,`/api/event` from ONE handler, public.ts:155); golden `event_alias_is_byte_identical_to_global_event` |
| `/session/{id}/diff` | GET | **out — documented with evidence (2026-10-03)**: (a) oc-remote's `getSessionDiff` (OpenCodeApi.kt:453) has **zero callers** — dead client code; (b) the route is `summary.diff` off the **snapshot/revert engine** (handlers/session.ts:99, `info.revert.snapshot/diff`) which §2/§17 declare out of MVP; (c) the SSE path already works: ocserve emits `session.diff` with `diff:[]` (prompt.rs:1227, honest stub; EventReducer path tested). Reopen only with the snapshot engine as its own project |
| `/skill`, `/find/symbol`, `/file/status`, `/vcs`, `/vcs/status`, `/vcs/diff`, `/vcs/diff/raw`, `/vcs/apply` | GET/POST | **out (2026-10-03, evidence)** — zero oc-remote callers (grep of OpenCodeApi.kt = 0 hits; client calls only `/find/file`, already live); TUI-tolerated (no M1 attach hit); `/vcs/apply` additionally write-path (would need heal-loop integration, AGENTS §2.5) |
| file watcher / `file.watched` events | — | **parked (2026-10-03, evidence)** — zero consumers in either client (oc-remote: no `file.*` event handling anywhere; ocserve bus unbounded-watched upstream). Reopen on TUI hit-set evidence or a consumer request |
| manifest defer dispositions (audit 2026-10-03) | — | `mcp` **flipped live** — re-recorded as connected-state `keys_subset` (status-keys only: improvement-safe, missing status = health-regression signal; M0 record captured pre-PATH-fix error state); `provider_auth` defer relabeled **`divergence:K-ADMIN-api-key-only`** (route live, shape diverges by design); `api_model|provider|skill|integration` relabeled **`out`** (M2 milestone labels were stale — never shipped, TUI-tolerated, not oc-remote). Replay after audit: **26 pass / 0 fail** |
| `/pty` `/pty/{id}` (POST/PUT/DELETE), `/provider/{id}/oauth/*` | mixed | **out** — PTY host and provider OAuth are separate features (never in MVP scope); UI probes tolerate 404 |
| `/session/{id}/share` (POST/DELETE), `/revert` `/unrevert` | POST | **out** — share/snapshot/git-revert infra explicitly out of MVP (PLAN §2); 404 tolerated by client |
| `/session/{id}/fork` | POST | **live** (K-FORK) — freeze session.fork ported (session.ts:691). The earlier "out, 404 tolerated" row was STALE: oc-remote defines `forkSession` (OpenCodeApi.kt:551) with 3 UI call sites (ChatScreen.kt:2074/2298/2552 → ChatViewModel.kt:2284) and shows a "fork failed" snackbar on 404 — a dead feature, not a tolerated probe |
| `/api/session/{id}/agent|model|prompt` | POST | **out** — v2-only endpoints, unused by oc-remote against v1 servers (call graph: UNUSED) |
| `/experimental/workspace` `/project/{id}/directories` | GET | live; call graph UNUSED by client but served (freeze) |
| `/session/{id}/message` (collection DELETE) | — | not a route: the client's second `.delete` was temp-file cleanup (static call graph false positive) |
