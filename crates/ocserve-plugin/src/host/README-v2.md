# ocserve v2 plugin support (Phase 3)

ocserve speaks the OpenCode v2 plugin API at `@opencode/plugin`. Both
registration dialects are accepted and normalized onto one dispatch path
(D7). This file is the capability matrix: what is bridged, and what is a
**named divergence** (`D-V2-*`) rather than a silent gap.

Authoritative upstream sources (read to build this, not bundled):
`~/src/opencode-v2/packages/plugin/src/{promise,effect}/*.ts` and
`.../schema/*`.

## Dialects (both work)

| Dialect | Shape | How ocserve runs it |
|---|---|---|
| promise | `Plugin.define({id, setup(ctx)})` | `await setup(ctx)` directly; returned cleanup runs on dispose/respawn |
| effect | `Plugin.define({id, effect(ctx)})` | `def.effect(ctx)` is an Effect; run via the plugin's **own** `effect` runtime (`createRequire(plugin entry).resolve("effect")`), scoped. No Effect is bundled or reimplemented. |

Detection: a module whose default (or a named export) is an object with a
string `id` and a `setup`/`effect` function is v2. `Plugin.define` is
identity, so this is exact.

## Hook domains

| v2 hook | Status | Maps to |
|---|---|---|
| `session.hook("context")` | **bridged** | mutates the system prefix immediately before provider dispatch (same site as v1 `experimental.chat.system.transform`, runs first) |
| `session.hook("compaction")` | **bridged** | `result.summary` overrides the built summary prompt (same channel as v1 `experimental.session.compacting` `prompt`); `context[]` appends |
| `tool.hook("execute.before")` | **bridged** | mutates tool `input`; runs before the widened-args guard so v2 mutations get the same re-evaluation as v1 |
| `tool.hook("execute.after")` | **bridged** | mutates the tool `result` string (status union mapped: `completed`→result) |
| `event.subscribe` | **bridged** | same bus → sidecar delivery as v1 `hooks["event"]` |
| `session.hook("prompt")` | **diverged** `D-V2-SESSION-PROMPT` | not dispatched; the v1 `chat.message` site covers prompt mutation and no v2 plugin in the wild depends on the v2-only `prompt` hook shape |
| `session.hook("model.request"/"http.request"/"http.response"/"retry"/"title"/"generate")` | **diverged** `D-V2-SESSION-LIFECYCLE` | the transport-level hooks bind to upstream's Effect HTTP/WebSocket driver, which ocserve does not have (a different, non-Effect client); retry/title/generate policy is internal |
| `session.hook("experimental.ws.handshake")` / `"experimental.ws.send"` / `"experimental.ws.receive"` | **diverged** `D-V2-WS` | ocserve drives no websocket provider transport |
| `permission.hook("evaluate")` | **diverged** `D-V2-PERM-EVAL` | ocserve's gate is `ocserve_core::permission` (rule-based, upstream-equivalent, unit-tested); a v2 evaluate hook is not on the dispatch path |
| `shell.hook("create.before")` | **diverged** `D-V2-SHELL` | the v1 `tool.execute.before` path covers shell spawn; the v2 shell domain binds upstream's shell service, absent here |
| `aisdk.hook` | **diverged** `D-V2-AISDK` | upstream's AI-SDK adapter is Effect-specific; ocserve has its own provider client |

## Domains

**Registration is not dispatch.** Every domain that exposes `transform` accepts
the callback and records it, even when ocserve cannot apply the edit. A throw at
registration time fails the plugin's entire `setup`, which loses the hooks that
*are* bridged — a divergence must cost one capability, not the plugin. Genuine
*use* of an unbridged domain (calling a verb, not registering) still throws the
named `D-V2-DOMAIN` error, because the capability really is absent. The accepted
domains are reported in the plugin inventory as `unbridged_transforms`.

| v2 domain | Status |
|---|---|
| `storage` (get/set/remove/scan) | **bridged** — in-memory per-plugin KV. Named divergence `D-V2-STORAGE-MEM`: upstream is disk-backed; ocserve's store is process-lifetime (cleared on restart). |
| `rpc` (register/call/events) | **partially bridged** — `register` + handler dispatch + `/api/rpc/{id}/{method}` are real. `Registration.events.emit` → `D-V2-RPC-EVENTS` (upstream emits over the RPC socket; not bridged). |
| `tool.transform/list/reload` | **partially bridged** — `transform` REGISTERS (the callback is recorded, never invoked: the tool registry is static, `D-V2-TOOL-EDITOR`) and `list` returns `[]`; `reload` is a no-op. Registration must not throw: v2 plugins register transforms during `setup`, and failing there would discard the hooks we do bridge (found by the real-plugin corpus, 2026-10-21). |
| `client` (REST proxy) | **bridged** — method-path proxy over the local HTTP API (same as v1's `client`) |
| `event.subscribe` | **bridged** |
| `plugin.list` | **bridged** — real inventory (`GET /api/plugin`) |
| `command.transform` + `skill.transform` | **bridged** — Rust PULLS every registered transform on config reload (`v2_transform_commands` RPC) and stores the result in `Payloads.command`. Both consumers see it: `GET /command` **and** the `<available_skills>` block in the system prompt. Pushed-transforms would race a reload and serve a stale list; the pull makes the edit land in the same swap window as the payload. Upstream `editor.add` REPLACES a same-named entry (`core/command.ts:61`), so a reload is idempotent. A throwing transform is reported and skipped, never fatal, and an empty result is rejected (a plugin cannot silently erase the command surface). Applied at boot too, not only on reload. |
| `command.list` / `skill.list` / `skill.get` | **bridged** — read the last pulled list |
| `command.reload` / `skill.reload` | **bridged** — marks a reload; the next reconcile (or the boot-time pass) re-runs the pull |
| `session`/`permission` (REST half) | **bridged** — each verb bound to the route that exists (`/api/session/{id}`, `/message`, `/context`, `/history`, `/inbox`, `/diff`; `/api/permission/saved`, `/request`). Unbound verbs throw `D-V2-DOMAIN`. |
| `agent`/`integration`/`mcp`/`model`/`provider`/`reference`/`vcs`/`websearch`/`worktree`/`generate`/`aisdk`/`experimental` | **diverged** `D-V2-DOMAIN` — exposed as honest throwing proxies: a plugin that calls one gets a named error at the call site, never a crash at setup and never a fabricated success. `agent`/`model`/`provider` stay unbridged deliberately: their payloads feed registries and system-prompt assembly, so editing only the served list would be shape-without-mutation (A1 rule). |

## RPC dispatch

`POST /api/rpc/{rpcID}/{method}` routes to the v2 plugin that registered
`{id: rpcID}` (last registration wins). Errors are the v2 RPC union:
`rpc.unavailable` (unknown id, 400), `rpc.method_not_found` (known id, unknown
method), `rpc.handler_error` (thrown handler, 500). When **no** plugin
registers the id, the response is the byte-exact `rpc.unavailable` error
(parity preserved — verified live against 2.0.28).

## Verification

`crates/ocserve-plugin` test `v2_battery_promise_and_effect_dialects` loads a
promise plugin (session hook + tool hook + RPC + storage) and an effect plugin
(tool hook with a vendored `effect` runtime) through the real sidecar and
asserts detection, dispatch, RPC round-trip, inventory, and error parity.
Negative control (proven): removing the effect runtime makes the effect
plugin fail loudly — it is never silently skipped.
