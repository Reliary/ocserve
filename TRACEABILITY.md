# refine — Traceability matrix (TESTING §2)

Checked by `scripts/check-matrix.sh`: every row resolves to at least one test ID
that exists in the codebase (`cargo test -- --list`), and every F/N standard +
PLAN kill criterion has a row. Orphan test IDs (tests referencing unknown rows)
fail the check. Updated in the same branch as the behavior it covers.

Status values: **green** (test exists and passes), **partial** (test exists,
coverage incomplete), **planned** (milestone-gated, no test yet — must name its
milestone).

## Functional standards (PLAN §1)

| Req | Requirement | Tests | Status |
|---|---|---|---|
| F1 | Drop-in: both frozen clients work unchanged (PLAN §3) | `golden::*`, `replay` 24/24 with 7 declared deferrals (M2/M4), live TUI attach 12 s zero-error run, `auth_metrics::*` | partial — E2E prompt stream works live (5/5 exact, deepseek); oc-remote device run still pending |
| F2 | Plugin + MCP compatibility (3+3) | — | planned — M4 (TESTING §8) |
| F3 | Import 20 sessions, original untouched (PLAN §9) | metadata pass live-tested (20 rows, ro+query_only, original untouched); payload streaming + hash equality — M3 | partial |
| F4 | Pure Rust request path (PLAN §2) | (architecture; no runtime Node/Bun in workspace deps — `cargo tree` audit planned M2) | partial |
| F5 | Low maintenance vs upstream drift (PLAN §8) | `replay::replay_all` self-test (9/9 vs upstream 1.18.31) | partial — drift watch nightly is M5 |
| F6 | v1 wire compatibility only (PLAN §11) | `golden::sse_first_frame_matches_upstream`, `golden::notfound_envelope_matches_upstream` | green |

## Non-functional standards (PLAN §1)

| Req | Requirement | Tests | Status |
|---|---|---|---|
| N1 | RSS <300 MB / 24 h; zero swap; bounded import (MEMORY §5) | `cache_fixture::cache_budget_and_100k_row_latency` (budget arithmetic + profile asserts) | partial — full soak gate M5 |
| N2 | SQLite tuned; no query >1 s; p95 list <50 ms (STORAGE §7) | `cache_fixture` (100k rows, EXPLAIN no-bare-SCAN, latency smoke) | partial — release-build p95 in bench job |
| N3 | Fail-fast, metrics, declared knobs (SRE §1-3) | boot checks exercised by `refine doctor` smoke; version-gate tests | partial — metrics endpoint M1, forced-failure tests M5 |
| N4 | Per-arch CPU, measured first (SRE §4) | — | planned — `refine bench --blob` M2 |
| N5 | Byte-golden deterministic compat (PLAN §6) | `golden::*` (health, 404, SSE headers+first frame), negative control `id_normalization_actually_normalizes` | green for captured surface |
| N6 | Auth mode = recorded freeze; no unauthenticated diagnostics (PLAN §3) | — | planned — auth=off is current freeze; decision-table tests M1 |
| N7 | ISTQB testing, anti-theater, adversarial program (TESTING) | matrix checker; negative controls in golden tests; `crash_fuzz` (SIGKILL) | green (process) / partial (coverage grows per milestone) |

## Plan kill criteria (PLAN §6, §7, §13)

