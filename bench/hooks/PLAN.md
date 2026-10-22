# Closing the plugin-contract class

Status: executing. Owner: ocserve. Scope: v1 plugin surface (v2 domain hooks in a follow-up).

## Why this document exists

A configured, installed plugin (Magic Context) was silently disabled under ocserve
for weeks. The traceback was a wrong message shape on one hook. That was not the
interesting part. The interesting part is that **four independent mechanisms all
should have caught it and none could**:

- the synthetic fixture used the same wrong shape as the caller, so the test
  asserted the bug;
- the real-plugin corpus invoked `"chat.messages.transform"`, a name that exists
  in neither the SDK nor any plugin, so it reported `hooks-ran` while never
  calling the real hook;
- the guard suite covered the **v2** declared surface and nothing for v1;
- the plugin catches its own exception and returns the input unchanged, so the
  Rust-side error counter structurally cannot see it.

The class is therefore: **a declared plugin surface that we can silently fail to
bind, mis-shape, mis-order, or fail to consume.**

## Definition of closed

Four invariants, mechanically enforced on every commit:

1. **Binding** — every hook the reference implementation actually fires has a
   Rust dispatch site, or a named `D-HOOK-*` row citing evidence.
2. **Shape** — the `input`/`output` we send matches what the reference sends,
   key for key, after normalisation of ids, timestamps and paths.
3. **Order** — within a scenario, the trigger sequence matches, so a plugin that
   reads evolving state sees the same thing.
4. **Effect** — the output we read back is the output that reaches the wire,
   history, or execution. A hook whose return value we discard is a divergence,
   not an implementation.

Meta-invariant: **silence is visible.** A plugin that throws inside its own
handler must not look like a successful no-op.

## Method: oracle first, guards second

The repo's proven pattern for a contract is not "parse the types"; it is "capture
from the reference implementation" (`capture-provider-contract.sh`,
`extract-spec.sh`, `pair-check`, `capdiff`). This plan applies the same pattern
to the plugin surface.

A **probe plugin**, loaded from a local absolute path (freeze's
`pluginSource()` classifies absolute paths as `file`, and `readV1Plugin` imports
them directly, so no npm install is needed), records every trigger with its
actual input and output. That corpus is the oracle. Guards are written against
the corpus, not against our own call sites — which is the difference between
catching an absent hook and passing vacuously.

## Audit: what is actually broken today

Verified against `@opencode-ai/plugin` types and the 1.18.31 sources.

| # | Surface | Status | Consequence |
|---|---|---|---|
| 1 | `experimental.chat.messages.transform` (prompt) | **broken**: sends `{role,content}`; contract is `{info,parts}` | Magic Context throws internally, returns input unchanged; no compaction; the reported overflow |
| 2 | `experimental.chat.messages.transform` (compaction) | shape correct, **length change rejected** (`arr.len() == head_len`) | A splicing transform is discarded |
| 3 | `tool.execute.after` | **wrong input** (4 invented keys) and **output not consumed** (`title`/`metadata` ignored) | Re-titled/annotated tool output is lost |
| 4 | `tool.definition` | **never dispatched** (upstream fires per tool, `registry.ts:318`) | Tool descriptions/schemas cannot be rewritten |
| 5 | `shell.env` | **never dispatched** (upstream `shell.ts:418`, `prompt.ts:555`) | Plugins cannot inject env into shell calls |
| 6 | `tool` registry | host drops `hooks.tool` | Plugin tools do not exist for the model |
| 7 | `config` | dispatched, **result discarded** | Looks wired; mutations vanish |
| 8 | `provider`, `auth` | not dispatched | Loader/registry hooks; oracle decides |
| 9 | `ctx.client` argument convention | `{path,query,body}` treated as a body | Magic Context's historian cannot create/read sessions, so no new compartments |
| 10 | `bench/plugins/check.mjs` | invokes a nonexistent hook name | Corpus was green without exercising the transform |

Items 1–3 and 9 are live defects; 4–7 are absent capabilities; 10 is why the
suite was green.

## Phases

### P0 — prove the oracle (gate)

Boot the vendored freeze under a fixture `HOME` with a local-path probe plugin
and a deterministic stub provider. Confirm the probe loads and that these fire:
`chat.message`, `chat.params`, `chat.headers`, `experimental.chat.system.transform`,
`experimental.chat.messages.transform`, `tool.definition`, `shell.env`,
`command.execute.before`, `experimental.session.compacting`, and that a
probe-declared tool appears in the provider's `tools` array.

**Kill criterion:** freeze will not load a local-path plugin → fall back to the
type surface for *enumeration only* and demote ordering to a documented
non-goal. Record which happened; do not pretend.

### P1 — capture the contract

`bench/hooks/`:

| file | role |
|---|---|
| `probe.mjs` | passive: appends `{seq,hook,input,output}` to `$HOOKS_LOG`, never mutates |
| `probe-active.mjs` | mutates every output the documented way, so *consumption* is observable at the provider/tool boundary |
| `scenarios.json` | the flows: prompt, tool-turn, shell, command, summarize, compact |
| `normalise.py` | strips `ses_/msg_/prt_/evt_` ids, timestamps, absolute paths; model objects reduced to key sets |
| `v1-hooks.json` | generated corpus: per scenario, the ordered `(hook, input keys, output keys)` |
| `v1-triggers.json` | the observed trigger set — the truth for "did freeze fire this at all" |

