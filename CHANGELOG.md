# Changelog

All notable changes to ocserve. The wire contract targets the frozen
openccode 1.18.31 release and the v2 surface the 2.0.28 client; anything that
differs is a named `D-*` row in the ledger at `PLAN.md §18`.

## Unreleased

Not yet released. No version bump, tag or publish has been made for these.

### Fixed
- **Five v2 operation families were bound from the contract, not the reference**
  (2.2.5). A live differential found each was plausible-but-wrong:
  `experimental.schedule` create REQUIRED a `type` key that upstream's untagged
  `{in|at|every|cron}` input does not have, so every valid client payload was
  rejected; `artifact` ids are branded by PREFIX only and PATCH decodes its
  body before the id lookup and feature gate; `debug.heap-dump` answered a 500
  where the contract requires a 200 (`path`, `pid`, five `memory` counters,
  no extra keys); the artifacts family refused every call even with
  `experimental.artifacts` on; `voice/session` transport is the discriminated
  `VoiceTransportInput` union and its refusal is the provider's own; and
  `client-mcp` GET answers a BARE 400 when a name is present (only the token
  route carries the tagged refusal). `fs/watch` now resolves inside the session
  location, so an existing file outside it is `FileNotFoundError`.
  The 27 vectors are committed in `pair-v2-check` (55 total) with the feature
  flags enabled in both temp HOMEs.
- **A model with unknown capabilities is text-only.** An image sent to such a
  model is now replaced with the explanatory text part (upstream's
  `unsupportedParts`) instead of forwarded for the provider to reject;
  config-declared models derive capabilities from `modalities`. A
  `text/plain` data-URL attachment is expanded AT INGEST into the synthetic
  Read-call line plus its body, and a turn with multiple parts is emitted as a
  content ARRAY in part order rather than one concatenated string — capdiff
  showed the merged form changed the prompt the model reads.
- **A provider's `options.apiKey` was never used as the credential** (issue
  #28), so every config-declared provider reached the upstream with an empty
  bearer token. Auth layering now falls back config-first; the 2.x
  `credential` table is a third source.
- **The embedded commit stamp was one build behind** after a commit (HEAD is a
  symbolic ref), and the offload path reported a foreign tree dirty. Both
  fixed; the offload passes the LOCAL sha and dirty flag.

### Security
- **RUSTSEC-2026-0459 / CVE-2026-119375** (SQLite before 3.54.0 could return
  uninitialised page-cache memory when a database and a sibling `-wal` file
  disagree on page size). The old `libsqlite3-sys` was pulled by `r2d2` and
  `r2d2_sqlite`, both declared in `ocserve-store` and imported nowhere;
  removing the two unused declarations let rusqlite 0.41 resolve
  libsqlite3-sys 0.39.1 (SQLite 3.54.1).

### Changed
- Dependency updates: rolldown 1.3.x, boring 5.4, brotli 9.1.1, toml 1.2.1,
  rustix 1.1.6, flate2 1.1.11, similar 3.2.1. `boring` needed its requirement
  raised, not only the lock.
- CI actions: `setup-rust-toolchain` v2.0.1 (13 uses) and `upload-artifact`
  v7.0.2 (5 uses), each pin verified against its upstream tag.

### Added
- `scripts/remote-build.sh` offloads cargo to a builder host, because the
  development box's `target/` reached 912 GB on a snapshot-pinned filesystem
  and failed with ENOSPC more often than with compiler errors.

## 0.7.0

Both API surfaces moved to the current upstream releases: the v1 freeze to
**1.19.5**, the v2 contract to **2.2.5**. Everything below is in this release.


### Added
- **Attachments reach the provider.** A user turn with an image or PDF was
  stored and then silently flattened to text at the provider boundary: the
  model never saw the picture and nothing reported it. User `file` parts now
  encode as OpenAI content parts (`{type:"file", url, mediaType, filename}`)
  exactly where upstream's AI-SDK conversion does, with `text/plain` and
  directory files still dropped as upstream drops them. A turn with no media
  stays a plain string, so text-only provider bytes are unchanged.
  `GET /api/experimental/media/{hash}` serves content-addressed attachments.
  The gate is a corpus differential over every file part in a real database
  (decoded from blobs exactly as production does), with a planted
  encoder-drop control proven red.
- **Scheduled session wake-ups** (v2 `experimental.schedule.*`, schema v16):
  create/list/get/pause/resume/delete, with due times computed for `in`,
  `at` and `every` specs. Gated on `experimental.schedules` in config with
  the reference's own `ForbiddenError` when off. A `cron` spec is stored
  without a due time (`D-SCHEDULE-CRON`) rather than guessed at.
