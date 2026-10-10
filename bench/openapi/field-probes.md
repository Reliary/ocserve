# Field-contract freeze probes — decode-error byte corpus

Captured 2026-10-09 against vendored freeze 1.18.31 (`bench/parity/vendor/opencode`)
under a fixture HOME (fake provider, dead endpoint `127.0.0.1:9`). Scripts:
`/tmp/opencode/field-probe{,2..6}.sh` (ephemeral; bytes recorded here are the
commit-time contract). Every claim below is a literal captured response.

## Envelopes

- v1 routes (`/session/{id}/message`, `/prompt_async`, `/command`):
  `{"name":"BadRequest","data":{"message":"<msg>","kind":"Payload"}}` [400]
- v2 routes (`/api/session/{id}/prompt`, `/api/session/{id}/permission`,
  `/api/session/{id}/revert/stage`):
  `{"_tag":"InvalidRequestError","message":"<msg>","kind":"Payload"}` [400]

## v1 message decode (validation order = spec property order, first failure wins)

| request | message |
|---|---|
| `{}` (valid + nonexistent session) | `Missing key\n  at ["parts"]` (decode precedes 404) |
| valid body, nonexistent session | `NotFoundError` / `Session not found: …` [404] |
| `messageID:123` | `Expected string \| null, got 123\n  at ["messageID"]` |
| `messageID:true` | `Expected string \| null, got true\n  at ["messageID"]` |
| `messageID:"notamsg"` | `Expected a string starting with "msg", got "notamsg"\n  at ["messageID"]` |
| `messageID:"msg"` | PASSES decode (bare `msg`; rule is `startsWith("msg")`) |
| `messageID:null` | PASSES (null = absent) |
| `{"messageID":"bad","model":{}}` | messageID error (order: messageID before model) |
| `{"agent":123,"parts":"x"}` | agent error (order: agent before parts) |
| `model:123` | `Expected object \| null, got 123\n  at ["model"]` |
| `model:{}` | `Missing key\n  at ["model"]["providerID"]` |
| `model:{providerID}` | `Missing key\n  at ["model"]["modelID"]` |
| `model:{providerID:123,modelID}` | `Expected string, got 123\n  at ["model"]["providerID"]` |
| `agent:123` | `Expected string \| null, got 123\n  at ["agent"]` |
| `noReply:"x"` | `Expected boolean \| null, got "x"\n  at ["noReply"]` |
| `tools:"x"` | `Expected object \| null, got "x"\n  at ["tools"]` |
| `tools:{a:1}` | `Expected boolean, got 1\n  at ["tools"]["a"]` |
| `format:123` | `Expected OutputFormat \| null, got 123\n  at ["format"]` |
| `format:"text"` | `Expected OutputFormat \| null, got "text"\n  at ["format"]` |
| `format:{}` | `Expected OutputFormat, got {}\n  at ["format"]` (object loses `\| null`) |
| `format:{type:"bogus"}` | `Expected OutputFormat, got {"type":"bogus"}\n  at ["format"]` |
| `format:{type:"json_schema"}` | `Missing key\n  at ["format"]["schema"]` |
| `format:{type:"text"}` | PASSES decode |
| `system:123` | `Expected string \| null, got 123\n  at ["system"]` |
| `variant:123` | `Expected string \| null, got 123\n  at ["variant"]` |
| `parts:"x"` | `Expected array, got "x"\n  at ["parts"]` |
| unknown key `messageId:"msg_…"` | PASSES; value NOT stored (server generates) |

`prompt_async {}` → identical `Missing key … ["parts"]` (decode precedes 204).

## v1 parts union

Union message (non-object, missing type, or unknown type — one shared message):

```
Expected { readonly "type": "text", ... } | { readonly "type": "file", ... } | { readonly "type": "agent", ... } | { readonly "type": "subtask", ... }, got <js_repr>
  at ["parts"][<i>]
```