`scripts/capture-hooks.sh` boots freeze, drives each scenario, writes the corpus.

**Determinism gate:** capture twice; normalised output must be byte-identical or
the run fails.

### P2 — replay and diff

`scripts/hook-contract-check.sh` boots ocserve on the same fixture with the same
probes, drives the same scenarios, and diffs:

- trigger name sequence per scenario;
- input key set and output key set per trigger;
- three **effect observables**: the provider request's `tools` array (plugin tool
  present, description rewritten), the shell child's env (probe-injected variable
  visible in output), and the persisted tool part (`title`/`metadata` from
  `tool.execute.after`).

Every difference must appear in `bench/hooks/allow.txt` with a `D-HOOK-*`
citation. **Today this fails on rows 1–7 and 9** — that is the acceptance
evidence.

**Kill criterion (non-vacuity):** a planted wrong key and a planted removed call
site must both red the harness, proven before it is trusted.

### P3 — fix the members

1. `messages.transform` (prompt): transform `{info, parts}` **after** the history
   budget and **before** `to_provider_messages`. Order matters: the budget drops
   from the front, and Magic Context injects at index 0, so transform-then-budget
   would silently delete the injection.
2. `messages.transform` (compaction): drop the length equality; keep structural
   validation; assert synthetic ids never reach persistence (they feed the
   request only; `tail_start_id` comes from the untransformed selection).
3. `tool.execute.after`: send exactly `{tool, sessionID, callID, args}`; read
   `title`/`output`/`metadata` back into the part.
4. `tool.definition`: fire per tool before schema assembly; apply
   `description`/`parameters` (recompute the JSON Schema only when the plugin
   changed it, matching `registry.ts:319-322`).
5. `shell.env`: fire in `bash()` and the PTY spawn; merge after session env.
6. `tool` registry: host returns `hooks.tool`; Rust adds namespaced tools with the
   plugin's JSON Schema and dispatches execution through the sidecar with a
   `ToolContext` (`directory`, `sessionID`, `ask` bridged to the permission gate).
7. `config`: apply the mutated config and re-derive payloads/wires through the
   existing `watch::reconcile`.
8. `ctx.client`: implement the SDK argument convention `{path, query, body}` with
   `path.id` interpolation. Required for the historian and for any client-using
   plugin.
9. `provider`/`auth`: oracle decides; otherwise `D-HOOK-LOADER-ONLY`.

### P4 — fixtures derived, not authored

Payloads in `tests/hooks.rs` and `bench/plugins/check.mjs` come from the captured
corpus. Delete the `{role,content}` fixture and the assertion that legitimises it.
Add a Magic-Context-shaped fixture that throws on a missing `info.role` and
`splice`s so a length change is exercised.

**Kill criterion:** restore the old payload → the transform test must fail;
restore the length check → the splice test must fail.

### P5 — silence becomes visible

Host returns `{matched, changed, error}` per plugin instead of swallowing; Rust
counts `ocserve_plugin_hook_errors_total{hook,plugin}`. Add
`ocserve_plugin_hook_noop_total{hook}` as a diagnostic (never a gate: legitimate
no-ops exist). One-shot WARN when a registered hook's output is unchanged N=3
times, naming the plugin.

### P6 — guards

- **Rule 47** — hook-contract replay is non-vacuous (planted wrong key + planted
  removed call site both red).
- **Rule 48** — the real-plugin corpus must invoke every hook it claims ran.
- **Rule 49** — no hand-written hook payload literals in tests/bench; they come
  from the generated corpus.

### P7 — documentation and acceptance

Rewrite the `K-HOOKS` clause (it recorded a defect as a decision), add
`bench/hooks/README.md` with the method and bump recipe, add the class to
`TESTING §1.6`, and accept on: replay green, Magic Context's log shows
`transform completed` for the incident session, and a compact on it produces a
new compartment.

## Decisions (locked)

1. `tool` registry: **implement** (schema first, execution second). It is the
   largest gap and Magic Context actually uses it.
2. `config`: **apply** the mutation, then reconcile.
3. Ordering: **exact within a scenario**, names-only across scenarios.
4. Capture scope: **v1 now**, v2 domain hooks in a follow-up.
5. SDK provenance: **pin** the `@opencode-ai/plugin` version the corpus is
   generated against and record it in the corpus file.

## Risks

- The probe perturbs the system → passive and active variants, plus the
  determinism capture.
- Normalisation becomes the new hand-maintained thing → restrict it to
  ids/times/paths; anything else is a real difference.
- Ordering churn on upstream bumps → per-scenario comparison, `D-HOOK-ORDER`
  only with a capture citation.
- Tool-registry execution is the only genuinely new subsystem (Zod→JSON Schema,
  sidecar execution, permission bridge); it is also the only item where a
  documented divergence would be defensible.