| ID | Kill criterion | Tests | Status |
|---|---|---|---|
| K-SSE-BYTES | raw SSE frames+headers byte-golden; no `id:`/`retry:`; 10 s heartbeat | `golden::sse_headers_match_freeze`, `golden::sse_first_frame_matches_upstream` | green (headers, first frame, 10 s heartbeat via paused-time test + live check; durable/sync twin parity 4/2/2 vs capture; bus owns sender → oneshot EOF regression covered) |
| K-ENVELOPE | P0 route schema/status identical to upstream | `golden::health_bytes_match_upstream`, `golden::notfound_envelope_matches_upstream`, `golden::session_status_bytes_match_upstream`, `refine replay --allow-missing` (3 pass/4 skip/2 data-gap) | partial — grows per M1-M3 route work |
| K-CRASH | SIGKILL blob/DB protocol: quick_check, zero dangling refs | `crash_fuzz::sigkill_fuzz_blob_db_protocol` (25 iters CI; 1000 nightly per SRE §5) | green |
| K-CACHE | aggregate page cache ≤32 MB declared+enforced | `cache_fixture` (budget arithmetic, profile asserts) | green |
| K-FTS | FTS5 design viable on shipped build | `fts_m0::external_content_lifecycle_local`, `fts_m0::contentless_fts_status_recorded` | green |
| K-VERSION | bundled SQLite ≥3.51.3 at boot | `pragma::tests::version_gate_passes_on_bundled` + `refine doctor` boot gate | green |
| K-IMPORT | import peak ≤budget; original untouched; hash equality | — | planned — M3 |
| K-SLOWCLIENT | stall 30 s → disconnect → REST reconcile | — | planned — M2 |
| K-PROVIDER | recorded streams replay byte-exact; chunk-boundary buffering | `refine_llm` fixture tests (2 recordings), buffered line parser (live 500 → fixed), stress 5/5 exact | green for fixtures; 10k fuzz — M5 nightly |
| K-TOOLLOOP | multi-step: stream→tool→execute→next turn→final; structure = upstream capture | live E2E ×3 (TOOL_OK/M2B_OK/final DONE), message chain matches tool_fixture (user→tool-calls→stop) | green |
| K-PERM | ask event → client reply → gate → execute; timeout → deny + map cleanup | live flow (ask→reply 200→file written→DONE), 300s-timeout cleanup fix, pending-list session filter | green; permission id prefix evt_ (upstream prefix unverified — noted) |
| K-TRUNC | tool output ≤50KB/2000 lines; bash timeout bounded | `refine-tools` truncation_bounds + bash_timeout_enforced (124, <5s) | green |
| K-IMPORT | streamed payload import: counts parity, byte-equal parts, blob spill >8KB, event verbatim, FTS rebuild; peak RSS <300MB | `import_counts_byte_parity_and_blob_spill` (synthetic source, byte+FTS asserts); live run: 36k msgs/123k parts/461k events/85s, VmHWM **102 MB**, parity 200/200, freelist 0% | green |
| K-PAGE | `?limit=` last-N ascending; `?before=` → 400 {"_tag":"BadRequest"} byte-exact (upstream rejects all values §1090) | `message_paging_limit_returns_last_n_ascending`, `message_before_param_rejects_like_freeze` (red→green TDD); live big-session: limit=50 **12.6ms**, no-limit 93.2MB in **787ms** (upstream same session single-run 17.7s — parity, unpaired) | green |
| K-SEARCH | GET /experimental/session substring-on-title + project embed; "not a content search" | `experimental_session_search_substring_and_project_keys` (incl. non-match = []); manifest keys golden (30 keys) | green |
| K-LATENCY | query gates: count/session/page on16k-msg session | idx_part_session (v4) 86ms→**0.4ms** covering-index plan asserted live; session list p95 2.3ms, metrics p95 4.4ms | green |
| K-RETENTION | ring bound (200k/session + 30d) enforced at boot/import/throttled-append; backup VACUUM INTO + restore drill | `ring_cap_keeps_newest...`, `age_window_drops_old_but_never_unknown_timestamps` (negative controls: no prune → fails), `vacuum_into_backup_restores_clean` (integrity+counts+overwrite-refusal); live boot: 5,148 rows pruned, 0 >30d remain | green |
| K-MCP | MCP client (stdio NDJSON + StreamableHTTP/SSE), handshake 2025-06-18, paginated tools/list, namespaced call; /mcp status shape; prompt-end-to-end | `mcp_client.rs` 4 tests (fake stdio server: handshake,2-page list, isError→Err, statuses+namespace+dispatch fallthrough); live: context7 connected via real HTTP/SSE, /mcp statuses byte-shape match freeze capture (§1146), full prompt cycle: model called context7_resolve-library-id → user `*`-ask permission → reply → tool output → answer | green |
| K-PLUGIN | quickjs gate decision on evidence; Node sidecar: load 3+ plugins, v1 extraction chain, hook dispatch proof, bun:sqlite both module paths | PLAN §10 gate result (docs); `sidecar_load_trigger_roundtrip` (synthetic PluginModule: load→names→mutation→statuses), entry-shape tests; live: 4/5 load (gemini raw-TS entry fails loudly — Bun-only, user-unused), **`trigger tool.execute.after: 3 hook(s)`** on real write prompt, hook errors surface to log | green (recorded-transcript parity + restart-state → M5) |
| K-METRICS | SRE §2 metric-per-KPI set + sidecar RSS boundary (MEMORY provenance rule) | `refine-metrics` 3 tests (bounded route labels incl. negative cases, exposition format, rss peak monotonic); live families verified: http_request_duration(+max), llm_request_duration/ttft, events_emitted{type}, sse_clients/events, rss/peak, sidecar_rss (151MB — MEMORY updated with measured boundary + provenance), wal, writer_queue, mcp_connected, requests; debug-build refine RSS 225MB | green (checkpoint_duration/blob_orphans deferred — no checkpoint hook yet, honest gap noted) |
| K-DEPLOY | systemd unit (MemoryMax/sidecar budget, hardening), release profile build, backup/restore drill | transient unit smoke (health/metrics 200, cgroup 270.9M/480M, swap max 0); real unit installed+enabled, release binary 8.2MB in 5m29s; `backup` drill: 375MB VACUUM INTO, integrity ok, 36,297 msgs, schema 4 — **drill caught readonly-handle bug** (fixed to open_writer, re-run green); doctor green (freelist 0, quick_check ok) | green |
| K-ASYNC | POST /session/{id}/prompt_async: 204 immediate + background run + client messageId honored + NotFound envelope (oc-remote send path) | `prompt_async_returns_204_persists_user_message_and_404s_unknown` (red→green: route absent = the field 404); live via tailnet with oc-remote's exact PromptRequest: 204/empty in 0.00s, reply REMOTE_OK in 2.0s, messageId persisted; per-session prompt locks (bounded active-set, 64 cap → 503) | green |
| K-SOAK |24h sampler: slope <1MB/h after h1, zero swap, health 200s | `scripts/soak.sh` running against the systemd unit (300s interval): first tick rss 227MB / sidecar 142MB / wal 33KB / health 200 | **partial** — sampler running, duration gate evaluated when the CSV completes (24h); no early claims (provenance rule) |
| K-MEMORY-24H | 24 h soak: every sample <300 MB, swap 0, slope <1 MB/h | — | planned — M5 |

