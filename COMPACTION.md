# COMPACTION.md — auto-compaction (M6): compatibility contract + design

**Status:** design for review — no engine code written. **Precedence:** TESTING.md gates
> PLAN.md > this doc. All upstream claims cite `~/src/opencode-v1` line numbers from the
2026-10-03 read (freeze 1.18.31). Antagonism ledger: §7.

## 1. Scope and contract

**Goal:** context-overflow auto-compaction with **upstream-compatible behavior on every
client-visible surface**, plus explicitly-listed internal performance improvements (§5)
that never change a wire byte.

**Compatibility-first rules:**
1. Responses, events, part shapes, config keys, and hook orderings match upstream unless
   the item appears in §6 divergence ledger.
2. Ocserve's sessions stay **stateless rows** — compaction adds no resident session state
   (MEMORY invariant); all new state is indexed columns.
3. Bounded by construction (AGENTS §2.3): every new buffer (conversation string, scans,
   write batches) declares its bound next to its creation.

## 2. Upstream algorithm (line-cited)

### 2.1 Trigger
- **Post-turn (usage path)** `processor.ts:481-496`: after the assistant message
  completes, if the message is not itself a summary message and `isOverflow()` →
  `ctx.needsCompaction = true`; the stream is cut via `Stream.takeUntil`
  (`processor.ts:658`) and `process` returns `"compact"` (`:693`).
- **Error path** `processor.ts:624-634`: a provider error classified as
  `ContextOverflowError` → if `compaction.auto === false` (and not a summary message)
  hard-fail with the error on the message; else publish a transient `session.error`
  event and set `needsCompaction` (retry-through-compaction).
- **`isOverflow` / `usable`** (`overflow.ts:7-36`): `count = tokens.total ||
  input+output+cache.read+cache.write`; overflows when `count >= usable`, where
  `usable = context − min(20_000, maxOutputTokens)` (or `limit.input − reserved` when
  `limit.input` exists); `auto === false` or `context === 0` → never.
- **Tool-loop doom** (`processor.ts:29,356-365`): three identical completed tool calls
  with identical inputs in the last 3 parts cut the stream — *not* a compaction guard
  (corrected during antagonism A2).

### 2.2 `Result::compact` consumer (prompt loop)
`prompt.ts:1320-1330`: on `"compact"` → `compaction.create({sessionID, agent, model,
auto: true, overflow: !handle.message.finish})`, then `"continue"` — the prompt loop
iterates again on the compacted history. After the loop: `compaction.prune` forked
(`prompt.ts:1344`).

### 2.3 `compaction.create` / `process`
- Builds a **summarize request**: single user message, `system: []`, body =
  `nextPrompt` + optional `"The following is the conversation history:\n\n" +
  conversation` when `cfg.compaction.prompt` (`compaction.ts:430-447`).
- `conversation = msgs.map(serialize).filter(Boolean).join("\n\n")` (`:380`).
- **`serialize` format** (`compaction.ts:54-92`) — byte format ocserve must reproduce:
  `[User]: text` + `[Attached mime: filename]` lines; assistant `[Assistant]: `,
  `[Assistant reasoning]: `, `[Assistant tool call]: name({args})` +
  `[Tool result]: output` (a part with `time.compacted` renders as
  `"[Old tool result content cleared]"`, otherwise `truncate(...)`), tool errors as
  `[Tool error]: `.
- If **that** request overflows too (`result === "compact"` again) → honest
  `ContextOverflowError` on the message ("Conversation history too large to compact",
  `compaction.ts:450-457`) — fail-hard, never loop unbounded on this path.
- **Replay** (`compaction.ts:475-503`): the original user message is *cloned* — new
  message id, `created = now`, same agent/model/format/tools/system; parts re-created
  with new ids; `compaction` parts skipped; media file parts replaced by text
  `[Attached ${mime}: ${filename}]`. **Nothing is ever deleted** (grep: zero
  `removeMessage`/`delete` in `compaction.ts`).
