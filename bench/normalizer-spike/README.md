# Normalizer spike (R3) — pre-registered kill criteria

Written BEFORE any build (house rule: pre-register to disk before first run).

## Question

Can a Rust-side normalizer (rolldown crate as a library) convert each opencode
plugin into ONE self-contained ESM file that loads with **zero runtime-specific
patches** on bun, node, AND deno — making runtime choice irrelevant?

## Kill criteria (any FAIL = spike dead, recorded honestly)

- **K1 — normalize**: all 3 real plugins (codex-auth 16-file graph,
  magic-context 4.3 MB bundle, reliary8) bundle to a single ESM output without
  rolldown errors.
- **K2 — hook parity**: on each of {bun, node, deno}, normalized output loads
  and returns hook sets **identical** to the raw load on the same runtime.
- **K3 — behavior parity**: trigger outputs (chat.message, tool.execute.after,
  event delivered count) byte-identical raw-vs-normalized on the same runtime.
- **K4 — dynamic-import survival**: magic-context's computed
  `` import(`@huggingface/${"transformers"}`) `` survives verbatim in output
  AND resolves at runtime from the output's location (emit beside entry);
  embedding init observed on normalized load (the "embedding model changed"
  log line) at minimum, live embed preferred.
- **K5 — sqlite shim on bun**: with `bun:sqlite` aliased to the node:sqlite
  shim in the bundle, magic-context performs REAL sqlite ops on bun
  (create/insert/select through the loaded plugin's db handle path — at
  minimum the shim itself functionally exercised on bun, since bun's
  `require("node:sqlite")` FAILS and only ESM import works).
- **K6 — cost**: cold normalize < 2 s per plugin (it is on the load path);
  warm cache hit ≈ 0 (content-hash keyed). Bundle must not regress trigger
  latency (re-run perf driver, p50 within 2× of raw).
- **K7 — no silent drops**: treeshaking DISABLED (top-level side effects are
  the plugin registration contract); proven by K2/K3 on all three plugins —
  plus explicit config assertion in code.

## K5b — POST-HOC rescue (registered AFTER K5 failed, before testing K5b)

**K5 result: FAIL as written.** The criterion's premise was factually wrong:
bun 1.3.14 has NO `node:sqlite` in file context at all (dynamic AND static
both reject it — my original "ESM import works" evidence came from
`bun -e` eval context, which resolves differently and does not represent
file-module loads). The node:sqlite-only shim cannot serve bun; on bun the
bundled magic-context failed to load (`ResolveMessage: No such built-in
module`), cascading to bun K2/K2b/K3 FAIL. Node + deno normalized PASS
clean (K2/K3/K4-embedding-init seen).

**K5b (post-hoc, explicitly not the original registration):** dual-runtime
shim — prefer native `bun:sqlite` via *computed* dynamic import
(`"bun:" + "sqlite"` — non-analyzable, same trick as magic-context's own
transformers import, so no bundler alias recursion and runtime-catchable),
fall back to computed `node:sqlite` (dynamic too: bun would fail
*graph link* on any static node:sqlite import), TLA live-binding exports
providing `Database` (the only name magic-context imports: 4 aliases).
Re-normalize all 3, rerun the FULL parity matrix. If K5b passes: spike
rescued, with the record showing the registered criterion failed and a
post-hoc shim variant fixed it. If K5b fails: spike dead per original K5.

## Non-criteria (recorded, not gating)

- Binary-size/build-time impact on ocserve itself: spike crate is detached
  from the workspace; integration-phase gate = release ≤ 10,485,760 B
  (existing ceiling) + build-time delta reported.
- Sourcemap/debuggability of outputs.
- TS transformation (oxc transform exists; no TS plugin in scope today).

## Attack ledger (design answers, decided pre-build)

- A1 output location → MUST be beside entry (dynamic-import ancestry);
  content-hash filename, additive file in opencode cache, rebuild on miss
  (opencode cache wipe = fail-soft rebuild).
- A2 side-effect order → esm import order preserved by bundler; verified K2/K3.
- A3 externals → node builtins only (platform=node); bare npm specs get
  bundled EXCEPT non-analyzable dynamics (stay runtime by construction).
- A4 `@opencode-ai/plugin` → bundled (tiny, kills deno declared-dep rule);
  if it regresses, flip to external-beside-entry (both recorded in config).
- A5 `import.meta` → 0 uses (grep-proven) — attack dead.
- A6 treeshake → forced off (K7).