## Test design techniques coverage (TESTING §4)

| Technique | Applied | Where |
|---|---|---|
| EP | config knob classes (cache sizes, version parse suffixes) | `pragma::tests::version_parse_handles_suffixes`, cache budget bounds |
| BVA | 100k-row boundary, blob sizes 0/1/1MiB±1/8MB-cap, ring 4096th event | `blob::tests::roundtrip_empty_and_boundary`, `cache_fixture`; SSE ring M2 |
| Decision table | auth mode × route × credential | planned M1 (N6) |
| State transition | session lifecycle incl. crash→claim→resume | planned M2 (session engine) |
| Use case | 9 oc-remote flows, TUI attach | planned M1/M2 (F1) |
| Error guessing | every antagonism-ledger finding | `crash_fuzz` (A2/A6), FTS tests (A1), cache test (A3), golden SSE (C1) |
| White-box branch | blob crash-protocol ordering | `crash_fuzz` + `blob::tests` |
| Mutation (TESTING §1) | cargo-mutants gate | planned — first run M2, threshold after baseline |
| Fuzz | SSE/JSON/provider parsers | planned — cargo-fuzz M2 (corpus seeded from recordings) |
| Property | chunker roundtrip arbitrary sizes | `blob::tests::roundtrip_multi_chunk_above_alloc_cap` + boundary set (proptest crate M2) |