- **Summary assistant message** (`compaction.ts:397-430`): `mode: "compaction"`,
  `agent: "compaction"`, `summary: true`, `cost 0`, all token fields 0, produced by a
  nested processor with `tools: {}`, `system: []`.
- **Autocontinue** (`compaction.ts:505-545`): after replay, if
  `experimental.compaction.autocontinue` resolves enabled (default **`{enabled: true}``)
  → an extra user message with `"Continue if you have next steps, or stop and ask for
  clarification if you are unsure how to proceed."` (overflow variant prefixes the
  attachments-too-large paragraph) and part metadata `compaction_continue: true`.
  The plugin hook itself is **excluded by evidence** (0 registrations in both installed
  plugins — P0 `tool`-hook class); the default-on *behavior* is core and is ported.

### 2.4 `prune` — timestamps only, never deletes
`compaction.ts:275-318`: walks messages **backwards**, only completed tool parts,
skipping `PRUNE_PROTECTED_TOOLS = ["skill"]` and stopping at a previously-marked part
or a summary assistant; keeps the most recent `PRUNE_PROTECT = 40_000` estimated tokens
verbatim; everything older (when total pruned > `PRUNE_MINIMUM = 20_000`) gets
`state.time.compacted = Date.now()` via `updatePart`. No row is removed.

### 2.5 `filterCompacted` — assembly-time retention (`message-v2.ts:518-578`)
The algorithm every model request (and `/message` reads? — see §8) filters through:
1. Backward walk collecting messages; a user message carrying a `compaction` part sets
   `retain = tail_start_id` (stop-at-boundary semantics when absent); a completed
   summary assistant marks its parent as `completed`.
2. After reversal: find the **last** user message whose `compaction` part has
   `tail_start_id`; locate its summary assistant (child with `summary: true`) and the
   message at `tail_start_id`.
3. If all present and ordered (`tailIndex < compactionIndex < summaryIndex`): output =
   `[compaction user .. summary assistant] ++ [tail_start_id .. compaction user) ++
   [after summary]` — i.e. the compaction anchor + its summary, then the retained tail,
   then everything newer; all older history drops out **without deletion**.

### 2.6 Config keys (shared `opencode.json`, read-sites cited)
`cfg.compaction?.` → `auto` (default true; schema `config.ts:152`), `prune`
(default **false**, `:154`), `tail_turns` (`:228`), `preserve_recent_tokens`
(`:117`, default `min(15_000, max(2_000, usable·0.25))` — constants `:32-33`),
`reserved` (`overflow.ts:16`). There is **no `cfg.compaction.prompt` key** —
`compacting.prompt` is the HOOK output (resolved §8.3: schema read at the
freeze tag confirms five keys only).
Both servers read the same config file → **zero new config surface for ocserve**.

## 3. Wire/compat surfaces (the freeze list)