- `parts:[123]` → got `123`; `parts:[{}]` → got `{}`; `parts:[{type:"bogus"}]` → got `{"type":"bogus"}`
- text: `{type:"text"}` → `Missing key\n  at ["parts"][0]["text"]`;
  `text:123` → `Expected string, got 123\n  at ["parts"][0]["text"]`;
  `id:"bad"` → `Expected a string starting with "prt", got "bad"\n  at ["parts"][0]["id"]`
  (id checked before text = property order)
- file: `{}` → `…["mime"]`; `{mime,filename}` → `…["url"]` (required order mime, url)
- agent: `{}` → `…["name"]`
- subtask: `{}` → `…["prompt"]`; `+prompt` → `…["description"]`;
  `+prompt+description` → `…["agent"]` (spec required order)
- extra keys on a valid part (`bogus:1`, `sessionID:"ses_z"`) → PASSES
  (`additionalProperties:false` in spec is NOT runtime-enforced)

## v1 command

- `{}` → `Missing key\n  at ["arguments"]`; `{"command":…}` → `…["arguments"]`;
  `{"arguments":…}` → `…["command"]` (required order: arguments, command)
- `parts:[{type:"text",…}]` → `Expected { readonly "type": "file", ... }, got
  {"type":"text","text":"z"}\n  at ["parts"][0]` (command parts = file-only union)
- `parts:[{type:"file",mime,url}]` → PASSES decode
- no `parts` key → PASSES (optional)

## v2 `/api/session/{id}/prompt`

| request | message (envelope `_tag:InvalidRequestError`) |
|---|---|
| `{}` | `Missing key\n  at ["prompt"]` |
| `prompt:{text:123}` | `Expected string, got 123\n  at ["prompt"]["text"]` |
| `prompt:{text,files:[{}]}` | `Missing key\n  at ["prompt"]["files"][0]["uri"]` |
| `id:"bad"` / `id:"msgx"` | `Expected a string starting with "msg_", got "…"\n  at ["id"]` (**`^msg_`, underscore required — differs from v1 `^msg`**) |
| `delivery:"bogus"` | `Expected "steer" \| "queue", got "bogus"\n  at ["delivery"]` |
| `delivery:null` | PASSES (null = absent) |
| `resume:"bad"` | `Expected boolean \| null, got "bad"\n  at ["resume"]` |
| valid `{id:"msg_…",prompt:{text}}` | [200] `{data:{admittedSeq,id,sessionID,prompt,delivery,timeCreated}}` (echoes client `id`) |

Other v2 routes share the envelope: `/revert/stage {}` → `…["messageID"]`;
`/permission {}` → `…["action"]`.

## Behavioral facts used by the differential

- Persist-before-fail: valid `messageID` + dead endpoint → freeze stores the
  CLIENT id (`msg_fprobe00000000000000001` observed via GET) despite [500] —
  so a dead-endpoint fixture suffices for echo testing on both arms (no LLM).
- Decode precedes session existence (400 beats 404) and precedes 204 on
  prompt_async.

## Probe rounds 7–8 (semantics + remaining decode bytes)