- A v2 UI pack now implies the v2 runtime: `OCSERVE_UI_PACK=v2` (or `2.x`)
  turns the `/api/*` surface on when `OCSERVE_V2` is unset, because the 2.2.5
  app calls those routes on boot. An explicit `OCSERVE_V2=0` still wins.

### Changed
- The v2 plugin `ctx` domains `vcs`, `worktree`, `websearch` and `generate` are
  now bound to the routes ocserve already serves; they were honest throwing
  proxies even though every verb worked, which told a plugin a present
  capability was absent (the same class fixed earlier for model, provider and
  agent). Their `transform`/`reload` editors stay recorded-but-inert with a
  named row, because replacing the git/worktree/search provider is not a
  route. Unknown verbs on any bridged domain still throw a NAMED error rather
  than an undefined-function TypeError.
- **The v1 freeze moved to upstream 1.19.5** (from 1.18.31), adopted through the
  `bump.sh v1` pipeline rather than by hand. The wire contract is unchanged:
  188 operations over the same 162 paths, two schema descriptions, one additive
  `reasoning_options` property, and identical tool/prompt text for everything
  the server sends. The measurable deltas were the composite user-agent's
  runtime segment and provider header routing (below), both caught by capdiff's
  header oracle, not by any shape gate.
- The web-UI pack, ARITY rules table, prompt assets and provider tool capture
  were re-extracted from 1.19.5 and are now slot-named (`v1.*`), so the six
  `ocserve-ui-v1-*` shard crates keep their names across bumps.
- The bash tool's description template and its command section were the last
  hand-vendored prompt files; `extract-prompts.sh` now derives both from the
  pinned tag (`tool/shell/shell.txt` plus the `bashCommandSection` TS literal),
  so they cannot silently go stale on the next bump.