| Surface | Contract |
|---|---|
| `compaction` part | `{type:"compaction", id, sessionID, messageID, auto, tail_start_id?, summary?}` — client parses `{id, sessionID, messageID, auto}` (Part.kt:164-169) and tolerates extra fields (proven against upstream daily) |
| replay/summary messages | new ids, `mode:"compaction"`/`summary:true` assistant with zeroed tokens/cost |
| `session.compacted` SSE | ocserve emits on compaction (since P0e); **upstream v1 has zero in-tree publishers** — client handler is `Unit` (EventReducer:216) → harmless superset, kept for plugin listeners (magic-context registers it) — §6 divergence |
| transient `session.error` before retry | mirror error-path publish (`processor.ts:631`) |
| manual `POST /session/{id}/summarize` | response stays byte `true`; body behavior upgraded from marker-only to the real engine (`auto=false`) — closes the A7 divergence (today's button relieves nothing) |
| hook order | `experimental.session.compacting` fires inside create (P0c invariant: prompt override + context append; exactly-1 marker); **replay/clone persistence bypasses `chat.message`** (upstream never routes replay through prompt.ts:1000) — re-assert P0c exactly-once |
| config | upstream `compaction.*` keys only |

## 4. Ocserve architecture

- **Trigger** (post-finalize, `prompt.rs`): usage totals already in memory at finalize
  (token columns persisted, schema.rs:28-32) — `is_overflow(count, limit)` pure fn; no
  DB read (§5 P5). Error path: new **overflow classifier** in `ocserve-llm` mapping
  provider error bodies → `ContextOverflow` (OnceLock patterns, P6) feeding the
  prompt-loop retry; fixtures synthesized by the stub.
- **Schema v8 projection** `compaction(session_id, part_id, user_msg_id, tail_start_id,
  summary_msg_id, auto, time)` — maintained by `ocserve-store` part helpers exactly like
  `part_search` (guards rule 4 extends to compaction parts). Purpose: assembly needs the
  *last* compaction anchor + tail **before** a forward streaming pass; without it every
  prompt pays a full `msg_part` type scan. Rows: rare (one per compaction) → tiny.
- **Assembly**: single forward `for_each_message_json` pass implementing §2.5's output
  permutation as a 3-range state machine seeded by the projection row — **never**
  materializes the session (guards rule 3), filters `time.compacted` tool parts inline.
- **Engine**: `post_summarize` (lib.rs:1001) and the auto path share one
  `create_compaction(session, auto, overflow)`:
  serialize conversation (§2.4 format) → compacting-hook request (existing machinery,
  P0c-proven) → on success: clone replay + summary assistant + optional autocontinue
  message in **one `apply_ops` transaction** (§5 P3) → emit part events per message
  (wire-compatible, no batching) → fork prune-equivalent marks → emit `session.compacted`.
- **Guards**: prompt lock held for the whole create (no 409 from inside); the 120 s
  stall watchdog applies per model call as today; **consecutive-auto cap = 3** then
  honest `ContextOverflowError` (§6 D2).

## 5. Internal performance improvements (compat-neutral)

| # | Improvement | vs upstream | Proof obligation |
|---|---|---|---|
| P1 | **Projection-seeded assembly** (§4) — no per-prompt `msg_part` type scan, no second O(N) pass | upstream builds `turns()` + `completedCompactions()` = two extra full in-memory passes per request | equivalence test: projection-seeded output ≡ reference full-walk `filterCompacted` on fixtures (multi-compaction histories included) |
| P2 | **Conversation string**: streamed assembly from stored part JSON (no `Value` round-trip) with a **declared byte cap + tail-weighted truncation** (marker line on truncate) | upstream `msgs.map(serialize).join` = unbounded single String (`:380`) — violates our §1.3 by construction | AGENTS §2.3 bound next to creation; cap-hit test; hooks-visible divergence logged (§6 D4) |
| P3 | **Replay clone = one write transaction** (N rows, 1 fsync) | upstream N × `updatePart` (N writes; partial-failure possible) | atomicity test: injected write failure ⇒ zero partial replay rows; emitted SSE frames identical to row-by-row |
| P4 | **Idempotent prune** — skip parts already carrying `time.compacted` | upstream re-writes marks it already set (dirty-page churn) | selection equivalence vs reference walk; no-op update test |
| P5 | Trigger reads in-memory usage totals (finalize path) — no extra DB round-trip | upstream re-fetches after stream | covered by e2e (no query assertion needed — noted for review) |
| P6 | Overflow classifier patterns `OnceLock`-compiled | n/a (ocserve-only surface) | classifier unit tests: positive fixtures (context-length bodies), negatives (rate-limit/401/500 must NOT compact) |
| P7 | Prune-equivalent scan windowed in SQL (last 2 user turns + budgets) instead of materializing all messages | upstream walks full `msgs` array | selection equivalence (same part set as reference) |
| P8 | Serialize uses raw stored JSON slices for tool inputs where shapes allow (no parse→re-emit) | upstream `JSON.stringify(part.state.input)` | byte-format golden for `serialize()` output on fixture sessions |

**Rejected (antagonized):** resident conversation caching (breaks stateless-sessions
invariant), precomputed token indexes (invalidation vs delta-sync/import — future work
only with a measured trigger), SSE event batching for replay (wire divergence — client
must see per-part events), pre-flight overflow check before sending (upstream is
post-hoc only; a preflight changes failure modes — §6 D5).

## 6. Divergence ledger (all deliberate, all tested)

| ID | Divergence | Rationale |
|---|---|---|
| D1 | **doom-window** (amended K-AUTONOMY): more than 3 automatic compactions *within 30 minutes* → `session.error` (`ContextOverflowError`) then stop compacting — the current answer is kept if one exists; pending-check with no answer errors the prompt. Compactions spaced outside the window (hours-apart overnight refills) NEVER trip. The original flat per-prompt count of 3 killed healthy long turns — the real doom signature is *frequency*, not lifetime | upstream has no hard counter (only the summarize-overflow fail-hard); safety + stall-watchdog precedent; overnight-run requirement |
| D2 | conversation byte cap + tail-weighted truncation (P2) | AGENTS bounded-by-construction; upstream unbounded |
| D3 | overflow classifier = our own unit with fixtures | upstream classifies inside provider SDKs; ocserve has no typed error map — false± each get tests |
| D4 | config trust: your models declare `limit.context = 1_000_000` (A9) → triggers land late, mostly on the error path — **same as upstream on the same config** | freeze-faithful; optional doctor warning parked, not in M6 |
| D5 | no pre-flight token check | mirror upstream call sites (processor:493/629 only) |
| D6 | `session.compacted` emitted by us, zero upstream publishers | client handler `Unit` (safe), plugin listeners benefit; revert to silence if a client ever chokes |
| D7 | autocontinue **hook registration** skipped (0 plugin registrations), default-on **behavior** ported | evidence-excluded class (P0 `tool` hook) |

## 7. Antagonism ledger (2026-10-03, two passes)

mark-vs-delete → **mark-only, zero deletes** (A1/A3); client tolerance → Part.Compaction
serializer + `Unit` handler (A5); autocontinue → default-on core, hook excluded (A6);
**manual summarize relieves nothing today** (A7 — engine unification is mandatory, not
optional); error path is half the trigger (A8 — classifier is a first-class unit);
DOOM_LOOP is tool-loop not compaction (A2 corrected); A9 config limits; retention
permutation fully read (B10); conversation format read (serialize); prunes constants
read. Remaining risks parked in §8.

## 8. Open reading items — ALL RESOLVED (2026-10-03, freeze tag v1.18.31)

1. ✅ `truncate` =2,000 chars + `\n[truncated]` (`compaction.ts:30,51-52`) — in `serialize`.
2. ✅ select/splitTurn read fully; port matches (head = fits budget, `tail_start_id`).
3. ✅ Five config keys only (schema at tag); `prompt` = hook output, not config (§2.6 fixed).
4. ✅ `filterCompacted` = **model assembly only** (`prompt.ts:1092` sole caller); REST
   `/message` serves unfiltered history.
5. ✅ `nextPrompt` = `compacting.prompt ?? [buildPrompt(prevSummary, conversation),
   ...compacting.context].join("\n\n")` (`:381-393`); templates captured (§2.3).

**Probe conflict resolved:** the W3-era "empty user marker (parts=[])" recording
contradicts `create()` at the freeze tag (identical in1.18.31 and .34) — source
wins: the manual anchor carries the compaction part. K-SUMMARIZE test rewritten
to source truth (`dead_provider_persists_anchor_and_summary_shell_then_fails`).

## 9. Test plan (draft; K-COMPACTION row lands with Phase 2 code)

- **Stub e2e**: provider model with `"limit": {"context": 1000}` (fixture exists,
  runtime.rs:926) → forced overflow → assert: single compaction per prompt, doom-window
  (3 clustered compactions trip, hours-apart never — `doom_tests`) →
  honest error, `auto:false` → hard error, replay rows + summary assistant shape,
  autocontinue text + metadata, `session.compacted` + part events, prune marks only,
  P1/P7 equivalence vs reference walk, P3 atomicity (injected failure), classifier
  positive/negative fixtures, `serialize()` format golden, **no-inflation** (summarized
  request strictly smaller than pre-compaction history), hook orderings (compacting
  exactly-1; chat.message not fired by replay; both proven with negative twins).
- **K-SUMMARIZE re-verify**: byte `true` + P0c invariants unchanged after retrofit.
- **Negative control per fix** (TESTING §1) including planted mutant on the tail
  permutation order (§5 P1 obligation).

## 9b. Cache interactions (audit 2026-10-03)

**Provider prefix/KV cache** (vLLM APC / RadixAttention hashing: any changed
byte invalidates from that block onward):
- `filter_compacted` is the **identity function** until the first anchor
  exists → pre-compaction request bytes are identical to pre-M6 → hit-rate
  preserved (byte-golden request suites prove it).
- Post-compaction prefix change is **one-time and by design**; the summary
  request uses a separate prefix (system = hidden compaction agent prompt)
  and never pollutes the main conversation cache.
- Invariant: **no volatile bytes (timestamps, request IDs) in the system or
  tools prefix.** `chat.system.transform` must emit *deterministic* content —
  this is the plugin's responsibility (magic-context appends live memory; if
  it ever timestamps, every request busts its own prefix). Divergence risk
  documented, not suppressible from core.
