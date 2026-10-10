# Streaming-delta contract (K-STREAMDELTA)

The 2026-10-11 bug: **the web UI showed nothing when you sent a message, but the
reply appeared after exiting and re-entering.** Root cause and its class closure.

## Root cause

The web UI (`index-Kw4ozAkJ.js`, the pinned 1.18.31 bundle) detects v1 protocol
via `/global/health` and renders an assistant message's **live streaming text
exclusively from `message.part.delta` events**. Its reducer
(`case "message.part.delta"`, app bundle) does:

```js
const i = store.part[r.messageID];      // the message's parts
if (!i) break;
const s = search(i, r.partID);          // find the part by id
if (!s.found) break;                    // <-- DROP: nothing to accumulate into
...
```

Upstream (`packages/opencode/src/session/processor.ts:500-524`) mints a
`PartID` at **text-start**, publishes the empty part via `updatePart`, then
streams `updatePartDelta` with that real `partID` and the **assistant**
`messageID`. The assistant message itself is created (and published) *before*
the provider turn (`prompt.ts:1186-1201`).

ocserve did neither:

```rust
// prompt.rs (pre-fix) — StreamEvent::TextDelta
emit_live(ctx, "message.part.delta", json!({
    "sessionID": session_id, "messageID": user_msg_id, "partID": "",
    "field": "text", "delta": t,
}));
```

Three independent contract violations, each enough to drop the delta:
1. `partID: ""` — spec `MessagePartDelta.partID` is **required** with pattern
   `^prt`; the reducer's `search(partID)` fails → `break`.
2. `messageID` was the **user** message, not the assistant message.
3. No preceding `message.part.updated` start part, so `store.part[messageID]`
   was empty anyway.

The full text part IS persisted at turn end, which is why a **reload** showed
the reply. Deltas are `emit_live` (bus-only) and never persisted, so guard
rule 19 / `event-validate.py` — which validates the persisted `event` table —
**could not see this class** by construction.

## Fix

`crates/ocserve-core/src/prompt.rs`:
- The assistant `MessageID` and the text/reasoning `PartID`s are minted
  **before** the provider turn and the assistant skeleton is published
  (`assistant_message_start`, mirroring `prompt.ts:1186-1201`) — lazily, on the
  first content event, so a turn that fails with no output leaves no ghost.
- `emit_part_start` publishes the empty text/reasoning part before its first
  delta (`processor.ts:280-291/500-511`).
- Every `message.part.delta` carries the real `^prt` `partID` and the assistant
  `messageID`.
- The persisted final text/reasoning part **reuses the streamed id**, so the
  client reconciles stream → final into one part (upstream updates the same
  `currentText` at text-end, `processor.ts:526-545`).

The compaction summary path (`compaction.rs`) already used a real id and is
unchanged.

## Class closure

The blind spot is **live-only (`emit_live`) events**: they never reach the
persisted event log, so the log validator and the JSON-body validator both miss
them.

- **Behavioral (canonical):** `crates/ocserve-http/tests/stream_delta.rs` drives
  a real provider turn against a stub SSE provider (no LLM), captures the bus
  frames, and asserts the full contract: live assistant skeleton (no
  `time.completed`), text start part with `^prt` id before its deltas, deltas
  carrying `^prt` `partID` + assistant `messageID` + a preceding start part, and
  the persisted final part reusing the streamed id. A second test feeds the
  **exact pre-fix shape** (`partID:""`, user messageID, no start part) to the
  shared checker and requires a flag — the checker can never pass vacuously.
- **Static:** guard rule 21 (`check-guards.sh`) bans the literal empty `partID`
  assignment in `crates/*/src`, with a planted-violation self-check.

## Per-upstream-bump cost

Unchanged discipline: the wire contract is the frozen spec
(`bench/openapi/1.18.31.json`) already vendored for the coverage/field guards.
No new hand-maintained truth. `extract-spec.sh` + gates.
