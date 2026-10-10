# Field-contract closure — typed PromptRequest + generated guard

Pre-registered 2026-10-09. Bug: ocserve reads `messageId` (lowercase d) where the
frozen v1 contract sends `messageID` (spec `POST /session/{sessionID}/message`
properties, upstream `prompt.ts:1499 PromptInput`), so client-supplied ids are
silently dropped → oc-remote double-prompt render (app's optimistic row never
confirmed by the server row). Three internal sites: `prompt.rs:536`,
`v2.rs:1020`, `lib.rs:3950` (command).

## Antagonism ledger (decided BEFORE build)

- Global static "key ∈ spec" scan is DEAD: 299 key literals, 118 legitimately
  non-spec (events, DB, responses). Noise.
- Handler-scoped scan is the viable static layer: extractor-bound variable
  names ONLY, read-only ops (assignment targets excluded), request keys must be
  ∈ spec properties ∪ spec query params ∪ allowlist (`offset` = our additive
  `POST /session/search`, PLAN §16.7). Proven at build time with a planted
  `messageId` control.
- Request-side validation alone does NOT close the class (incoming body is
  valid; ocserve ignores it). Only (a) compile-time types on the identity
  family and (b) behavioral echo differential catch it.
- Upstream decode semantics: `Schema.decodeUnknownEffect` with default
  `onExcessProperty: ignore` — unknown keys (`messageId`) pass through and are
  dropped; `MessageID = String.check(isStartsWith("msg"))` — invalid ids are a
  DECODE FAILURE (400), not a silent regenerate. Current ocserve
  `len<=64 && alnum/_` + silent fallback diverges on both axes → align to
  `starts_with("msg")` + 400 at parse (exact envelope bytes probed below).

## Probe list (freeze, fixture HOME, never the user's config)

| # | request | pins |
|---|---------|------|
| P1 | POST /session/{id}/message `{}` | parts required? (missing-key envelope bytes) |
| P2 | same, `{"parts": []}` | decode passes → handler error (endpoint/model) |
| P3 | `{"messageID": 123}` | type-error envelope bytes |
| P4 | `{"messageID": "notamsg"}` | startsWith failure envelope bytes |
| P5 | `{"messageID": "msg_ok…", "parts": []}` dead endpoint | persist-before-fail? (echo readable?) |
| P6 | POST /command w/o `arguments` | arguments required? |
| P7 | POST /api/session/{id}/prompt `{id:"bad", prompt:{text}}` | v2 `^msg_` enforced? bytes |
| P8 | valid echo → GET messages | stored id == client id |

Results → `bench/openapi/field-probes.md` (bytes verbatim).

## Build

- **B1 typed core** (`ocserve-core/src/prompt.rs`): `PromptRequest` with all 9
  spec fields typed (`messageID` via shared `de_message_id` deserializer =
  `starts_with("msg")` → serde error → Payload envelope; `model` →
  `ModelRef{providerID,modelID}`; `noReply` typed; `tools/format` Value;
  `agent/system/variant` String; `parts` Vec<Value>). `CommandRequest =
  {flatten PromptRequest, command, arguments?}` (per P6). `V2PromptInput`
  {id `^msg_` per P7, prompt, delivery, resume}.
- **B2 thread**: `run_prompt`/`run_prompt_with`/`build_prompt_context`/
  `resolve_model(st, Option<&ModelRef>, sid)` take types — `&Value` is GONE
  from the prompt path (type IS the guard). Delete the three `messageId`
  sites. Callers: post_message, prompt_async, post_command, post_shell (via
  resolve_model), summarize builder, run_compact builder, v2 prompt/compact.
- **B3 structural static guard (rule 20)**: python handler request-key scan as
  above + allowlist; planted control `messageId` red→green. Runs commit-time.
- **B4 generated struct-key test (Rust)**: serialize fully-populated
  `PromptRequest`/`CommandRequest` → key set == spec requestBody properties
  (generated from `bench/openapi/1.18.31.json`; upstream adds a field → spec
  refresh → red). Includes ignoring `messageId`-only bodies (negative pin).
- **B5 behavioral differential (nightly)**: `scripts/field-contract-check.sh`
  — boots freeze+ocserve under fixture (parity stub provider), identity echo
  probes P5/P8-equivalents for /message, /prompt_async, /command, v2 prompt;
  assert stored id == client id on BOTH arms and cross-arm equal. Harness
  pattern copied from permission-check.sh.
- **B6 tests**: golden.rs flips to `messageID` + echo assert; 6 fixture files
  swept; invalid-id → probed envelope bytes; v2 + command echo; validation
  unit (`msg`→true? per upstream `isStartsWith("msg")` → `"msg"` passes,
  `"msga"` passes, `"nope"` fails).

## Results (executed 2026-10-09)

Every kill criterion resolved by probe evidence before code was written:

- **`startsWith("msg")` IS enforced at decode** (probe P4 → 400 with
  `Expected a string starting with "msg"`) → implemented; the old
  `≤64 && alnum/_` check was stricter than upstream and is gone
  (5000-char `msg_…` ids pass on freeze).
- **Flatten rejected preemptively**: `CommandRequest`/`V2PromptInput` use
  explicit fields + the shared `de`-style helpers (rename covered by the
  spec-pin test); no flatten-chain ambiguity.
- **Persist-before-fail proven on freeze** (valid `messageID` + dead
  endpoint → client id stored despite 500) → dead-endpoint fixture works
  for echo testing; no LLM needed (`noReply` even skips generation).
- **`noReply`/`system`/`tools` semantics implemented** where the plan had
  them parked: `noReply` returns the user message without a model call
  (probe-proven [200]); `system`/`tools`/`format` persist into User info
  (prompt.ts:661/668/669). The `tools → session.permission RULES replace`
  side-effect (prompt.ts:1059-1067) is NOT ported — ocserve's
  `session.permission` column holds always-grant keys, not
  `PermissionV1.Rule` objects — named **D-PROMPT-TOOLS-RULES**.

Shipped: wire.rs typed decode (4 structs, ~70 probe-corpus byte cases in
unit tests) + spec-pin test; threading through run_prompt/build_context/
resolve_model; envelope fixes (v1 Payload kind / v2 `_tag:InvalidRequestError`);
`tests/field_contract.rs` (15); guard rule 20 (selftest-proven); nightly
`field-contract-check.sh` — **first live run: 42/42 decode vectors
byte-identical to freeze, echo assertions green on both arms, 0 divergences.**

## Kill criteria (historical — all resolved above)

- If freeze does NOT enforce `startsWith` at decode (P4 returns 200-class) →
  record divergence instead of enforcing (parity with observed behavior wins).
  **RESOLVED: freeze enforces (400).**
- If flatten on CommandRequest corrupts decode-error text → explicit duplicate
  fields + shared deserializer (rename covered by B4). **RESOLVED: explicit
  fields used; flatten never introduced.**
- Differential script requires fixture config with an endpoint; if
  persist-before-fail is false on EITHER arm, switch probes to the parity stub
  provider (success path) rather than asserting on failure ordering.
  **RESOLVED: persist-before-fail proven on both arms with a dead endpoint.**
- Out of scope (named, not silent): other handlers stay Value-based (their
  request keys verified in-spec by B3 rule 20); per-request `system`/`noReply`
  semantics not implemented (declared in the type only).
  **SUPERSEDED: noReply/system/tools/format implemented as above; residual
  named gaps live in field-probes.md §Out of scope.**