- `ocserve_llm_cache_tokens_total{provider,dir="read"}` exposes cache-read
  token accrual per request (Usage.parsed `prompt_tokens_details.cached_tokens`).
  Session rows already carried `tokens_cache_read` (finalize) — metrics close
  the gap. Cache-**write** tokens are not parsed by our client (Usage has no
  field) → honestly absent, not zero.

**SQLite page caches:** M6 changed **zero pragmas** (role-scoped `cache_size`,
mmap off untouched). Compaction table rows are tiny/inline; prune marks
(`cfg.prune` default **false**) would churn `part_search` reindex — off.

**Hot-path rule:** `compaction_preflight` = ONE reader per prompt answering
seq+state (`Ready|Pending|SummaryExit`); the newest-message query runs only
when projection rows exist. Round-1 opens = preflight + `load_messages` =
**2 (pre-M6 parity)**. Engine rounds add their own readers (rare, bounded).
Regression net: `ocserve-store` `preflight.rs` 4 tests incl. the row-gate
control (a summary with no projection rows must stay `Ready`).

**Governance decay (arXiv 2606.22528):** the paper's *Constraint Pinning*
defense = re-inject the system constraint after compaction — our architecture
does this by construction (system prompt re-sent verbatim every request,
never compacted). Conversation-carried constraints survive only via the
`SUMMARY_TEMPLATE` **Important Details** section; reassembly keeps
anchor+summary+tail, not the raw oldest turn → residual gap named here,
mitigation = template + system pinning. `ocserve_compaction_total{auto}`
counts every completed compaction (success or overflow-stop) — closes the
rate–distortion survey's "repeated compaction is almost never measured"
gap (arXiv 2607.08032) for the soak to trend.

**Parked (research, not built):** parallel/chunked blocking-compaction
avoidance (arXiv 2605.23296 — tens-of-seconds stall bounded by our 120 s
watchdog today); Acon-style compression-guideline optimization (ICML 2026).

## 10. Phases

- **P1:** reviewed ✅. **P2 (2026-10-03):** DONE — §8 resolved; C1–C5 landed (store v8
  projection → filter/select → engine → outer-loop trigger → manual retrofit at the
  freeze tag); §9 **forced-overflow e2e green** (`forced_overflow_runs_three_compactions_then_caps`:
 3 auto+linked anchors, cap=processes, autocontinue×3, answer kept). Remaining from §9:
 P1/P7 equivalence oracles are covered by `m6_tests` fixtures; no-inflation by
 `bounded_entries` unit bound.
- **P3 (next):** live battery (real plugins + real config against the stub/real model),
  deploy + fresh 24h soak → then K-COMPACTION stays green with live evidence.