### Fixed
- **v2-only routes now serve the exact contract envelope (nightly gate).**
  The nightly `spec-validate` caught `VIOL /api/experimental/usage/limits
  [location] Additional properties are not allowed ('project' was unexpected)`.
  Root cause was the D-V2-UNION envelope applied where no union is needed: the
  `{directory, project}` location shape exists because a single port serves
  both client families, but routes ABSENT from the frozen 1.18.31 contract have
  no v1 caller at all. All 41 v2-only handlers now use `api_location_ref`
  (`{directory}` only, the contract's `Location.PublicRef`), and the eight
  allowlist entries that had been papering over the extra member were removed
  rather than kept. Shared routes keep the union. Verified: spec-validate-v2
  36 checked / 0 violations (was 36/1), pair-v2 0 divergences, 847 tests.

  - The non-persisted goal reminder (upstream `goal.ts:219-231`) was compiled
    into the binary and never used, so an overnight goal lost its objective as
    soon as a compaction dropped the kickoff from the filtered history. It is
    now spliced before the tail on a user turn when the objective is not in
    view, and it is never persisted.
  - A FAILED continuation was logged and retried: the reviewer ran again, said
    `continue`, and the same failure repeated once per cycle with a provider
    call each time. A failed goal turn now pauses with the reason.
  - `ocserve_goal_total{event}` counts armed/paused/complete/turn_failed, and
    `ocserve doctor` prints the loop switch and every ACTIVE goal (session,
    rounds, objective) so "the loop is doing nothing" is visible without SQL.
- **Goal-loop reliability review findings (2026-11-05).** Six defects found by
  a production-reliability review, all fixed and covered:
  - The reviewer ran against a synthetic session id, so the history loader
    found nothing and the judge could only guess. It now runs against the real
    session (`persist_user:false` + `transient` keep it out of history).
  - Goal writes computed `seq` as a per-session `MAX(seq)+1` into the GLOBAL
    event primary key with `INSERT OR REPLACE`, so arming a goal on one session
    could delete another session's event. Both writes are gone: nothing read
    the row, and its type was not in the frozen Event union.
  - Continuations were untracked spawns: `/abort`, session delete and
    `/goal pause|clear` did not stop the loop, which re-spawned after each
    turn. A tracked driver per session now re-checks the goal id and status
    before every turn, and is cancelled by all four paths.
  - The idle hook awaited the reviewer, so `/message` and `/prompt_async` on a
    goal session waited for a second full model call. It is now fire-and-forget
    with one pending review per session (overlapping wakes could double rounds).
  - A kickoff that could not acquire the session lock or build its context left
    an ACTIVE goal with no driver; it now pauses with the reason.
  - The v2 drain never fired the idle hook, so a goal armed through the v2
    command route could never be reviewed.
  Goal state changes are now announced on the live bus as `session.updated`
  with an additive `goal` member (the frozen union has no goal event type).
  The unreachable 30-minute wait timer was NOT implemented: the shell registry
  carries no session id, so the judge is never told about a running shell;
  recorded as `D-GOAL-NO-BACKGROUND-SHELLS` rather than shipping dead code.
- **`/goal` was unreachable from every real client.** The built-in entry is
  advertised with `template: ""` because the v1 `Command` schema requires the
  field, but the dispatcher tested `template.is_none()`. That predicate is true
  only for the empty command list used by the old fixture, so in production a
  `/goal <objective>` fell through to template expansion, produced an EMPTY
  prompt, and failed the model call; zero goal rows were ever written. The
  predicate is now "missing OR empty", a user command of the same name still
  wins, and the v2 command path matches the built-in by name before its
  template lookup (it previously bailed `command disappeared`). The v1
  response is now a real `SessionV1.WithParts`: the kickoff turn runs inside
  the request and its assistant message is returned, instead of a goal-state
  object masquerading as `info` with an empty `parts` array. Control words
  (`pause`/`resume`/`clear`) persist a short assistant notice and return the
  goal state under `goal`. The command payload's `model` is honored by the
  kickoff, and the post-kickoff review is fired in the background so the
  request does not wait on a second completion.
- **CI: the musl build broke on a `cargo-zigbuild` regression.** 0.24.1 ships a
  cmake toolchain whose ASM compiler points at the 0.24.0 wrapper path, which
  does not exist in a fresh cache, so BoringSSL's `enable_language(ASM)` failed.
  Pinned to 0.24.0 in both `ci.yml` and `release.yml` with the reason recorded;
  unpin only after a newer release builds BoringSSL for musl.
- **CI: rule 58 flagged its own test knobs on macOS.** The media corpus gate
  read `OCSERVE_MEDIA_DB`/`OCSERVE_MEDIA_PLANT` from `src/`, and the guard's
  allowlist branch was itself broken (it flagged any env read on the same line
  as a listed name). The gate now lives in `tests/media_corpus.rs` where a
  test-only reader belongs, so the allowlist entries were removed rather than
  widened.
- **Transient generation sent an empty prompt.** Every caller of the
  transient executor (`/api/experimental/generate`, `generate/text`,
  `generate/decisions` and the goal reviewer) set `skip_history`, and with no
  prelude the model received a user message with an empty string: the
  instruction was never sent. Fixed for both shapes (skip-history and a
  history run with `persist_user: false`), which also un-hid the reviewer.
- **Synthetic text parts lost their metadata.** A goal continuation is tagged
  `{source:"goal", goalID}` on the PART (`TextPart.metadata`, per the 1.19.5
  contract); persistence dropped it, so the stall detector and any timeline
  saw an ordinary user message.
- **Config-declared providers could not authenticate** ([issue #28]): the API
  key was read only from the auth layer, so a key in `opencode.json` reached
  the provider as a bare `Bearer `. Three sources are now honoured in upstream's
  order: `{env:VAR}`/`{file:path}` substitution on the raw config text, the
  provider's config `options.apiKey`, and the credential table. A config
  provider resolving to an empty key logs a warning naming it.
- `switchModel` (v2) decoded `model` as a string while both contracts and the
  2.x client send `{id, providerID, variant?}`; the reasoning variant was then
  persisted as `"default"` on every switch and never applied. The object form
  is decoded, the variant is stored, and `model.variants[variant]` now reaches
  the provider request.
- 65 handler-level `OCSERVE_V2` checks are gone; the generated v2-only table is
  the single authority. Five of those handlers were operations that exist in
  the v1 contract too, so they returned `SessionNotFoundError` on a server with
  the flag unset.
- The composite user-agent is sent to every provider, not only the Zen lane,
  and session headers follow upstream's provider-family split
  (`x-opencode-session` for opencode providers, `x-session-affinity` +
  `X-Session-Id` otherwise).
- A revert test created its session with the repository as the worktree, so
  `unrevert` ran the snapshot engine's `checkout-index` over the checkout and
  silently discarded uncommitted changes. It now pins a temp directory, and
  **guard 68** requires the query-parameter form that actually sets it.

[issue #28]: https://github.com/Reliary/ocserve/issues/28

### Fixed
- **Credential rename and delete were no-ops.** `PATCH /api/credential/{id}` and
  `DELETE /api/credential/{id}` returned 204 without touching the store, so a
  client renaming a credential saw success and no change. Both now edit the
  `credential` table with the reference's exact status and error bodies
  (missing `label` is the tagged `Payload` error; an unknown id is still a 204
  no-op, as the 2.0.28 server does). `POST /api/integration/{id}/connect/key`
  also answers the known/unknown split instead of a blanket 204.

### Changed
- The README documents `cargo install ocserve` as a first-class install path
  (it was still calling crates.io stale), names the build prerequisites, and
  lists every published target.
- Every `D-*` divergence now has one row in a single ledger (`PLAN.md` §18),
  and `scripts/check-docs.py` fails a token used in source but not declared
  there.
- The CLI description and module header listed a `bench` subcommand that no
  longer exists; they now name the real set.

## 0.6.10

### Fixed
- **The JSON fast path could emit output it could not parse.** `compact_splice`
  (the zero-parse passthrough used for every stored `info`/`inline` row)
  promised "accepted output is valid JSON" and seven inputs broke that promise
  while returning success: raw C0 bytes in a value or key, VT/FF accepted as
  whitespace (`char::is_ascii_whitespace` is Rust's set, not JSON's), illegal
  escapes copied through, a container closing in the wrong phase (`{"a":}`,
  `{"a"`), a nested trailing comma (`[1,]`), and serde_json's reserved
  `$serde_json::private::Number` key. Every case now refuses and falls back to
  the DOM path, so behaviour on real payloads is unchanged. Found by the
  nightly fuzz job, which had been running with a different serde_json feature
  set than production.
- The fuzz workspace now resolves `serde_json` exactly as the shipped server
  does (`preserve_order` plus the transitively enabled `arbitrary_precision`),
  and the corpus is seeded from `fuzz/seeds/` so regression shapes cannot fall
  out of rotation (**guard 59**, with planted controls).

### Changed
- Dependency bumps: sha2 0.11 and parking_lot 0.13 required source changes
  (digest output no longer implements `LowerHex`, so hashes are hex-encoded
  explicitly; `const_mutex` was removed because `Mutex::new` is const), plus
  hyper 1.12, serde_json 1.0.154, toml 1.1, libc 0.2.191, rolldown 1.2.15,
  base64 0.23, brotli 9 and similar 3.
- CI actions: setup-rust-toolchain v2.0.0 (its `CARGO_BUILD_WARNINGS=deny`
  default is relaxed only on the musl leg, where a benign BoringSSL linker-path
  warning is not a defect), download-artifact v8.0.2, scorecard-action v2.5.0,
  and codeql-action v4.39.1 for init, analyze and upload-sarif together.

## 0.6.9

### Fixed
- **`cargo install ocserve` was stuck on 0.3.1.** `ocserve-http` embedded both
  pinned web-UI packs (35.8 MB), and crates.io caps a single `.crate` at
  10 MiB, so no version after 0.3.1 could be published. The packs are now
  split at zstd frame boundaries into six `ocserve-ui-*` crates (v1 in two
  parts, v2 in four); the index stays in `ocserve-http` and the loader maps
  absolute frame offsets across the parts. Nothing is dropped and the binary
  is unchanged: the installed product is the same one the release tarball
  ships. Largest part is 69.6% of the cap.
- The publish workflow no longer keeps a hand-written crate list: it asks
  `scripts/check-publish-order.py --order` (derived from `Cargo.toml`), so a
  new crate cannot be forgotten or mis-ordered. It also fails closed now: only
  "already published" is tolerated, while any other failure stops the chain
  instead of leaving a crate unpublished and every dependent broken. New
  crates hit crates.io's 5-per-10-minutes limit, so the job waits and retries.
- `CRATE_PAYLOAD_LIMIT_BYTES` is back to the real 10 MiB. It had been set to
  64 MiB on the belief that the size increase was granted; the registry API
  reports `max_upload_size: null`, so the guard was validating an aspiration
  and publish would have failed at `ocserve-http` after uploading ten crates.
- The size-increase request document now reflects that it was sent (2026-10-20)
  rather than "draft, not sent", with current figures. The request is no longer
  on the critical path.

## 0.6.8

### Fixed
- **Free-tier models returned 429 for every request.** Two gates, both fixed.
  The transport gate now fingerprints the TLS stack: the same body and headers
  that opencode sends pass from Bun's BoringSSL and are rejected from rustls,
  OpenSSL and curl (verified with a 5-stack replay), so opencode.ai goes
  through a BoringSSL-backed lane while other providers keep rustls
  (`D-ZEN-TLS`). The request gate also tightened: a short system prompt or a
  small hand-built tool array is rejected, while the real production prompt
  (~10 KB base + env + skills) and the real advertised tool set pass. The
  nightly live trio now sends exactly what the server sends, so the gate
  cannot silently drift again.
- **A final chunk carrying both content and `finish_reason` lost the text.**
  opencode's free tier sends `{"finish_reason":"stop","delta":{"content":"…"}}`
  in one chunk; the parser returned Done and dropped the content, so the turn
  completed with `finish=stop` and an empty reply. Content and terminal events
  are now emitted in order from one payload.
- **A spent opencode free allowance failed silently.** Three consecutive
  attempts on 2026-10-25 died with the raw provider body (429
  `FreeUsageLimitError`) and only a log line; the client showed nothing
  actionable and the user retried blindly. It is now classified as terminal
  and surfaced with upstream's exact upsell ("Free usage exceeded, subscribe
  to Go") as a typed `APIError{statusCode:429,isRetryable:false}`, plus
  `ocserve_zen_freetier_total{result="usage_limit"}`. The keyless/free tier is
  not free at scale: the allowance is shared and finite.
- **Retired models were still listed.** The served model map included every
  catalog entry, so `opencode` showed 46 models, 37 of them `deprecated`
  (upstream shows 9). Freeze deletes `status:"deprecated"` and, unless
  `OPENCODE_ENABLE_EXPERIMENTAL_MODELS` is set, `status:"alpha"`
  (`provider.ts:1744-1745`); ocserve now does the same in `build_providers`
  and when picking the effective default, so the picker and the default agree.

### Added
- `ocserve import --session <id>` (repeatable) and `--force` are now reachable
  from the CLI, matching the documented behaviour: adopt named sessions into a
  data dir that already has history (the search trigger is kept, transactions
  stay short, so a running server is safe), refuse a session that is already
  present unless `--force`, and require `--force` to name at least one
  session so a destructive import is never implicit. Session task lists are
  adopted too.
- `ocserve import --bulk` gives a populated data dir the fast index path while
  the server is stopped. Before this, stopping the server changed nothing: the
  strategy was chosen from the target's contents (`existing == 0`), so an
  existing data dir always took the per-row trigger path. `--bulk` drops the
  trigger, streams the parts and rebuilds the index once; on the measured
  5,722-part session that was 26-32 s versus 40-47 s incremental (warm), and
  the warning states that a server must not be writing.
- A live import above 10,000 source parts is refused before writing anything
  unless `--allow-slow-live` or `--bulk`: per-row indexing was measured at
  ~140 parts/s on a small populated target, and a 21k-message / 80k-part
  adoption exceeded 15 minutes with no progress output.
- `--force` snapshots the target with `VACUUM INTO` before deleting anything,
  and applies in every mode (a bulk re-import used to bypass the gate and let
  `INSERT OR REPLACE` cascade the old session away).
- The bulk index rebuild and trigger recreation are one transaction, and the
  parity stamp records `count(part_search)`; the boot gate now compares the
  index to the rows, so a row committed during the old gap is repaired instead
  of being recorded as healthy.
- The external-commit watcher survives a backup restore (per-path state keyed
  by device+inode, reopen on replacement), runs on the blocking pool, and
  counts poll failures.
- The bounded prompt load is conservative whenever ANY compaction row exists:
  an unfinished imported summary reorders history under the full filter, so a
  suffix is only used when the session has no compaction rows or the boundary
  resolves exactly.
- Source parts are size-checked before the target is created, and source paths
  are URI-encoded (`#`/`?`/`%` no longer open a different file).
- Nullable upstream columns no longer abort the import. `session.path` is
  nullable in the opencode schema; reading it as a `String` failed the whole
  import with `Invalid column type Null` on a real row.
- `ocserve doctor --env` and `ocserve env-reference` expose every `OCSERVE_*`
  variable with its default and accepted values. `docs/ENVIRONMENT.md` is
  generated from that contract, and guard rule 58 fails the build if a runtime
  read is undocumented or the committed reference is stale.
- The server notices commits made by another process into its data dir
  (`PRAGMA data_version` on one long-lived reader, folded into the write epoch
  by the 15 s sampler), so an importer's rows cannot leave the list, page or
  search caches stale.

### Changed
- **Prompt history is read as a proven suffix instead of the whole session.** A
  56k-message session stores ~85 MB of parts while the 8 MB history budget keeps
  only the tail. `prompt_window` proposes a `seq` suffix mirroring
  `filter_compacted`'s stop condition; `load_prompt_history` accepts it only when
  the exact byte trim proves it, or the compaction boundary sits at its start.
  Every other shape falls back to the full load. Interleaved, warm-up discarded:
  deep session 4.6-5.2 s -> 1.45-1.59 s, compacted session 117 ms -> 22 ms, a
  dense mid-size session 2.89 s -> 2.56 s; emitted provider messages are
  byte-identical in every case. Kill switch
  `OCSERVE_PROMPT_HISTORY_UNBOUNDED=1`; metric
  `ocserve_prompt_history_load_total{mode}`; guard rule 57.
- **Prefix-cache changes are observable.**
  `ocserve_provider_prefix_changed_total{part="system"|"tools"}` counts
  per-session changes of the system prompt and tool-schema array between
  requests, so a silent KV-prefix invalidation is visible instead of merely
  expensive.
- The per-tool-call permission path no longer re-parses the 26 KB provider tool
  schema JSON to answer "is this a builtin" (`ocserve_tools::is_builtin`).

### Fixed (review pass)
- **The server hung under `memory.high`.** `main/memory.high=512M` throttled the
  single server process into direct reclaim it could never satisfy (no swap,
  almost all anon): on 2026-10-24 it sat at 601 MiB with 325,225 high events,
  four threads parked in `mem_cgroup_handle_over_high`, and served nothing for
  over an hour while the unit stayed `active`. `main` no longer gets
  `memory.high`; the parent `MemoryHigh=960M` still throttles, `max` is the
  backstop. Regression test pins the absence.
- `--version` now reports the build commit (`ocserve 0.6.8 (e1a54dd)`), and the
  install receipt records `COMMIT=`, so a local build is distinguishable from a
  released artifact.
- Per-agent `tools:{bash:false}` in config now becomes permission rules, as
  upstream normalizes it; it was silently ignored.
- The forwarder retries forever (`StartLimitIntervalSec=0`) so a boot with
  Tailscale down cannot leave it permanently failed.
- `install.sh` no longer re-enables a forwarder the operator deliberately
  disabled; it only starts an already-enabled one.
- Plugin-tool execution allow-list is intersected with the retained wire set,
  so a name dropped by a deny rule cannot execute.
- `check-matrix.sh` has an explicit docs-only opt-out; the default stays
  fail-closed.

### Fixed (first review pass)


### Fixed
- **Tailnet access died on every deploy.** The forwarder unit used
  `PartOf=ocserve.service`, which propagates stop but not start, and
  `install.sh` restarts ocserve on every install. It is now independent
  (`Wants`+`After`, `Restart=always`), install restarts it when present, and
  `doctor` prints `TAILNET IS DOWN` instead of a neutral line. Guard 55.
- **A policy-denied plugin tool was still executable.** The advertised set is
  also the execution allow-list, and the name was inserted before the policy
  check, so a plan-mode-denied tool could be called by name and was gated as a
  wildcard write rather than `edit`. Insert now happens only on the advertised
  path.
- **Plugin tools could duplicate an MCP tool name.** The reserved set now
  covers every advertised name (builtin + MCP), not just builtins; a duplicate
  is a provider 400 and the plugin branch wins dispatch over MCP.
- `check-matrix.sh` failed open when the workspace did not build (it printed a
  WARN and exited 0); it now fails, since CI runs it after a successful build.
- `check-v2-hooks.py` no longer passes on an empty surface, an empty or
  commented-out hook const, or a stale fixture on both sides.
- Guard 54 is semantic (reserved source + policy-before-insert) with a planted
  control for each half; guard 53c rejects a `panic!` stub.

## 0.6.7

### Fixed
- v2 plugins that register `experimental.ws.handshake`/`send`/`receive` no
  longer fail to load. Upstream declares those hooks, but the shim's accepted
  list omitted them, so registration threw `unknown session hook` and the
  plugin's whole `setup` failed (losing every hook we do dispatch). All 16
  declared hooks are now accepted; the six with a dispatch site still run, the
  rest stay named divergences.
- `ctx.app.version` reports the running server version instead of a hardcoded
  `0.3.1` left over from Phase 3.
- The v2 capability matrix said `session.prompt` was diverged (it has been
  dispatched since 0.6.0) and listed five read domains as throwing proxies (they
  are proxied). Corrected; rule 36 now also requires every extracted hook to be
  *registrable*, with a planted removal as the control.

## 0.6.6

Everything in 0.6.5 plus the install provenance, advisory and CI fixes that
landed after that tag. No schema, wire, or storage change.

### Fixed
- `install.sh` installs to the one path the unit executes
  (`~/.local/bin/ocserve`, with `.prev` and a SHA256 receipt) instead of
  pointing `ExecStart` at the build output.
- tokio 1.54.1 (RUSTSEC-2026-0386, `AsyncFd` I/O-safety unsoundness).
- `--render-to` is a pure render: no build, no install.

## 0.6.5

The 0.6.4 tag never produced a release: both Windows jobs failed on a missing
non-unix `Shutdown` stub. 0.6.5 is 0.6.4 plus that fix. Content otherwise
identical; no schema, wire, or storage change.

## 0.6.4

Fixes from independent SRE and security reviews of 0.6.3. No schema, wire, or
storage change; rollback is a binary swap.

### Fixed
- Graceful shutdown closes the event bus, so an open SSE client can no longer
  hold the server until systemd SIGKILLs it (`drain_budget.rs`).
- SIGTERM is registered before all startup work and the drain deadline starts
  at the signal (`OCSERVE_DRAIN_SECS`, default 20 s).
- Partial cgroup budgets report `degraded`, never `ok`.
- L1 recycle default 280 MB so it sits below the 320 MB plugin cap.
- `MemoryLow=256M` on the unit makes `main/memory.low` effective.
- Spawns without a leaf clear `OCSERVE_KIDS_CGROUP`.
- `install.sh --render-to`; guard 53b asserts rendered units in both modes.
- Restored the non-unix `Shutdown` stub: the 0.6.4 tag failed both Windows
  release jobs with `cannot find function install_shutdown`, because the host
  build and every Linux test exercised only the unix variant. Guard 53c now
  requires both cfgs and a planted removal proves it bites.

## 0.6.3

### Fixed
- SIGTERM is registered as soon as the server starts (before boot checks,
  partition, config load, MCP probe and plugin load), and the drain deadline
  is measured from the signal, not from startup: `OCSERVE_DRAIN_SECS` (default
  20 s, 0 = wait forever). The 0.6.2 fix only covered the bind-to-serve
  window; a stop during startup, or a drain with an open connection, still hit
  systemd's 90 s timeout and SIGKILL. (The first version of this fix wrapped
  the signal wait itself, which shut the server down 20 s after every start;
  `drain_budget.rs` pins the correct behaviour.)
- `MemoryLow=256M` on the unit so `main/memory.low` is actually effective:
  kernel cgroup-v2 limits protections by the ancestors' `memory.low`, and
  every ancestor was 0.
- A partially applied partition (missing leaf, failed cap write, non-delegable
  kids subtree) now reports `degraded` instead of `ok`; it still never fails
  boot.
- `doctor --budget` reports per-node `memory.events` and pressure, and
  distinguishes "installed but stopped" from "not installed".
- Graceful shutdown closes the event bus, ending SSE streams. A connected
  event-stream client used to hold `axum::serve` open, so every restart waited
  out the drain budget (and 90 s SIGKILL before the budget existed). Verified
  with a live SSE client: shutdown now completes in under 2 s.
- L1 sidecar recycle default lowered 450 → 280 MB: 450 sat above the plugin
  leaf's 320 MB cap, so the kernel always won and the graceful path was dead
  code. Ordered invariant pinned by tests.
- Spawns with no leaf now clear `OCSERVE_KIDS_CGROUP` instead of inheriting an
  outer one; `OCSERVE_KIDS_MEMORY_MAX_MB` is range-checked; `doctor --budget`
  rejects a `..` cgroup path; `install.sh --render-to` lets the guard assert
  the rendered unit in both modes (the old rule grepped source text).
- `soak-gate.sh` now rejects samples above the parent or kids memory max (a
  500 MB kids allocation used to pass); `soak.sh` no longer references an
  undefined `$BASE` for a metric that does not exist.

## 0.6.2

### Reliability
- SIGTERM is registered **before** the socket binds. Previously the signal
  stream was created when `axum::serve` first polled it, so a signal in the
  bind-to-serve window killed the process without draining (seen once as
  `ExitStatus(15)`). `shutdown_race.rs` pins it with a deterministic window.

### Resource containment
- **Per-class memory caps and CPU weights.** Every child leaf now has its own
  `memory.high`/`memory.max` (plugin 288/320M, mcp 160/192M, tools 448/640M,
  pty 64/96M) and the `cpu` controller is enabled: the server weighs 500
  against the child classes' 50-100, so a build uses idle cores and yields
  when the API needs one. Previously all leaves inherited `max`, one shared
  700M pot, and no CPU priority.
- The aggregate `kids/` throttles at 640M before its 700M cap; `main` has a
  512M soft ceiling and a 768M backstop; the unit carries `MemoryHigh=960M`
  under the existing `MemoryMax=1024M`.
- `ocserve doctor --budget` prints the resolved caps, per-leaf current/peak,
  `memory.events` and CPU throttling.
- `scripts/install.sh --no-resource-limits` installs the unit without memory
  caps and disables the child partition and sidecar recycle for operators who
  own memory policy.
- The service deliberately does **not** set build job counts or CPU quotas;
  heavy builds belong in a `systemd-run --user --scope`, documented in README
  and SRE section 5.6.



Security and reliability review of 0.6.x (two independent audits). Fixes:

### Critical
- **A dead reader left a live-but-deaf host.** One host line over 8 MiB stopped
  the response reader while the child stayed alive, so every later hook burned
  its full budget and failed until restart. The reader now poisons the
  generation and the owner replaces the host; requests on a dead generation
  fail immediately instead of waiting.
- **The hook budget did not cover the stdin write.** A host that stopped
  draining blocked every caller on the stdin mutex with no timer. The budget
  now covers the write and the wait.

### High
- **A recycled or crashed sidecar silently disabled every plugin.** The 0.6.0
  handle refactor removed `ensure_alive()` from the traffic path and
  `recycle_child()` also cleared the loaded-plugin map, so `registers()` said
  "no plugins" forever. Recovery now runs on the hot path, and a recycle keeps
  the load knowledge.
- **A plugin tool returning a non-object `metadata` aborted the process.**
  serde_json's `IndexMut` panics on non-objects and release builds abort. The
  value is coerced at both boundaries.
- **The boot `config` hook result was overwritten** by a following
  `reconcile()` that re-read the file, so a plugin that tightened permissions
  was reverted milliseconds later. Applied once, and the follow-up only runs
  when the apply failed.
- **Plugin tools bypassed the `plan` agent's edit ban.** The gate sees a tool
  name, never its effects. A plugin tool now needs explicit `allow`, or an
  agent with bash-level authority; `ctx.ask` throws instead of silently
  approving.
- **Plugin tool names could collide** with builtins or each other, which is a
  provider 400 for every request. Refused and counted.
- **The `config` hook was serialized** behind the old mutex at three more sites;
  all now use the handle path.

### Medium
- `plugin_origins` (an absolute home path) no longer reaches the served config.
- Plugin tool output is bounded in the host, before it crosses the pipe.
- Pending RPC entries are removed by a `Drop` guard, so an aborted turn cannot
  wedge the recycle gate.
- `compacting` now matches the 60 s budget (the substring check missed it).
- `soak-gate.sh` no longer accepts a failed probe (`000000` coerced to 0).
- New metrics: plugin tool calls/failures/duration, sidecar respawns,
  config-apply failures, event-delivery failures, sidecar up/down.
- Load failures are `ERROR` with a counter instead of a single boot warning.

## 0.6.1

### Fixed
- Plugin tools advertised an **invalid JSON Schema** when the plugin's Zod was
  v3 (resolved first) or when the schema was generated from the normalized
  entry, where the plugin's `node_modules` is not resolvable. Every request in
  such a session got a provider 400. The conversion now resolves `zod/v4`
  first, uses the plugin's real entry, mirrors upstream's normalizer, and
  never emits a raw Zod object.

## 0.6.0

Both the safety work and the feature work are in this release: hook
serialization and unbounded hooks were fixed first, then the capability
gaps. They were developed and tested together, so they ship as one rollback
unit.

### Safety
- Per-hook timeouts derived from measured maxima on a live install
  (`v2.session.context` 15.3 s, `tool.execute.after` 30.0 s): 60 s for
  transforms/context/compaction, 180 s for tool hooks, 300 s for plugin tools.
  `OCSERVE_HOOK_TIMEOUT_MS` overrides every hook budget; `0` restores upstream
  parity (unbounded).
  Named divergence `D-HOOK-TIMEOUT` (upstream has no hook timeout at all).
- **Hook dispatch no longer serializes sessions.** The sidecar is reached
  through a cloneable handle; the server mutex is released before awaiting, so
  two sessions' hooks interleave. The event pump uses the same path.
- `ocserve_plugin_hook_timeout_total`, `ocserve_plugin_internal_error_total`,
  per-hook duration maxima.
- Plugin-internal hook failures (a plugin catching its own exception and
  returning the input unchanged) are counted per plugin and logged.

### Added
- **v1 plugin tools.** `hooks.tool` registrations are advertised to the model.
  The JSON Schema is generated in the plugin's own runtime (`z.toJSONSchema`,
  falling back to the legacy plain-object shape), so there is no Rust-side Zod
  converter. Execution runs in the sidecar with the reference's
  `PluginToolContext`; output is bounded to 1 MiB and failures become error
  tool results. Kill switch: `OCSERVE_PLUGIN_TOOLS=0`.
- **v2 `session.prompt` hook** fires before admission and its returned prompt
  is what the inbox row carries, matching upstream `SessionPrompt.prepare`.
- **v2 read-only context domains** (`model`, `provider`, `agent`, `integration`,
  `reference`) are proxied to the routes ocserve already serves instead of
  throwing a false `D-V2-DOMAIN`.
- **`ocserve doctor --plugins`**: per-plugin dialect, registered hooks,
  bridged/unbridged transforms.
- Demand ledger (`bench/plugins/demand.json`) + guard 50: every hook the
  ecosystem declares is classified as bound or named-diverged.

### Changed
- Hooks nobody registered are skipped without a sidecar round-trip.
- The plugin `config` hook's returned object is applied through the same
  reconcile path as a hot reload. Previously it was called and discarded.

## 0.4.1
- v1 hook-contract capture (`scripts/capture-hooks.sh`), replay/order/effect
  gates, and the fixes they found: `messages.transform` payload, length-changing
  compaction output, `tool.execute.after` keys, `tool.definition`, `shell.env`,
  and the `ctx.client` `{path,query,body}` convention.

## 0.4.0
- v2 plugin dialects (promise + Effect), command/skill transforms, RPC dispatch.

## 0.3.1
- Repository/asset packaging fix; `cargo install` works again.

## 0.3.0
- v2 plugin surface, snapshot engine, revert semantics.

## 0.2.0
- Full CI suite, aarch64 release, Linux/macOS PTY.

## 0.1.0
- First public release.