| request | message / result |
|---|---|
| `noReply:true` + text part, dead endpoint | **[200] info=role:user, parts=[text] — NO model call** (user persisted, loop skipped; upstream prompt.ts:1069) |
| `format:{type:"json_schema",schema:123}` | `Expected JSONSchema, got 123\n  at ["format"]["schema"]` |
| `format:{…,retryCount:"x"}` (schema `{}` OK) | `Expected number \| null \| null, got "x"\n  at ["format"]["retryCount"]` (triple-null is Effect's byte) |
| `agent:""` | [500] — same as default-agent path (JS falsy `""` → default; ocserve's `filter(!is_empty)` matches) |
| v2 `id:null` | [200] server-generated id (null = absent) |
| v2 `prompt:"x"` | `Expected PromptInput, got "x"\n  at ["prompt"]` |
| v2 `prompt:{files:[],…}` w/o text | `Missing key\n  at ["prompt"]["text"]` |
| v2 `files:[123]` | `Expected PromptInput.FileAttachment, got 123\n  at ["prompt"]["files"][0]` (DOT notation) |
| v2 `agents:[123]` | `Expected Prompt.AgentAttachment, got 123\n  at ["prompt"]["agents"][0]` |
| v2 `agents:[{}]` | `Missing key\n  at ["prompt"]["agents"][0]["name"]` |
| part `synthetic:"bad"` | `Expected boolean \| null, got "bad"\n  at ["parts"][0]["synthetic"]` |
| permission `save:false` | **v2 envelope**: `{"_tag":"InvalidRequestError","message":"Expected array, got false\n  at [\"save\"]","kind":"Payload"}` |
| command `{messageID:"bad"}` (no args) | messageID pattern error (property order: messageID first) |
| shell `{}` and `{command:"ls"}` | `Missing key\n  at ["agent"]` (required order agent, command; v1 envelope) |
| root body `[1]` / `"hello"` | `Expected object, got [1]` / `Expected object, got "hello"` — **NO `at` path** (root) |
| `tools:null` | passes (null = absent) |
| `parts:null` | `Expected array, got null\n  at ["parts"]` (required field: null ≠ absent) |
| command extra key `{…,"extra":1}` | passes (unknown keys ignored — probed 4× now) |
| command unknown name | [500] UnknownError (lookup failure ≠ decode) |
| v2 `delivery:"queue"` | [200] echoes `"delivery":"queue"` |
| `messageID` 5000 chars | passes (**no length limit** upstream — drop ocserve's `≤64`) |
| `model:{providerID,modelID,zzz:1}` | passes (unknown keys ignored inside model) |

Upstream `prompt()` flow (prompt.ts:1052-1070): persist user message (chat.message hook inside) → `input.tools` → **session permission rules replace** `{permission:t, action:allow|deny, pattern:"*"}` persisted via `setPermission` → `noReply===true` → **return message** (no loop). `input.system`/`input.tools` also persisted into User info (prompt.ts:661,668).

## Out of scope (named, not silent)

- Deep `JSONSchema` validation under `format.json_schema.schema` — presence
  only (recursive schema validation not probed).
- Endpoint-resolution vs session-404 ordering when no endpoint configured
  (ocserve resolves endpoint pre-404; freeze requires session first) —
  adjacent ordering divergence, not a field-name issue.
- **D-PROMPT-TOOLS-RULES**: upstream rewrites `session.permission` with
  `{permission, action, pattern}` RULE objects from `input.tools` before the
  noReply return (prompt.ts:1059-1067); ocserve's `session.permission`
  column stores always-grant keys (different schema + different consumer) —
  the ruleset side-effect is not ported (field itself is decoded and
  persisted into User info).
- Shell `messageID` echo: decoded and used for the synthetic user message,
  but freeze's shell-side use of the client id was not probe-verified
  (shell flow was probed for DECODE only).
- Command input `parts` are decoded (file-only union, freeze-exact 400s) but
  not appended to the run's parts — matches the pre-existing behavior
  (run_prompt persists text parts only; upstream appends them — M3 file-part
  persistence gap, independent of this class).
- v2 `admittedSeq`: ocserve returns `0`; freeze returns an opaque counter
  (fresh session → 1) — semantics unprobed, excluded from byte-diff.
- v2 delivery: ocserve runs the prompt synchronously; freeze ADES the
  request (200 on a dead endpoint, results via SSE). Decode + response
  `delivery` field (`steer` default, probe-pinned) are aligned; the
  admission-vs-sync execution model is a separate named divergence.
- `format`/`system` on User info are persisted (byte shape per
  prompt.ts:661-669) but not ACTED on (no structured-output provider
  wiring / no per-request system prompt override — neither traced to a
  consumer in upstream prompt.ts).
